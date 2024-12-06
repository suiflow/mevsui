use std::sync::Arc;

use crate::authority::authority_per_epoch_store::AuthorityPerEpochStore;
use crate::authority::authority_store;
use crate::execution_cache::ObjectCacheRead;
use crate::transaction_input_loader::TransactionInputLoader;
use sui_types::base_types::ObjectID;
use sui_types::base_types::ObjectRef;
use sui_types::base_types::SequenceNumber;
use sui_types::base_types::VersionNumber;
use sui_types::committee::EpochId;
use sui_types::digests::TransactionDigest;
use sui_types::error::SuiError;
use sui_types::error::SuiResult;
use sui_types::error::UserInputError;
use sui_types::messages_checkpoint::CheckpointSequenceNumber;
use sui_types::object::Object;
use sui_types::storage::BackingPackageStore;
use sui_types::storage::BackingStore;
use sui_types::storage::ChildObjectResolver;
use sui_types::storage::ObjectKey;
use sui_types::storage::ObjectOrTombstone;
use sui_types::storage::ObjectStore;
use sui_types::storage::PackageObject;
use sui_types::storage::ParentSync;
use sui_types::sui_system_state::SuiSystemState;
use sui_types::transaction::InputObjectKind;
use sui_types::transaction::InputObjects;
use sui_types::transaction::ObjectReadResult;
use sui_types::transaction::ObjectReadResultKind;
use sui_types::transaction::ReceivingObjectReadResult;
use sui_types::transaction::ReceivingObjectReadResultKind;
use sui_types::transaction::ReceivingObjects;

pub struct InputLoaderCache<'a> {
    pub(crate) loader: &'a TransactionInputLoader,
    pub cache: Vec<(ObjectID, Object)>,
}

impl InputLoaderCache<'_> {
    pub fn read_objects_for_signing(
        &self,
        _tx_digest_for_caching: Option<&TransactionDigest>,
        input_object_kinds: &[InputObjectKind],
        receiving_objects: &[ObjectRef],
        epoch_id: EpochId,
    ) -> SuiResult<(InputObjects, ReceivingObjects)> {
        // Length of input_object_kinds have been checked via validity_check() for ProgrammableTransaction.
        let mut input_results = vec![None; input_object_kinds.len()];
        let mut object_refs = Vec::with_capacity(input_object_kinds.len());
        let mut fetch_indices = Vec::with_capacity(input_object_kinds.len());

        for (i, kind) in input_object_kinds.iter().enumerate() {
            match kind {
                // Packages are loaded one at a time via the cache
                InputObjectKind::MovePackage(id) => {
                    let Some(package) = self.get_package_object(id)?.map(|o| o.into()) else {
                        return Err(SuiError::from(kind.object_not_found_error()));
                    };
                    input_results[i] = Some(ObjectReadResult {
                        input_object_kind: *kind,
                        object: ObjectReadResultKind::Object(package),
                    });
                }
                InputObjectKind::SharedMoveObject { id, .. } => match self.get_object(id) {
                    Some(object) => {
                        input_results[i] = Some(ObjectReadResult::new(*kind, object.into()))
                    }
                    None => {
                        if let Some((version, digest)) =
                            self.get_last_shared_object_deletion_info(id, epoch_id)
                        {
                            input_results[i] = Some(ObjectReadResult {
                                input_object_kind: *kind,
                                object: ObjectReadResultKind::DeletedSharedObject(version, digest),
                            });
                        } else {
                            return Err(SuiError::from(kind.object_not_found_error()));
                        }
                    }
                },
                InputObjectKind::ImmOrOwnedMoveObject(objref) => {
                    object_refs.push(*objref);
                    fetch_indices.push(i);
                }
            }
        }

        let objects = self.multi_get_objects_with_more_accurate_error_return(&object_refs)?;
        assert_eq!(objects.len(), object_refs.len());
        for (index, object) in fetch_indices.into_iter().zip(objects.into_iter()) {
            input_results[index] = Some(ObjectReadResult {
                input_object_kind: input_object_kinds[index],
                object: ObjectReadResultKind::Object(object),
            });
        }

        let receiving_results =
            self.read_receiving_objects_for_signing(receiving_objects, epoch_id)?;

        Ok((
            input_results
                .into_iter()
                .map(Option::unwrap)
                .collect::<Vec<_>>()
                .into(),
            receiving_results,
        ))
    }

    fn read_receiving_objects_for_signing(
        &self,
        receiving_objects: &[ObjectRef],
        epoch_id: EpochId,
    ) -> SuiResult<ReceivingObjects> {
        let mut receiving_results = Vec::with_capacity(receiving_objects.len());
        for objref in receiving_objects {
            // Note: the digest is checked later in check_transaction_input
            let (object_id, version, _) = objref;

            if self.have_received_object_at_version(object_id, *version, epoch_id) {
                receiving_results.push(ReceivingObjectReadResult::new(
                    *objref,
                    ReceivingObjectReadResultKind::PreviouslyReceivedObject,
                ));
                continue;
            }

            let Some(object) = self.get_object(object_id) else {
                return Err(UserInputError::ObjectNotFound {
                    object_id: *object_id,
                    version: Some(*version),
                }
                .into());
            };

            receiving_results.push(ReceivingObjectReadResult::new(*objref, object.into()));
        }
        Ok(receiving_results.into())
    }
}

impl ObjectCacheRead for InputLoaderCache<'_> {
    fn get_package_object(&self, id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        // first query the cache
        for (cache_id, obj) in &self.cache {
            if obj.is_package() && cache_id == id {
                return Ok(Some(PackageObject::new(obj.clone())));
            }
        }

        // if not found in cache, query the loader
        self.loader.cache.get_package_object(id)
    }

    fn get_object(&self, id: &ObjectID) -> Option<Object> {
        // first query the cache
        for (cache_id, obj) in &self.cache {
            if cache_id == id {
                return Some(obj.clone());
            }
        }

        // if not found in cache, query the loader
        self.loader.cache.get_object(id)
    }

    fn force_reload_system_packages(&self, system_package_ids: &[ObjectID]) {
        self.loader
            .cache
            .force_reload_system_packages(system_package_ids);
    }

    fn get_latest_object_ref_or_tombstone(&self, object_id: ObjectID) -> Option<ObjectRef> {
        self.loader
            .cache
            .get_latest_object_ref_or_tombstone(object_id)
    }

    fn get_latest_object_or_tombstone(
        &self,
        object_id: ObjectID,
    ) -> Option<(ObjectKey, ObjectOrTombstone)> {
        self.loader.cache.get_latest_object_or_tombstone(object_id)
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: SequenceNumber) -> Option<Object> {
        // first query the cache
        for (cache_id, obj) in &self.cache {
            if cache_id == object_id && obj.version() == version {
                return Some(obj.clone());
            }
        }

        self.loader.cache.get_object_by_key(object_id, version)
    }

    fn multi_get_objects_by_key(&self, object_keys: &[ObjectKey]) -> Vec<Option<Object>> {
        // get one by one

        let mut results = Vec::with_capacity(object_keys.len());
        for object_key in object_keys {
            let object_id = &object_key.0;
            let version = object_key.1;
            let mut found = false;
            for (cache_id, obj) in &self.cache {
                if cache_id == object_id && obj.version() == version {
                    results.push(Some(obj.clone()));
                    found = true;
                    break;
                }
            }

            if !found {
                results.push(self.loader.cache.get_object_by_key(object_id, version));
            }
        }

        results
    }

    fn object_exists_by_key(&self, object_id: &ObjectID, version: SequenceNumber) -> bool {
        // first query the cache
        for (cache_id, obj) in &self.cache {
            if cache_id == object_id && obj.version() == version {
                return true;
            }
        }

        self.loader.cache.object_exists_by_key(object_id, version)
    }

    fn multi_object_exists_by_key(&self, object_keys: &[ObjectKey]) -> Vec<bool> {
        self.loader.cache.multi_object_exists_by_key(object_keys)
    }

    fn find_object_lt_or_eq_version(
        &self,
        object_id: ObjectID,
        version: SequenceNumber,
    ) -> Option<Object> {
        self.loader
            .cache
            .find_object_lt_or_eq_version(object_id, version)
    }

    fn get_lock(
        &self,
        obj_ref: ObjectRef,
        epoch_store: &AuthorityPerEpochStore,
    ) -> authority_store::SuiLockResult {
        self.loader.cache.get_lock(obj_ref, epoch_store)
    }

    fn _get_live_objref(&self, object_id: ObjectID) -> SuiResult<ObjectRef> {
        for (cache_id, obj) in &self.cache {
            if cache_id == &object_id {
                return Ok((object_id, obj.version(), obj.digest()));
            }
        }

        self.loader.cache._get_live_objref(object_id)
    }

    fn check_owned_objects_are_live(&self, owned_object_refs: &[ObjectRef]) -> SuiResult {
        self.loader
            .cache
            .check_owned_objects_are_live(owned_object_refs)
    }

    fn get_sui_system_state_object_unsafe(&self) -> SuiResult<SuiSystemState> {
        self.loader.cache.get_sui_system_state_object_unsafe()
    }

    fn get_bridge_object_unsafe(&self) -> SuiResult<sui_types::bridge::Bridge> {
        self.loader.cache.get_bridge_object_unsafe()
    }

    fn get_marker_value(
        &self,
        object_id: &ObjectID,
        version: SequenceNumber,
        epoch_id: EpochId,
    ) -> Option<sui_types::storage::MarkerValue> {
        self.loader
            .cache
            .get_marker_value(object_id, version, epoch_id)
    }

    fn get_latest_marker(
        &self,
        object_id: &ObjectID,
        epoch_id: EpochId,
    ) -> Option<(SequenceNumber, sui_types::storage::MarkerValue)> {
        self.loader.cache.get_latest_marker(object_id, epoch_id)
    }

    fn get_highest_pruned_checkpoint(&self) -> CheckpointSequenceNumber {
        self.loader.cache.get_highest_pruned_checkpoint()
    }
}

pub struct ObjectCache {
    pub inner: Arc<dyn BackingStore>,
    pub cache: Vec<(ObjectID, Object)>,
}

impl ObjectCache {
    pub fn new(inner: Arc<dyn BackingStore + Send + Sync>, cache: Vec<(ObjectID, Object)>) -> Self {
        Self { inner, cache }
    }
}

impl BackingPackageStore for ObjectCache {
    fn get_package_object(&self, package_id: &ObjectID) -> SuiResult<Option<PackageObject>> {
        for (id, obj) in &self.cache {
            if id == package_id {
                return Ok(Some(PackageObject::new(obj.clone())));
            }
        }

        self.inner.get_package_object(package_id)
    }
}

impl ChildObjectResolver for ObjectCache {
    fn read_child_object(
        &self,
        parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> SuiResult<Option<Object>> {
        self.inner
            .read_child_object(parent, child, child_version_upper_bound)
    }

    fn get_object_received_at_version(
        &self,
        owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        epoch_id: EpochId,
    ) -> SuiResult<Option<Object>> {
        self.inner.get_object_received_at_version(
            owner,
            receiving_object_id,
            receive_object_at_version,
            epoch_id,
        )
    }
}

impl ObjectStore for ObjectCache {
    fn get_object(&self, object_id: &ObjectID) -> Option<Object> {
        // iterate through the cache to find the object
        for (id, obj) in &self.cache {
            if id == object_id {
                return Some(obj.clone());
            }
        }
        self.inner.get_object(object_id)
    }

    fn get_object_by_key(&self, object_id: &ObjectID, version: VersionNumber) -> Option<Object> {
        for (id, obj) in &self.cache {
            if id == object_id && obj.version() == version {
                return Some(obj.clone());
            }
        }

        self.inner.get_object_by_key(object_id, version)
    }
}

impl ParentSync for ObjectCache {
    fn get_latest_parent_entry_ref_deprecated(&self, object_id: ObjectID) -> Option<ObjectRef> {
        // deprecated
        self.inner.get_latest_parent_entry_ref_deprecated(object_id)
    }
}
