//! Fork Sui mainnet at a historical checkpoint and dev-inspect a custom PTB in memory.
//!
//! Usage: sui-fork-sim <request.json> [output.json]

mod store;

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use move_core_types::{
    annotated_value as A,
    identifier::Identifier,
    language_storage::{StructTag, TypeTag},
    u256::U256,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{cell::RefCell, str::FromStr, sync::Arc};
use store::CheckpointStore;
use sui_data_store::{
    EpochStore, SetupStore, VersionQuery,
    node::Node,
    stores::{DataStore, FileSystemStore, ReadThroughStore},
};
use sui_types::{
    base_types::{ObjectID, SuiAddress, SystemObjectVersions},
    digests::TransactionDigest,
    execution_params::ExecutionOrEarlyError,
    gas::SuiGasStatus,
    layout_resolver::LayoutResolver,
    metrics::ExecutionMetrics,
    object::{MoveObject, OBJECT_START_VERSION, Object, Owner, bounded_visitor::BoundedVisitor},
    parse_sui_type_tag,
    transaction::{
        Argument, CallArg, CheckedInputObjects, Command, GasData, InputObjectKind, InputObjects,
        ObjectArg, ObjectReadResult, ProgrammableTransaction, SharedObjectMutability,
        TransactionKind,
    },
    SUI_CLOCK_OBJECT_ID, SUI_RANDOMNESS_STATE_OBJECT_ID,
};

const GQL_URL: &str = sui_data_store::node::MAINNET_GQL_URL;
// Same value the fullnode uses for its dev-inspect mock gas coin.
const MOCK_GAS_VALUE: u64 = 1_000_000_000_000_000_000;

#[derive(Deserialize)]
struct Request {
    checkpoint: u64,
    #[serde(default)]
    sender: Option<String>,
    inputs: Vec<Input>,
    commands: Vec<Cmd>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Input {
    /// On-chain object loaded at the checkpoint. `mutable` only matters for shared objects.
    Object { object: String, mutable: Option<bool> },
    /// Pure value, e.g. {"pure": "u64", "value": "1000"} or {"pure": "vector<address>", "value": [..]}.
    Pure { pure: String, value: Value },
    /// Mock `Coin<T>` owned by the sender, created in memory.
    Coin { coin: String, amount: Value },
    /// Raw BCS (base64) pure argument.
    Bcs { bcs: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Cmd {
    MoveCall {
        target: String,
        #[serde(default)]
        type_args: Vec<String>,
        #[serde(default)]
        args: Vec<Argument>,
    },
    SplitCoins { coin: Argument, amounts: Vec<Argument> },
    MergeCoins { coin: Argument, sources: Vec<Argument> },
    TransferObjects { objects: Vec<Argument>, address: Argument },
    MakeMoveVec {
        #[serde(rename = "type")]
        ty: Option<String>,
        elements: Vec<Argument>,
    },
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let req_path = args.get(1).context("usage: sui-fork-sim <request.json> [output.json]")?;
    let req: Request = serde_json::from_slice(&std::fs::read(req_path)?).context("invalid request JSON")?;

    let out = simulate(req)?;
    let text = serde_json::to_string_pretty(&out)?;
    match args.get(2) {
        Some(path) => std::fs::write(path, text)?,
        None => println!("{text}"),
    }
    Ok(())
}

fn simulate(req: Request) -> Result<Value> {
    // Disk cache (~/.sui_data_store) in front of GraphQL.
    let fs_store = FileSystemStore::new(Node::Mainnet)?;
    let cached_chain = fs_store.setup(None)?;
    let data_store = ReadThroughStore::new(fs_store, DataStore::new(Node::Mainnet, env!("CARGO_PKG_VERSION"))?);
    if cached_chain.is_none() {
        data_store.setup(None)?;
    }

    let (epoch, checkpoint_ts) = checkpoint_metadata(req.checkpoint)?;
    let epoch_data = data_store.epoch_info(epoch)?.with_context(|| format!("epoch {epoch} not found"))?;
    let protocol_config = data_store
        .protocol_config(epoch)?
        .context("protocol config not found")?;
    let rgp = epoch_data.rgp;

    let missing_path = checkpoint_dir()?.join(format!("{}.missing.json", req.checkpoint));
    let missing = std::fs::read(&missing_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let store = CheckpointStore {
        store: &data_store,
        checkpoint: req.checkpoint,
        cache: RefCell::default(),
        missing: RefCell::new(missing),
    };
    let sender = match &req.sender {
        Some(s) => parse_address(s).context("bad sender")?,
        None => SuiAddress::ZERO,
    };

    // Inputs
    let mut call_args = vec![];
    let mut reads = vec![];
    let mut mock_coins = vec![];
    for input in req.inputs {
        match input {
            Input::Object { object, mutable } => {
                let id = ObjectID::from_hex_literal(&object)?;
                let obj = store
                    .fetch(&id, VersionQuery::AtCheckpoint(req.checkpoint))?
                    .with_context(|| format!("object {id} not found at checkpoint {}", req.checkpoint))?;
                let kind = if let Some(initial_shared_version) = obj.owner.start_version() {
                    // Clock and Random can only be taken by immutable reference.
                    let default_mut = id != SUI_CLOCK_OBJECT_ID && id != SUI_RANDOMNESS_STATE_OBJECT_ID;
                    let mutability = if mutable.unwrap_or(default_mut) {
                        SharedObjectMutability::Mutable
                    } else {
                        SharedObjectMutability::Immutable
                    };
                    call_args.push(CallArg::Object(ObjectArg::SharedObject { id, initial_shared_version, mutability }));
                    InputObjectKind::SharedMoveObject { id, initial_shared_version, mutability }
                } else {
                    let oref = obj.compute_object_reference();
                    call_args.push(CallArg::Object(ObjectArg::ImmOrOwnedObject(oref)));
                    InputObjectKind::ImmOrOwnedMoveObject(oref)
                };
                reads.push(ObjectReadResult::new(kind, obj.into()));
            }
            Input::Pure { pure, value } => {
                let mut bytes = vec![];
                encode_pure(&parse_type(&pure)?, &value, &mut bytes)?;
                call_args.push(CallArg::Pure(bytes));
            }
            Input::Bcs { bcs } => call_args.push(CallArg::Pure(B64.decode(bcs)?)),
            Input::Coin { coin, amount } => {
                let amount: u64 = scalar(&amount).parse().context("bad coin amount")?;
                let obj = Object::new_move(
                    MoveObject::new_coin(parse_type(&coin)?, OBJECT_START_VERSION, ObjectID::random(), amount),
                    Owner::AddressOwner(sender),
                    TransactionDigest::genesis_marker(),
                );
                let oref = obj.compute_object_reference();
                call_args.push(CallArg::Object(ObjectArg::ImmOrOwnedObject(oref)));
                reads.push(ObjectReadResult::new(InputObjectKind::ImmOrOwnedMoveObject(oref), obj.clone().into()));
                mock_coins.push(oref.0);
            }
        }
    }

    // Commands
    let mut commands = vec![];
    for cmd in req.commands {
        commands.push(match cmd {
            Cmd::MoveCall { target, type_args, args } => {
                let mut parts = target.split("::");
                let (Some(pkg), Some(module), Some(function), None) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    bail!("bad move_call target `{target}`, expected 0xPKG::module::function");
                };
                let tys = type_args.iter().map(|t| parse_type(t)).collect::<Result<_>>()?;
                Command::move_call(
                    ObjectID::from_hex_literal(pkg)?,
                    Identifier::new(module)?,
                    Identifier::new(function)?,
                    tys,
                    args,
                )
            }
            Cmd::SplitCoins { coin, amounts } => Command::SplitCoins(coin, amounts),
            Cmd::MergeCoins { coin, sources } => Command::MergeCoins(coin, sources),
            Cmd::TransferObjects { objects, address } => Command::TransferObjects(objects, address),
            Cmd::MakeMoveVec { ty, elements } => Command::make_move_vec(ty.as_deref().map(parse_type).transpose()?, elements),
        });
    }
    let ptb = ProgrammableTransaction { inputs: call_args, commands };

    // Gas: mock coin + max budget, like the fullnode's dev-inspect.
    let budget = protocol_config.max_tx_gas();
    let gas_coin = Object::new_move(
        MoveObject::new_gas_coin(OBJECT_START_VERSION, ObjectID::MAX, MOCK_GAS_VALUE),
        Owner::AddressOwner(sender),
        TransactionDigest::genesis_marker(),
    );
    reads.push(ObjectReadResult::new_from_gas_object(&gas_coin));
    let gas_data = GasData { payment: vec![gas_coin.compute_object_reference()], owner: sender, price: rgp, budget };
    let gas_status = SuiGasStatus::new(budget, rgp, rgp, &protocol_config)?;

    let input_objects = CheckedInputObjects::new_for_replay(InputObjects::new(reads));
    let system_object_versions = SystemObjectVersions::from_inputs_or_latest_in_store(input_objects.inner(), &store);
    let executor = sui_execution::executor(&protocol_config, true)?;
    let metrics = Arc::new(ExecutionMetrics::new(&prometheus::Registry::new()));

    let (inner, _gas_status, effects, result) = executor.dev_inspect_transaction(
        &store,
        &protocol_config,
        metrics,
        false,
        ExecutionOrEarlyError::ok(None),
        &epoch,
        epoch_data.start_timestamp,
        input_objects,
        system_object_versions,
        gas_data,
        gas_status,
        TransactionKind::ProgrammableTransaction(ptb),
        None,
        sender,
        // Fixed digest: IDs created by the tx are derived from it, so a stable digest keeps the
        // VM's existence probes for those IDs cacheable (and runs reproducible).
        TransactionDigest::new([0xfe; 32]),
        true, // skip_all_checks, as dev-inspect does
    );

    std::fs::write(&missing_path, serde_json::to_vec(&*store.missing.borrow())?)?;

    // Output
    let mut resolver = executor.type_layout_resolver(&protocol_config, Box::new(&store));
    let mut decode = |bytes: &[u8], ty: &TypeTag| -> Value {
        let decoded = type_layout(&mut *resolver, ty)
            .and_then(|l| BoundedVisitor::deserialize_value(bytes, &l))
            .and_then(|v| Ok(serde_json::to_value(v)?));
        json!({
            "type": ty.to_canonical_string(true),
            "bcs": B64.encode(bytes),
            "json": decoded.unwrap_or_else(|e| json!({ "decode_error": e.to_string() })),
        })
    };

    let (error, results) = match result {
        Ok(results) => (Value::Null, results),
        Err(e) => (json!(format!("{e:?}")), vec![]),
    };
    let results: Vec<Value> = results
        .into_iter()
        .map(|(mut_refs, returns)| {
            json!({
                "return_values": returns.iter().map(|(b, t)| decode(b, t)).collect::<Vec<_>>(),
                "mutable_reference_outputs": mut_refs.iter().map(|(arg, b, t)| json!({ "argument": arg, "value": decode(b, t) })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let events: Vec<Value> = inner
        .events
        .data
        .iter()
        .map(|e| {
            json!({
                "package_id": e.package_id,
                "module": e.transaction_module.as_str(),
                "sender": e.sender,
                "event": decode(&e.contents, &TypeTag::Struct(Box::new(e.type_.clone()))),
            })
        })
        .collect();
    let mut dump = |o: &Object| -> Value {
        let mut v = json!({ "id": o.id(), "version": o.version().value(), "owner": o.owner });
        if let Some(m) = o.data.try_as_move() {
            let tag: StructTag = m.type_().clone().into();
            v["object"] = decode(m.contents(), &TypeTag::Struct(Box::new(tag)));
        } else {
            v["object"] = json!("package");
        }
        v
    };
    let loaded: Vec<Value> = store.cache.borrow().values().flat_map(|vs| vs.values()).map(&mut dump).collect();
    let written: Vec<Value> = inner.written.values().map(&mut dump).collect();

    Ok(json!({
        "checkpoint": req.checkpoint,
        "checkpoint_timestamp": checkpoint_ts,
        "epoch": epoch,
        "protocol_version": protocol_config.version.as_u64(),
        "reference_gas_price": rgp,
        "sender": sender,
        "mock_coins": mock_coins,
        "error": error,
        "effects": effects,
        "results": results,
        "events": events,
        "loaded_objects": loaded,
        "written_objects": written,
    }))
}

/// Epoch and timestamp of a checkpoint via GraphQL, cached on disk next to the data store.
fn checkpoint_metadata(checkpoint: u64) -> Result<(u64, Value)> {
    let path = checkpoint_dir()?.join(format!("{checkpoint}.json"));
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(serde_json::from_slice(&bytes)?);
    }
    let meta = fetch_checkpoint_metadata(checkpoint)?;
    std::fs::write(&path, serde_json::to_vec(&meta)?)?;
    Ok(meta)
}

fn checkpoint_dir() -> Result<std::path::PathBuf> {
    let dir = FileSystemStore::base_path()?.join("checkpoints");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn fetch_checkpoint_metadata(checkpoint: u64) -> Result<(u64, Value)> {
    let resp: Value = reqwest::blocking::Client::new()
        .post(GQL_URL)
        .json(&json!({
            "query": "query($n: UInt53!) { checkpoint(sequenceNumber: $n) { timestamp epoch { epochId } } }",
            "variables": { "n": checkpoint },
        }))
        .send()?
        .error_for_status()?
        .json()?;
    let cp = &resp["data"]["checkpoint"];
    let epoch = cp["epoch"]["epochId"]
        .as_u64()
        .with_context(|| format!("checkpoint {checkpoint} not found: {resp}"))?;
    Ok((epoch, cp["timestamp"].clone()))
}

fn parse_type(s: &str) -> Result<TypeTag> {
    parse_sui_type_tag(s).map_err(|e| anyhow!("bad type `{s}`: {e}"))
}

/// Accepts short forms like `0x6` as well as full 32-byte hex.
fn parse_address(s: &str) -> Result<SuiAddress> {
    Ok(ObjectID::from_hex_literal(s)?.into())
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// BCS-encode a JSON value as the given Move type.
fn encode_pure(ty: &TypeTag, v: &Value, out: &mut Vec<u8>) -> Result<()> {
    let s = scalar(v);
    match ty {
        TypeTag::Bool => out.push(v.as_bool().context("expected bool")? as u8),
        TypeTag::U8 => out.extend(bcs::to_bytes(&s.parse::<u8>()?)?),
        TypeTag::U16 => out.extend(bcs::to_bytes(&s.parse::<u16>()?)?),
        TypeTag::U32 => out.extend(bcs::to_bytes(&s.parse::<u32>()?)?),
        TypeTag::U64 => out.extend(bcs::to_bytes(&s.parse::<u64>()?)?),
        TypeTag::U128 => out.extend(bcs::to_bytes(&s.parse::<u128>()?)?),
        TypeTag::U256 => out.extend(bcs::to_bytes(&U256::from_str(&s)?)?),
        TypeTag::Address => out.extend(bcs::to_bytes(&parse_address(&s)?)?),
        TypeTag::Vector(inner) => {
            let items = v.as_array().context("expected array")?;
            let mut n = items.len();
            loop {
                let byte = (n & 0x7f) as u8;
                n >>= 7;
                out.push(if n == 0 { byte } else { byte | 0x80 });
                if n == 0 {
                    break;
                }
            }
            for item in items {
                encode_pure(inner, item, out)?;
            }
        }
        TypeTag::Struct(st) if matches!((st.module.as_str(), st.name.as_str()), ("string", "String") | ("ascii", "String")) => {
            out.extend(bcs::to_bytes(v.as_str().context("expected string")?)?)
        }
        _ => bail!("unsupported pure type {ty}; pass {{\"bcs\": \"<base64>\"}} instead"),
    }
    Ok(())
}

fn type_layout(resolver: &mut dyn LayoutResolver, ty: &TypeTag) -> Result<A::MoveTypeLayout> {
    Ok(match ty {
        TypeTag::Bool => A::MoveTypeLayout::Bool,
        TypeTag::U8 => A::MoveTypeLayout::U8,
        TypeTag::U16 => A::MoveTypeLayout::U16,
        TypeTag::U32 => A::MoveTypeLayout::U32,
        TypeTag::U64 => A::MoveTypeLayout::U64,
        TypeTag::U128 => A::MoveTypeLayout::U128,
        TypeTag::U256 => A::MoveTypeLayout::U256,
        TypeTag::Address => A::MoveTypeLayout::Address,
        TypeTag::Signer => A::MoveTypeLayout::Signer,
        TypeTag::Vector(inner) => A::MoveTypeLayout::Vector(Box::new(type_layout(resolver, inner)?)),
        TypeTag::Struct(st) => resolver.get_annotated_layout(st)?.into_layout(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_encoding_matches_bcs() {
        let enc = |ty: &str, v: Value| {
            let mut out = vec![];
            encode_pure(&parse_type(ty).unwrap(), &v, &mut out).unwrap();
            out
        };
        assert_eq!(enc("u64", json!("1000")), bcs::to_bytes(&1000u64).unwrap());
        assert_eq!(enc("u128", json!(7)), bcs::to_bytes(&7u128).unwrap());
        assert_eq!(enc("bool", json!(true)), vec![1]);
        let v: Vec<u64> = (0..200).collect();
        assert_eq!(enc("vector<u64>", json!(v)), bcs::to_bytes(&v).unwrap());
        assert_eq!(enc("0x1::string::String", json!("hi")), bcs::to_bytes("hi").unwrap());
    }
}
