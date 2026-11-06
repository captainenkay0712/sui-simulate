//! Execution-facing store pinned to a checkpoint. Adapted from `sui-replay-2/src/execution.rs`.
//! Every miss goes to the `sui_data_store` read-through store (disk cache, then GraphQL).

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};
use sui_data_store::{ObjectKey, ObjectStore, VersionQuery};
use sui_types::{
    base_types::{ObjectID, ObjectRef, SequenceNumber, VersionNumber},
    committee::EpochId,
    error::{SuiErrorKind, SuiResult},
    object::Object,
    storage::{BackingPackageStore, PackageObject, ParentSync, RuntimeObjectResolver},
};

pub struct CheckpointStore<'a> {
    pub store: &'a dyn ObjectStore,
    pub checkpoint: u64,
    /// Every object loaded during the run, i.e. the pre-execution state that was touched.
    pub cache: RefCell<BTreeMap<ObjectID, BTreeMap<u64, Object>>>,
    /// IDs known not to exist at `checkpoint` (e.g. dynamic-field existence probes);
    /// the disk store only caches hits, so these are persisted separately.
    pub missing: RefCell<BTreeSet<ObjectID>>,
}

impl CheckpointStore<'_> {
    pub fn fetch(&self, id: &ObjectID, query: VersionQuery) -> SuiResult<Option<Object>> {
        let at_checkpoint = query == VersionQuery::AtCheckpoint(self.checkpoint);
        if at_checkpoint && self.missing.borrow().contains(id) {
            return Ok(None);
        }
        let obj = self
            .store
            .get_objects(&[ObjectKey { object_id: *id, version_query: query }])
            .map_err(|e| SuiErrorKind::Storage(e.to_string()))?
            .into_iter()
            .next()
            .flatten()
            .map(|(obj, _)| obj);
        if obj.is_none() && at_checkpoint {
            self.missing.borrow_mut().insert(*id);
        }
        if let Some(obj) = &obj {
            self.cache.borrow_mut().entry(obj.id()).or_default().insert(obj.version().value(), obj.clone());
        }
        Ok(obj)
    }

    fn at_checkpoint(&self, id: &ObjectID) -> SuiResult<Option<Object>> {
        if let Some(obj) = self.cache.borrow().get(id).and_then(|v| v.last_key_value()) {
            return Ok(Some(obj.1.clone()));
        }
        self.fetch(id, VersionQuery::AtCheckpoint(self.checkpoint))
    }

    fn at_version(&self, id: &ObjectID, version: VersionNumber) -> Option<Object> {
        if let Some(obj) = self.cache.borrow().get(id).and_then(|v| v.get(&version.value())) {
            return Some(obj.clone());
        }
        self.fetch(id, VersionQuery::Version(version.value())).ok().flatten()
    }
}

impl BackingPackageStore for CheckpointStore<'_> {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        // AtCheckpoint gives the right version for upgradable system packages (0x1, 0x2, ...).
        Ok(self.at_checkpoint(package_id)?.map(PackageObject::new))
    }
}

impl sui_types::storage::ObjectStore for CheckpointStore<'_> {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        self.at_checkpoint(object_id).ok().flatten()
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: VersionNumber) -> Option<Object> {
        self.at_version(object_id, version)
    }
}

impl RuntimeObjectResolver for CheckpointStore<'_> {
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        // State as of the fork checkpoint; a later version would exceed the bound.
        let obj = self.at_checkpoint(child)?;
        Ok(obj.filter(|o| o.version() <= child_version_upper_bound))
    }

    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        Ok(self.at_version(receiving_object_id, receive_object_at_version))
    }
}

impl ParentSync for CheckpointStore<'_> {
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        unreachable!("unexpected ParentSync::get_latest_parent_entry_ref_deprecated({object_id})")
    }
}
