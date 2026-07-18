//! The ZFS pool backend drives ZFS datasets via the zfs(8) CLI, modeled on
//! how zfs-localpv does it (arg-vector exec only, no libzfs):
//!  - Pool: a mayastor pool named P created on the disks entry "tank/data"
//!    is the container filesystem dataset "tank/data/P". Commands used
//!       - zfs create -o io.mayastor:pool=<uuid> ... tank/data/P
//!       - zfs get/list -Hp ... -> discovery and detail queries
//!       - zfs inherit io.mayastor:pool -> export (drops ownership)
//!       - zfs destroy -r -> destroy
//!  - Replica: a ZFS volume (zvol) "tank/data/P/<replica-uuid>", exposed to
//!    SPDK through its /dev/zvol block device via an aio bdev. Commands used
//!       - zfs create [-s] -V <size> -b <volblocksize> -o ... -> create
//!       - zfs set volsize=<size> -> resize (expand only)
//!       - zfs set/inherit io.mayastor:* -> property updates
//!       - zfs destroy [-r] -> destroy
//!  - Snapshot/Clone: native ZFS snapshots and clones (which the LVM backend
//!    cannot support at all). Commands used
//!       - zfs snapshot -o io.mayastor:* ds@<snapshot-uuid> -> atomic create
//!       - zfs clone -o io.mayastor:* ds@snap tank/data/P/<clone-uuid>
//!       - zfs destroy [-d] ds@snap -> destroy (deferred when clones exist)
//!       - zfs promote <clone> -> re-home snapshots on replica destroy
//!
//! Ownership and metadata are persisted as ZFS user properties in the
//! "io.mayastor:" namespace. ZFS user properties are INHERITED by child
//! datasets and snapshots, therefore every ownership check requires the
//! property source to be "local".

/// Helps run the zfs commands and decode their -Hp tab-separated output.
mod cli;
/// ZFS pool management (the container filesystem dataset).
mod ds_pool;
mod error;
/// Pool and replica creation options parsing.
mod options;
mod property;
/// Native ZFS snapshot and clone management.
mod snapshot;
/// ZFS volume (zvol) replica management.
mod zvol_replica;

/// Errors encountered whilst interacting with the ZFS module.
pub(crate) use error::Error;

/// Query arguments used to lookup and filter ZFS resources.
pub(crate) use cli::ZfsQueryArgs;

/// A pool which is a ZFS container filesystem dataset.
pub use ds_pool::ZfsPool;

/// A replica which is a ZFS volume (zvol) and its query arguments.
pub(crate) use zvol_replica::{VolQueryArgs, ZfsVol};

/// A snapshot which is a native ZFS snapshot and its query arguments.
pub(crate) use snapshot::{SnapQueryArgs, ZfsSnapshot};

use crate::{
    bdev::PtplFileOps,
    core::{
        snapshot::SnapshotDescriptor, BdevStater, BdevStats, CloneParams, CoreError,
        NvmfShareProps, Protocol, PtplProps, SnapshotParams, UnshareProps, UntypedBdev,
        UpdateProps,
    },
    pool_backend::{
        FindPoolArgs, IPoolFactory, IPoolProps, ListPoolArgs, PoolArgs, PoolBackend,
        PoolMetadataInfo, PoolOps, ReplicaArgs,
    },
    replica_backend::{
        FindReplicaArgs, FindSnapshotArgs, IReplicaFactory, ListCloneArgs, ListReplicaArgs,
        ListSnapshotArgs, ReplicaBdevStats, ReplicaOps, SnapshotOps,
    },
};
use futures::channel::oneshot::Receiver;

/// Validate a single dataset path component, eg: a pool name or a replica
/// uuid, which becomes a child dataset name.
pub(super) fn is_valid_dataset_component(value: &str) -> Result<(), Error> {
    let mut chars = value.chars();
    let valid_first = chars
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false);
    if !valid_first
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
    {
        return Err(Error::InvalidName {
            name: value.to_string(),
            error: "must be [a-zA-Z0-9][a-zA-Z0-9_.:-]*".to_string(),
        });
    }
    Ok(())
}

/// Validate a full dataset path, eg: "tank/data".
pub(super) fn is_valid_dataset_path(value: &str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::InvalidName {
            name: value.to_string(),
            error: "must not be empty".to_string(),
        });
    }
    for component in value.split('/') {
        is_valid_dataset_component(component)?;
    }
    Ok(())
}

/// Submit the given future to the tokio runtime, collecting its result back
/// on the primary reactor. See the equivalent LVM helper (lvm/mod.rs).
pub(crate) fn tokio_submit<F, R>(future: F) -> Receiver<Result<R, Error>>
where
    F: std::future::Future<Output = Result<R, Error>> + Send + 'static,
    R: Send + std::fmt::Debug + 'static,
{
    let (s, r) = futures::channel::oneshot::channel();

    crate::core::runtime::spawn(async move {
        let result = future.await;

        if let Ok(r) = crate::core::Reactor::spawn_at_primary(async move {
            s.send(result).ok();
        }) {
            r.await.ok();
        }
    });
    r
}

#[macro_export]
macro_rules! zfs_tokio_run {
    ($fut:expr) => {{
        let r = $crate::zfs::tokio_submit($fut);
        r.await
            .map_err(|_| $crate::zfs::Error::ReactorSpawnChannel {})?
    }};
}

#[async_trait::async_trait(?Send)]
impl PoolOps for ZfsPool {
    async fn create_repl(
        &self,
        args: ReplicaArgs,
    ) -> Result<Box<dyn ReplicaOps>, crate::pool_backend::Error> {
        let replica = self.create_zvol(args).await?;
        Ok(Box::new(replica))
    }

    async fn destroy(self: Box<Self>) -> Result<(), crate::pool_backend::Error> {
        (*self).destroy().await?;
        Ok(())
    }

    async fn export(mut self: Box<Self>) -> Result<(), crate::pool_backend::Error> {
        ZfsPool::export(&mut self).await?;
        Ok(())
    }

    async fn grow(&self) -> Result<(), crate::pool_backend::Error> {
        Err(Error::GrowNotSup {}.into())
    }

    async fn reset_errors(&self) -> Result<(), crate::pool_backend::Error> {
        Err(Error::GrowNotSup {}.into())
    }
}

#[async_trait::async_trait(?Send)]
impl BdevStater for ZfsPool {
    type Stats = BdevStats;

    async fn stats(&self) -> Result<BdevStats, CoreError> {
        Err(CoreError::NotSupported {
            source: nix::errno::Errno::ENOSYS,
        })
    }

    async fn reset_stats(&self) -> Result<(), CoreError> {
        Err(CoreError::NotSupported {
            source: nix::errno::Errno::ENOSYS,
        })
    }
}

impl IPoolProps for ZfsPool {
    fn pool_type(&self) -> PoolBackend {
        PoolBackend::Zfs
    }

    fn name(&self) -> &str {
        self.name()
    }

    fn uuid(&self) -> String {
        self.uuid().to_string()
    }

    fn disks(&self) -> Vec<String> {
        vec![self.disks()]
    }

    fn disk_capacity(&self) -> u64 {
        self.zpool_size()
    }

    fn cluster_size(&self) -> u32 {
        self.volblocksize() as u32
    }

    fn page_size(&self) -> Option<u32> {
        None
    }

    fn capacity(&self) -> u64 {
        self.capacity()
    }

    fn used(&self) -> u64 {
        self.used()
    }

    fn committed(&self) -> u64 {
        self.committed()
    }

    fn md_props(&self) -> Option<PoolMetadataInfo> {
        None
    }

    fn encrypted(&self) -> bool {
        self.encrypted()
    }

    fn max_expandable_size(&self) -> Option<u64> {
        None
    }
}

#[async_trait::async_trait(?Send)]
impl ReplicaOps for ZfsVol {
    async fn share_nvmf(
        &mut self,
        props: NvmfShareProps,
    ) -> Result<String, crate::pool_backend::Error> {
        self.share_nvmf(Some(props)).await.map_err(Into::into)
    }
    async fn unshare(
        &mut self,
        opts: Option<UnshareProps>,
    ) -> Result<(), crate::pool_backend::Error> {
        self.unshare(opts).await.map_err(Into::into)
    }
    async fn update_properties(
        &mut self,
        props: UpdateProps,
    ) -> Result<(), crate::pool_backend::Error> {
        self.update_share_props(props).await?;
        Ok(())
    }

    async fn set_entity_id(&mut self, id: String) -> Result<(), crate::pool_backend::Error> {
        ZfsVol::set_entity_id(self, id).await?;
        Ok(())
    }

    async fn resize(&mut self, size: u64) -> Result<(), crate::pool_backend::Error> {
        self.resize(size).await.map_err(Into::into)
    }

    async fn destroy(self: Box<Self>) -> Result<(), crate::pool_backend::Error> {
        (*self).destroy().await.map_err(Into::into)
    }

    fn shared(&self) -> Option<Protocol> {
        self.share_proto()
    }

    fn create_ptpl(&self) -> Result<Option<PtplProps>, crate::pool_backend::Error> {
        self.ptpl()
            .create()
            .map_err(|source| crate::pool_backend::Error::Zfs {
                source: Error::BdevShare {
                    source: crate::core::CoreError::Ptpl {
                        reason: source.to_string(),
                    },
                },
            })
    }

    async fn create_snapshot(
        &mut self,
        params: SnapshotParams,
    ) -> Result<Box<dyn SnapshotOps>, crate::pool_backend::Error> {
        let snapshot = ZfsVol::create_snapshot(self, params).await?;
        Ok(Box::new(snapshot))
    }

    fn try_as_bdev(&self) -> Result<UntypedBdev, crate::pool_backend::Error> {
        let bdev = Self::bdev(self.bdev_opts()?.uri())?;
        Ok(bdev)
    }
}

#[async_trait::async_trait(?Send)]
impl BdevStater for ZfsVol {
    type Stats = ReplicaBdevStats;

    async fn stats(&self) -> Result<ReplicaBdevStats, CoreError> {
        Err(CoreError::NotSupported {
            source: nix::errno::Errno::ENOSYS,
        })
    }

    async fn reset_stats(&self) -> Result<(), CoreError> {
        Err(CoreError::NotSupported {
            source: nix::errno::Errno::ENOSYS,
        })
    }
}

#[async_trait::async_trait(?Send)]
impl SnapshotOps for ZfsSnapshot {
    async fn destroy_snapshot(self: Box<Self>) -> Result<(), crate::pool_backend::Error> {
        (*self).destroy().await?;
        Ok(())
    }

    async fn create_clone(
        &self,
        params: CloneParams,
    ) -> Result<Box<dyn ReplicaOps>, crate::pool_backend::Error> {
        let clone = ZfsSnapshot::create_clone(self, params).await?;
        Ok(Box::new(clone))
    }

    fn descriptor(&self) -> Option<SnapshotDescriptor> {
        Some(ZfsSnapshot::descriptor(self))
    }
    fn discarded(&self) -> bool {
        ZfsSnapshot::discarded(self)
    }
}

/// A factory instance which implements ZFS specific `PoolFactory`.
#[derive(Default)]
pub struct PoolZfsFactory {}
#[async_trait::async_trait(?Send)]
impl IPoolFactory for PoolZfsFactory {
    async fn create(&self, args: PoolArgs) -> Result<Box<dyn PoolOps>, crate::pool_backend::Error> {
        let pool = ZfsPool::create(args).await?;
        Ok(Box::new(pool))
    }

    async fn import(&self, args: PoolArgs) -> Result<Box<dyn PoolOps>, crate::pool_backend::Error> {
        let pool = ZfsPool::import(args).await?;
        Ok(Box::new(pool))
    }

    async fn find(
        &self,
        args: &FindPoolArgs,
    ) -> Result<Option<Box<dyn PoolOps>>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(None);
        }

        let query = match args {
            FindPoolArgs::Uuid(uuid) => ZfsQueryArgs::any().uuid(uuid),
            FindPoolArgs::UuidOrName(id) => {
                // First try to match by uuid, then fallback to the name.
                match ZfsPool::lookup(ZfsQueryArgs::any().uuid(id)).await {
                    Ok(pool) => return Ok(Some(Box::new(pool))),
                    Err(Error::NotFound { .. }) => ZfsQueryArgs::any().named(id),
                    Err(error) => return Err(error.into()),
                }
            }
            FindPoolArgs::NameUuid { name, uuid } => ZfsQueryArgs::any().named(name).uuid_opt(uuid),
        };
        match ZfsPool::lookup(query).await {
            Ok(pool) => Ok(Some(Box::new(pool))),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    async fn list(
        &self,
        args: &ListPoolArgs,
    ) -> Result<Vec<Box<dyn PoolOps>>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(vec![]);
        }
        if matches!(args.backend, Some(p) if p != PoolBackend::Zfs) {
            return Ok(vec![]);
        }

        let pools = ZfsPool::list(
            &ZfsQueryArgs::any()
                .named_opt(&args.name)
                .uuid_opt(&args.uuid),
        )
        .await?;

        Ok(pools
            .into_iter()
            .map(|p| Box::new(p) as _)
            .collect::<Vec<_>>())
    }

    fn backend(&self) -> PoolBackend {
        PoolBackend::Zfs
    }
}

/// A factory instance which implements ZFS specific `ReplicaFactory`.
#[derive(Default)]
pub struct ReplZfsFactory {}
#[async_trait::async_trait(?Send)]
impl IReplicaFactory for ReplZfsFactory {
    fn bdev_as_replica(&self, _bdev: crate::core::UntypedBdev) -> Option<Box<dyn ReplicaOps>> {
        None
    }
    async fn find(
        &self,
        args: &FindReplicaArgs,
    ) -> Result<Option<Box<dyn ReplicaOps>>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(None);
        }
        let lookup =
            ZfsVol::lookup(&VolQueryArgs::new().with_vol(ZfsQueryArgs::any().uuid(&args.uuid)))
                .await;
        match lookup {
            Ok(repl) => Ok(Some(Box::new(repl) as _)),
            Err(Error::VolNotFound { .. } | Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    async fn find_snap(
        &self,
        args: &FindSnapshotArgs,
    ) -> Result<Option<Box<dyn SnapshotOps>>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(None);
        }
        match ZfsSnapshot::lookup(&SnapQueryArgs::new().uuid(&args.uuid)).await {
            Ok(snap) => Ok(Some(Box::new(snap) as _)),
            Err(Error::SnapNotFound { .. } | Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn list(
        &self,
        args: &ListReplicaArgs,
    ) -> Result<Vec<Box<dyn ReplicaOps>>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(vec![]);
        }
        let replicas = ZfsVol::list(
            &VolQueryArgs::new()
                .with_vol(
                    ZfsQueryArgs::any()
                        .named_opt(&args.name)
                        .uuid_opt(&args.uuid),
                )
                .with_pool(
                    ZfsQueryArgs::any()
                        .named_opt(&args.pool_name)
                        .uuid_opt(&args.pool_uuid),
                ),
        )
        .await?;
        let replicas = replicas.into_iter().map(|r| Box::new(r) as _);
        Ok(replicas.collect::<Vec<_>>())
    }
    async fn list_snaps(
        &self,
        args: &ListSnapshotArgs,
    ) -> Result<Vec<SnapshotDescriptor>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(vec![]);
        }
        let snapshots = ZfsSnapshot::list(
            &SnapQueryArgs::new()
                .uuid_opt(&args.uuid)
                .source_opt(&args.source_uuid),
        )
        .await?;
        Ok(snapshots
            .iter()
            .map(ZfsSnapshot::descriptor)
            .collect::<Vec<_>>())
    }
    async fn list_clones(
        &self,
        args: &ListCloneArgs,
    ) -> Result<Vec<Box<dyn ReplicaOps>>, crate::pool_backend::Error> {
        if !crate::core::MayastorFeatures::get().zfs() {
            return Ok(vec![]);
        }
        let vols = ZfsVol::list(&VolQueryArgs::new()).await?;
        let clones = vols.into_iter().filter(|vol| match &args.snapshot_uuid {
            Some(uuid) => vol.snapshot_uuid_prop() == Some(uuid),
            None => vol.snapshot_uuid_prop().is_some(),
        });
        Ok(clones.map(|clone| Box::new(clone) as _).collect::<Vec<_>>())
    }

    fn backend(&self) -> PoolBackend {
        PoolBackend::Zfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_name_validation() {
        assert!(is_valid_dataset_component("pool-1").is_ok());
        assert!(is_valid_dataset_component("0af5efaa-8b09-4a4f-91d2-49a83f7f75e5").is_ok());
        assert!(is_valid_dataset_component("a_b.c:d").is_ok());
        assert!(is_valid_dataset_component("").is_err());
        assert!(is_valid_dataset_component("-leading").is_err());
        assert!(is_valid_dataset_component("has space").is_err());
        assert!(is_valid_dataset_component("has/slash").is_err());

        assert!(is_valid_dataset_path("tank").is_ok());
        assert!(is_valid_dataset_path("tank/data").is_ok());
        assert!(is_valid_dataset_path("tank/data/nested").is_ok());
        assert!(is_valid_dataset_path("").is_err());
        assert!(is_valid_dataset_path("/dev/sda").is_err());
        assert!(is_valid_dataset_path("tank//data").is_err());
        assert!(is_valid_dataset_path("aio:///dev/sda").is_err());
    }
}
