use super::{
    cli::{DsProps, ZfsCmd, ZfsQueryArgs},
    ds_pool::ZfsPool,
    error::Error,
    options::{round_up, ZvolCreateOpts},
    property::{protocol_from, Property, PropertyType},
    snapshot::{SnapQueryArgs, ZfsSnapshot},
};
use crate::{
    bdev::PtplFileOps,
    bdev_api::{bdev_create, BdevError},
    core::{NvmfShareProps, Protocol, PtplProps, Share, UnshareProps, UntypedBdev, UpdateProps},
    pool_backend::PoolBackend,
};

use std::{
    ops::{Deref, DerefMut},
    pin::Pin,
};

/// The properties fetched for the zvol replica bulk query.
const VOL_PROPS: &str = "volsize,volblocksize,used,referenced,refreservation,usedbysnapshots,\
origin,encryption,io.mayastor:pool,io.mayastor:uuid,io.mayastor:name,io.mayastor:entity_id,\
io.mayastor:share,io.mayastor:allowed_hosts,io.mayastor:snapshot_uuid";

/// Different list options for a zvol replica.
#[derive(Default, Debug)]
pub(crate) struct VolQueryArgs {
    /// Pertaining the pool container parent.
    pool: ZfsQueryArgs,
    /// Pertaining the zvol itself.
    vol: ZfsQueryArgs,
    /// Scope the query to the given container dataset, saving a system-wide
    /// walk when the pool is known.
    dataset: Option<String>,
}
impl VolQueryArgs {
    /// Builder-like creating a default `Self`.
    pub(crate) fn new() -> Self {
        Self::default()
    }
    /// Add the pool query args.
    pub(crate) fn with_pool(self, pool: ZfsQueryArgs) -> Self {
        Self { pool, ..self }
    }
    /// Add the zvol query args.
    pub(crate) fn with_vol(self, vol: ZfsQueryArgs) -> Self {
        Self { vol, ..self }
    }
    /// Scope the query to the given container dataset.
    pub(crate) fn scoped(self, dataset: &str) -> Self {
        Self {
            dataset: Some(dataset.to_string()),
            ..self
        }
    }
    /// Get a display string of the query, for error messages.
    pub(super) fn query(&self) -> String {
        format!("vol({}),pool({})", self.vol.query(), self.pool.query())
    }
}

/// A mayastor replica which is a ZFS volume (zvol), a child of the pool
/// container dataset, named after the replica uuid, and exposed to SPDK via
/// its /dev/zvol block device using an aio bdev.
/// Ownership is persisted as the local user property "io.mayastor:uuid";
/// inherited copies of the mayastor user properties do NOT count, see the
/// module level documentation.
#[derive(Debug, Clone)]
pub struct ZfsVol {
    /// The full zvol dataset path: <container>/<replica-uuid>.
    dataset: String,
    /// The pool container dataset path.
    #[allow(dead_code)]
    pool_dataset: String,
    /// The pool name (last path component of the container).
    pool_name: String,
    /// The pool uuid, inherited from the io.mayastor:pool user property.
    pool_uuid: String,
    /// The replica uuid, from the local io.mayastor:uuid user property.
    uuid: String,
    /// The replica name, from the local io.mayastor:name user property.
    name: Option<String>,
    /// The entity id which owns this resource, eg: the parent volume.
    entity_id: Option<String>,
    /// The source snapshot uuid, set on clones only.
    snapshot_uuid: Option<String>,
    /// The zvol volsize in bytes.
    size: u64,
    /// The zvol volblocksize in bytes (immutable after creation).
    volblocksize: u64,
    /// Total space charged to this zvol (its own data, its snapshots and any
    /// refreservation). Kept for diagnostics; space usage is reported from
    /// `referenced` and `usedbysnapshots`, see the `LogicalVolume` impl.
    #[allow(dead_code)]
    used: u64,
    /// Space referenced by the live zvol (its own data only, <= volsize).
    referenced: u64,
    /// Space allocated by the snapshots of this zvol.
    usedbysnapshots: u64,
    /// Thin provisioning: a sparse zvol has no refreservation.
    thin: bool,
    /// The origin snapshot dataset, set on clones only.
    origin: Option<String>,
    /// Whether the zvol is encrypted (usually inherited from the pool).
    encrypted: bool,
    /// The persisted share protocol of the replica.
    share: Protocol,
    /// The persisted nvmf allowed hosts of the replica.
    allowed_hosts: Vec<String>,
    /// SPDK Bdev runtime state.
    runtime: RunZfsVol,
}

/// Runtime settings for the ZfsVol.
#[derive(Debug, Default, Clone)]
pub struct RunZfsVol {
    /// SPDK Bdev parameters which are needed by the ZFS backend.
    bdev: Option<BdevOpts>,
}

/// SPDK Bdev parameters for the zvol's aio bdev.
#[derive(Debug, Default, Clone)]
pub(crate) struct BdevOpts {
    /// The share URI of the SPDK Bdev which is created against the zvol
    /// device path.
    share_uri: Option<String>,
    /// The URI of the SPDK Bdev which is created against the zvol device
    /// path.
    open_uri: String,
    allowed_hosts: Vec<String>,
    share: Protocol,
    size: u64,
}
impl From<UntypedBdev> for BdevOpts {
    fn from(bdev: UntypedBdev) -> Self {
        Self {
            share_uri: bdev.share_uri(),
            open_uri: bdev.bdev_uri_original_str().unwrap_or_default(),
            allowed_hosts: bdev.allowed_hosts(),
            share: bdev.shared().unwrap_or_default(),
            size: bdev.size_in_bytes(),
        }
    }
}
impl BdevOpts {
    fn update_from(&mut self, to: Self) {
        self.share_uri = to.share_uri;
        self.allowed_hosts = to.allowed_hosts;
        self.share = to.share;
    }
    /// Get a reference to the original bdev uri.
    pub(crate) fn uri(&self) -> &str {
        &self.open_uri
    }
}

impl Deref for ZfsVol {
    type Target = RunZfsVol;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}
impl DerefMut for ZfsVol {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

/// Wait for the udev-created /dev/zvol symlink to appear after a zfs
/// create/clone, polling every 100ms with a 10s timeout.
/// The polling runs on the tokio runtime, never on the reactor.
pub(super) async fn wait_for_zvol_device(path: String) -> Result<(), Error> {
    let in_spdk = spdk_rs::Thread::is_spdk_thread();
    let fut = async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::path::Path::new(&path).exists() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::DeviceWait { path });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    if in_spdk {
        crate::zfs_tokio_run!(fut)
    } else {
        fut.await
    }
}

impl ZfsVol {
    /// Create a new zvol replica on the given pool.
    /// The replica size is rounded up to a multiple of the volblocksize, and
    /// any allow-listed per-replica properties override the pool defaults.
    pub(crate) async fn create(
        pool: &ZfsPool,
        args: crate::pool_backend::ReplicaArgs,
    ) -> Result<ZfsVol, Error> {
        info!(
            name = args.name,
            uuid = args.uuid,
            pool = pool.name(),
            "Creating ZFS Volume"
        );

        super::is_valid_dataset_component(&args.uuid)?;
        let opts = ZvolCreateOpts::try_from_properties(&args.properties)?;
        let volblocksize = opts.volblocksize().unwrap_or_else(|| pool.volblocksize());
        super::options::validate_volblocksize(volblocksize)?;
        let size = round_up(args.size, volblocksize);
        let dataset = format!("{}/{}", pool.dataset(), args.uuid);

        // Note: wipe_super and use_extent_table are ignored, a fresh zvol
        // reads back zeroes.
        let mut cmd = ZfsCmd::zfs("create")
            .arg_if(args.thin, "-s")
            .args(["-V".to_string(), size.to_string()])
            .args(["-b".to_string(), volblocksize.to_string()]);
        for (key, value) in opts.props() {
            cmd = cmd.args(["-o".to_string(), format!("{key}={value}")]);
        }
        let entity_id = args.entity_id.clone().unwrap_or_default();
        let error = match cmd
            .prop(Property::VolUuid(args.uuid.to_string()))
            .prop(Property::Name(args.name.to_string()))
            .prop_if(!entity_id.is_empty(), Property::EntityId(entity_id))
            .prop(Property::Share(Protocol::Off))
            .arg(&dataset)
            .run()
            .await
        {
            Ok(()) => Ok(None),
            Err(error @ Error::Exists { .. }) => Ok(Some(error)),
            Err(error) => Err(error),
        }?;

        wait_for_zvol_device(format!("/dev/zvol/{dataset}")).await?;

        let vol = Self::lookup(
            &VolQueryArgs::new()
                .with_vol(ZfsQueryArgs::any().uuid(&args.uuid))
                .with_pool(ZfsQueryArgs::any().uuid(pool.uuid()))
                .scoped(pool.dataset()),
        )
        .await?;

        let Some(error) = error else {
            info!(
                name = args.name,
                uuid = args.uuid,
                pool = pool.name(),
                "ZFS Volume created successfully"
            );
            return Ok(vol);
        };

        // The zvol already existed: this is an idempotent re-create only if
        // all the creation parameters match.
        snafu::ensure!(vol.name().as_deref() == Some(&args.name), error);
        snafu::ensure!(vol.uuid() == args.uuid, error);
        snafu::ensure!(vol.size() == size, error);
        snafu::ensure!(vol.entity_id() == args.entity_id.as_ref(), error);
        snafu::ensure!(
            vol.share_proto().unwrap_or_default() == Protocol::Off,
            error
        );

        info!(
            name = args.name,
            uuid = args.uuid,
            pool = pool.name(),
            "ZFS Volume imported successfully"
        );

        Ok(vol)
    }

    /// Lookup a single zvol replica.
    pub(crate) async fn lookup(args: &VolQueryArgs) -> Result<Self, Error> {
        let vols = Self::list(args).await?;
        vols.into_iter().next().ok_or(Error::VolNotFound {
            query: args.query(),
        })
    }

    /// List zvol replicas using the provided options as query criteria.
    /// All zvols are imported as spdk bdevs and all imports must succeed.
    pub(crate) async fn list(opts: &VolQueryArgs) -> Result<Vec<ZfsVol>, Error> {
        let mut g_error = Ok(());
        let mut vols = Self::fetch(opts).await?;
        for vol in &mut vols {
            match vol.import().await {
                Ok(_) => {}
                Err(error) => {
                    tracing::error!("Failed to import {dataset}: {error}", dataset = vol.dataset,);
                    g_error = Err(error);
                }
            }
        }
        g_error?;
        Ok(vols)
    }

    /// Fetch zvol replicas using the provided options as query criteria.
    /// The query is a single bulk "zfs get" whose output is filtered
    /// client-side.
    async fn fetch(opts: &VolQueryArgs) -> Result<Vec<ZfsVol>, Error> {
        let mut cmd = ZfsCmd::zfs("get")
            .args(["-Hp", "-t", "volume", "-o", "name,property,value,source"])
            .arg(VOL_PROPS);
        if let Some(dataset) = &opts.dataset {
            cmd = cmd.args(["-d", "1"]).arg(dataset);
        }
        let all = cmd.prop_map().await?;
        Ok(all
            .iter()
            .filter_map(Self::from_props)
            .filter(|vol| vol.matches(opts))
            .collect())
    }

    /// Build a `ZfsVol` from its bulk query properties, yielding nothing if
    /// the zvol is not owned by us.
    fn from_props(props: &DsProps) -> Option<ZfsVol> {
        let dataset = props.name().to_string();
        let (pool_dataset, _) = dataset.rsplit_once('/')?;
        // Ours-check: the replica uuid property MUST be local, and the pool
        // uuid must be visible (inherited from an owned container).
        let uuid = props.local(PropertyType::VolUuid.value())?.to_string();
        let pool_uuid = props.value(PropertyType::PoolUuid.value())?.to_string();
        let pool_name = pool_dataset.rsplit('/').next().unwrap_or(pool_dataset);
        Some(ZfsVol {
            pool_dataset: pool_dataset.to_string(),
            pool_name: pool_name.to_string(),
            pool_uuid,
            uuid,
            name: props.local(PropertyType::Name.value()).map(String::from),
            entity_id: props
                .local(PropertyType::EntityId.value())
                .map(String::from),
            snapshot_uuid: props
                .local(PropertyType::SnapshotUuid.value())
                .map(String::from),
            size: props.u64("volsize")?,
            volblocksize: props
                .u64("volblocksize")
                .unwrap_or(super::options::DEFAULT_VOLBLOCKSIZE),
            used: props.u64("used").unwrap_or_default(),
            referenced: props.u64("referenced").unwrap_or_default(),
            usedbysnapshots: props.u64("usedbysnapshots").unwrap_or_default(),
            thin: props.u64("refreservation").unwrap_or_default() == 0,
            origin: props.value("origin").map(String::from),
            encrypted: props
                .value("encryption")
                .map(|e| e != "off")
                .unwrap_or(false),
            share: props
                .local(PropertyType::Share.value())
                .map(protocol_from)
                .unwrap_or_default(),
            allowed_hosts: props
                .local(PropertyType::AllowedHosts.value())
                .map(|hosts| {
                    hosts
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            runtime: RunZfsVol::default(),
            dataset,
        })
    }

    /// Check if the zvol matches the list options.
    fn matches(&self, opts: &VolQueryArgs) -> bool {
        let named = |name: Option<&String>, value: &str| name.map(|n| n == value).unwrap_or(true);
        named(
            opts.vol.name.as_ref(),
            self.name.as_deref().unwrap_or_default(),
        ) && named(opts.vol.uuid.as_ref(), &self.uuid)
            && named(opts.pool.name.as_ref(), &self.pool_name)
            && named(opts.pool.uuid.as_ref(), &self.pool_uuid)
    }

    /// Import the zvol.
    /// The zvol is imported as an spdk BDEV, which allows it to be shared
    /// via nvmf or open locally (ex: by the nexus).
    pub(crate) async fn import(&mut self) -> Result<(), Error> {
        self.import_bdev().await
    }

    /// Retrieve the block device path of the zvol.
    pub(crate) fn device_path(&self) -> String {
        format!("/dev/zvol/{}", self.dataset)
    }

    /// The aio bdev URI for the zvol device.
    /// The fallocate option is appended only when the device supports
    /// fallocate punch-hole, which makes trim/unmap passthrough work.
    fn disk_uri(&self) -> String {
        let path = self.device_path();
        let mut uri = format!("aio://{}?uuid={}", path, self.uuid);
        if crate::bdev::util::fallocate::supports_fallocate_punch_hole(&path) {
            uri.push_str("&fallocate=true");
        } else {
            info!(
                "ZFS volume '{}': trim passthrough is disabled as {path} does not \
                support fallocate punch-hole",
                self.uuid
            );
        }
        uri
    }

    /// Import the zvol by loading it via an SPDK bdev using AIO.
    async fn import_bdev(&mut self) -> Result<(), Error> {
        let disk_uri = self.disk_uri();
        let allowed_hosts = self.allowed_hosts.clone();
        let share = self.share;
        let ptpl = self.ptpl();

        let bdev = crate::spdk_run!(async move {
            if crate::core::UntypedBdev::lookup_by_name(&disk_uri).is_none() {
                bdev_create(&disk_uri)
                    .await
                    .map_err(|source| Error::BdevImport { source })?;
            }

            let mut bdev = Self::bdev(&disk_uri)?;
            Self::bdev_sync_props(&mut bdev, share, ptpl, allowed_hosts).await?;

            Ok(BdevOpts::from(bdev))
        })?;
        self.runtime.bdev = bdev.into();
        Ok(())
    }

    /// Export out the SPDK bdev.
    /// The bdev is unshared (if shared) and closed, allowing the zvol to be
    /// destroyed.
    pub(super) async fn export_bdev(&mut self) -> Result<(), Error> {
        let Ok(bdev) = self.bdev_opts() else {
            // Nothing to do if the bdev was not setup...
            return Ok(());
        };
        let uri = bdev.open_uri.clone();
        crate::spdk_run!(async move {
            if let Ok(mut bdev) = Self::bdev(&uri) {
                // todo: must we error if we can't unshare?
                Self::bdev_unshare(&mut bdev).await?;
            }

            let bdev = crate::bdev::uri::parse(&uri).unwrap();
            match bdev.destroy().await {
                Ok(()) | Err(BdevError::BdevNotFound { .. }) => Ok(()),
                Err(source) => Err(Error::BdevExport { source }),
            }
        })?;
        self.runtime.bdev = None;
        Ok(())
    }

    /// Destroy the zvol replica, along with its discarded snapshots.
    ///
    /// # Snapshot ordering
    /// 1. Snapshots with clones are first re-homed onto their first clone via
    ///    zfs promote (the snapshot identity is property-driven, so promotion
    ///    cannot orphan the metadata).
    /// 2. If any live (non-discarded) snapshot remains the destroy is refused.
    /// 3. Otherwise the zvol is destroyed, recursively when discarded
    ///    snapshots remain.
    pub(crate) async fn destroy(mut self) -> Result<(), Error> {
        let mut snapshots = self.list_snapshots().await?;

        for _ in 0..=snapshots.len() {
            let Some(snapshot) = snapshots.iter().find(|s| !s.clones().is_empty()) else {
                break;
            };
            let clone = snapshot.clones().first().cloned().unwrap_or_default();
            ZfsCmd::zfs("promote").arg(&clone).run().await?;
            snapshots = self.list_snapshots().await?;
        }
        if let Some(live) = snapshots.iter().find(|s| !s.discarded()) {
            warn!(
                "ZFS volume '{}' has a live snapshot '{}', refusing to destroy",
                self.uuid,
                live.uuid()
            );
            return Err(Error::HasLiveSnapshots {
                volume: self.uuid.clone(),
            });
        }

        self.export_bdev().await?;
        let ptpl = self.ptpl();

        ZfsCmd::zfs("destroy")
            .arg_if(!snapshots.is_empty(), "-r")
            .arg(&self.dataset)
            .run()
            .await?;
        ptpl.destroy().ok();

        info!("ZFS volume '{}' deleted", self.dataset);
        Ok(())
    }

    /// List the snapshots of this zvol.
    pub(crate) async fn list_snapshots(&self) -> Result<Vec<ZfsSnapshot>, Error> {
        ZfsSnapshot::list(&SnapQueryArgs::new().scoped(&self.dataset)).await
    }

    /// Create a snapshot of this zvol, with the snapshot parameters persisted
    /// atomically as user properties of the snapshot itself.
    pub(crate) async fn create_snapshot(
        &self,
        params: crate::core::SnapshotParams,
    ) -> Result<ZfsSnapshot, Error> {
        ZfsSnapshot::create(self, params).await
    }

    /// Resize the zvol AND the SPDK Bdev to the given size (expand only).
    /// > Note: If the SPDK Bdev fails to be resized, then the volsize is
    /// > reverted back.
    pub(crate) async fn resize(&mut self, size: u64) -> Result<(), Error> {
        let size = round_up(size, self.volblocksize);
        if size < self.size {
            return Err(Error::ShrinkNotSup {
                volume: self.uuid.clone(),
            });
        }
        let prev_size = self.size;
        self.set_volsize(size).await?;
        self.size = size;

        if let Err(error) = self.resize_bdev(size).await {
            self.set_volsize(prev_size).await.ok();
            self.size = prev_size;
            return Err(error);
        }

        Ok(())
    }

    /// Set only the zvol volsize to the given size (not the SPDK Bdev).
    async fn set_volsize(&self, size: u64) -> Result<(), Error> {
        ZfsCmd::zfs("set")
            .arg(format!("volsize={size}"))
            .arg(&self.dataset)
            .run()
            .await
    }

    /// Resize the zvol's SPDK Bdev to the given size.
    async fn resize_bdev(&mut self, size: u64) -> Result<(), Error> {
        let Ok((bdev, uri)) = self.bdev_mut_uri() else {
            // If the Bdev is not open, we don't need to resize it!?
            return Ok(());
        };
        let (rc, size) = crate::spdk_run!(async move {
            let mut bdev = Self::bdev(&uri)?;

            let blk_cnt = size / bdev.block_len() as u64;

            use spdk_rs::libspdk::spdk_bdev_notify_blockcnt_change;
            let rc =
                unsafe { spdk_bdev_notify_blockcnt_change(bdev.unsafe_inner_mut_ptr(), blk_cnt) };
            Ok((rc, bdev.size_in_bytes()))
        })?;
        if rc != 0 {
            error!("failed to notify block cnt change on zvol: {rc}");
            return Err(Error::BdevShareUri {});
        }
        bdev.size = size;
        Ok(())
    }

    /// Persist the given properties on the zvol as ZFS user properties, in a
    /// single zfs set invocation.
    async fn set_properties(&self, properties: Vec<Property>) -> Result<(), Error> {
        if properties.is_empty() {
            return Ok(());
        }
        let mut cmd = ZfsCmd::zfs("set");
        for property in properties {
            cmd = cmd.arg(property.set_arg());
        }
        cmd.arg(&self.dataset).run().await
    }
    /// Persist the given property on the zvol as a ZFS user property.
    pub(crate) async fn set_property(&self, property: Property) -> Result<(), Error> {
        self.set_properties(vec![property]).await
    }
    /// Set the entity id of the resource which owns this zvol.
    pub(crate) async fn set_entity_id(&mut self, id: String) -> Result<(), Error> {
        self.set_property(Property::EntityId(id.clone())).await?;
        self.entity_id = Some(id);
        Ok(())
    }

    /// Persist the current bdev share protocol and allowed hosts.
    async fn sync_share_opts(&mut self) -> Result<(), Error> {
        let Some(opts) = &self.runtime.bdev else {
            return Ok(());
        };
        let share = opts.share;
        let allowed_hosts = opts.allowed_hosts.clone();
        self.set_properties(vec![
            Property::Share(share),
            Property::AllowedHosts(allowed_hosts.clone()),
        ])
        .await?;
        self.share = share;
        self.allowed_hosts = allowed_hosts;
        Ok(())
    }
    /// Persist the given share protocol.
    async fn sync_share_protocol(&mut self, protocol: Protocol) -> Result<(), Error> {
        self.set_property(Property::Share(protocol)).await?;
        self.share = protocol;
        Ok(())
    }

    async fn bdev_sync_props(
        bdev: &mut UntypedBdev,
        protocol: Protocol,
        ptpl: impl PtplFileOps,
        allowed_hosts: Vec<String>,
    ) -> Result<(), Error> {
        match protocol {
            Protocol::Nvmf => {
                let props = NvmfShareProps::new()
                    .with_allowed_hosts(allowed_hosts)
                    .with_ptpl(ptpl.create().map_err(|source| Error::BdevShare {
                        source: crate::core::CoreError::Ptpl {
                            reason: source.to_string(),
                        },
                    })?);
                Self::bdev_share_nvmf(bdev, Some(props)).await?;
            }
            Protocol::Off => {
                Self::bdev_unshare(bdev).await?;
            }
        }

        Ok(())
    }

    fn bdev_mut(&mut self) -> Result<&mut BdevOpts, Error> {
        let Some(bdev) = self.runtime.bdev.as_mut() else {
            // Nothing to do if the bdev was not setup...
            return Err(Error::BdevMissing {});
        };
        Ok(bdev)
    }
    fn bdev_mut_uri(&mut self) -> Result<(&mut BdevOpts, String), Error> {
        let bdev = self.bdev_mut()?;
        let uri = bdev.open_uri.clone();
        Ok((bdev, uri))
    }
    pub(crate) fn bdev_opts(&self) -> Result<&BdevOpts, Error> {
        let Some(bdev) = self.runtime.bdev.as_ref() else {
            // Nothing to do if the bdev was not setup...
            return Err(Error::BdevMissing {});
        };
        Ok(bdev)
    }

    /// Get the full zvol dataset path.
    pub(crate) fn dataset(&self) -> &str {
        &self.dataset
    }
    /// Get the pool container dataset path.
    #[allow(unused)]
    pub(crate) fn pool_dataset(&self) -> &str {
        &self.pool_dataset
    }
    /// Get the name of the pool where this zvol resides.
    pub(crate) fn pool_name(&self) -> &str {
        &self.pool_name
    }
    /// Get the uuid of the pool where this zvol resides.
    pub(crate) fn pool_uuid(&self) -> &str {
        &self.pool_uuid
    }
    /// The size of the zvol (volsize).
    pub(crate) fn size(&self) -> u64 {
        self.size
    }
    /// The volblocksize of the zvol.
    #[allow(unused)]
    pub(crate) fn volblocksize(&self) -> u64 {
        self.volblocksize
    }
    /// Check if the zvol is thin provisioned (sparse).
    pub(crate) fn thin(&self) -> bool {
        self.thin
    }
    /// Get the replica uuid.
    pub(crate) fn uuid(&self) -> &str {
        &self.uuid
    }
    /// Get the replica name.
    pub(crate) fn name(&self) -> &Option<String> {
        &self.name
    }
    /// Get the entity id of the resource which owns this zvol.
    pub(crate) fn entity_id(&self) -> Option<&String> {
        self.entity_id.as_ref()
    }
    /// Get the source snapshot uuid, set on clones only.
    pub(crate) fn snapshot_uuid_prop(&self) -> Option<&String> {
        self.snapshot_uuid.as_ref()
    }
    /// Get the URI of the SPDK bdev which is layered on top of the zvol.
    pub(crate) fn uri(&self) -> Option<&String> {
        self.runtime.bdev.as_ref()?.share_uri.as_ref()
    }
    /// Get the SPDK bdev allowed hosts.
    pub(crate) fn bdev_allowed_hosts(&self) -> Option<&Vec<String>> {
        self.runtime.bdev.as_ref().map(|b| &b.allowed_hosts)
    }
}

/// These should be part of the Share trait but there are a few things that make
/// it difficult, see the equivalent LVM comment (lvm/lv_replica.rs).
impl ZfsVol {
    pub(crate) fn bdev(uri: &str) -> Result<UntypedBdev, Error> {
        UntypedBdev::get_by_name(uri).map_err(|_| Error::BdevMissing {})
    }

    async fn bdev_share_nvmf(
        bdev: &mut UntypedBdev,
        props: Option<NvmfShareProps>,
    ) -> Result<String, Error> {
        let mut bdev = Pin::new(bdev);
        match bdev.shared() {
            Some(Protocol::Nvmf) => {
                bdev.as_mut()
                    .update_properties(props.map(Into::into))
                    .await
                    .map_err(|source| Error::BdevShare { source })?;
                bdev.share_uri().ok_or(Error::BdevShareUri {})
            }
            Some(Protocol::Off) | None => bdev
                .share_nvmf(props)
                .await
                .map_err(|source| Error::BdevShare { source }),
        }
    }
    async fn bdev_unshare(bdev: &mut UntypedBdev) -> Result<Option<String>, Error> {
        let mut bdev = Pin::new(bdev);
        match bdev.shared() {
            Some(Protocol::Nvmf) => {
                bdev.as_mut()
                    .unshare(None)
                    .await
                    .map_err(|source| Error::BdevUnshare { source })?;
            }
            Some(Protocol::Off) | None => {}
        }
        Ok(bdev.share_uri())
    }

    /// Share the zvol via nvmf.
    pub(crate) async fn share_nvmf(
        &mut self,
        props: Option<NvmfShareProps>,
    ) -> Result<String, Error> {
        let (bdev, uri) = self.bdev_mut_uri()?;

        let (nqn, bdev_opts) = crate::spdk_run!(async move {
            let mut bdev = Self::bdev(&uri)?;
            let nqn = Self::bdev_share_nvmf(&mut bdev, props).await?;
            Ok((nqn, BdevOpts::from(bdev)))
        })?;

        bdev.update_from(bdev_opts);
        self.sync_share_opts().await?;

        info!("{:?}: shared as NVMF", self);
        Ok(nqn)
    }

    /// Update the zvol share properties.
    pub(crate) async fn update_share_props<P: Into<Option<UpdateProps>>>(
        &mut self,
        props: P,
    ) -> Result<(), Error> {
        let (bdev, uri) = self.bdev_mut_uri()?;
        let props = props.into();
        let bdev_opts = crate::spdk_run!(async move {
            let mut bdev = Self::bdev(&uri)?;
            Pin::new(&mut bdev)
                .update_properties(props)
                .await
                .map_err(|e| Error::UpdateProps {
                    source: e,
                    name: bdev.name().to_string(),
                })?;
            Ok(BdevOpts::from(bdev))
        })?;
        bdev.update_from(bdev_opts);
        self.sync_share_opts().await?;
        Ok(())
    }

    /// Unshare the nvmf target.
    pub(crate) async fn unshare(&mut self, opts: Option<UnshareProps>) -> Result<(), Error> {
        let (bdev, uri) = self.bdev_mut_uri()?;
        let share = crate::spdk_run!(async move {
            let mut bdev = Self::bdev(&uri)?;
            Self::bdev_unshare(&mut bdev).await
        })?;

        bdev.share_uri = share;
        bdev.share = Protocol::Off;

        if opts.unwrap_or_default().persist {
            self.sync_share_protocol(Protocol::Off).await?;
        }

        info!("{self:?}: unshared");
        Ok(())
    }

    /// Get the shared protocol, if any setup.
    pub(crate) fn share_proto(&self) -> Option<Protocol> {
        self.runtime.bdev.as_ref().map(|_| self.share)
    }

    /// Get a `PtplFileOps` from `&self`.
    pub(crate) fn ptpl(&self) -> impl PtplFileOps {
        ZvolPtpl::from(self)
    }
}

/// Persist through power loss implementation for a zvol (replica).
pub struct ZvolPtpl {
    pool: super::ds_pool::ZfsPoolPtpl,
    uuid: String,
}
impl ZvolPtpl {
    fn pool(&self) -> &super::ds_pool::ZfsPoolPtpl {
        &self.pool
    }
    fn uuid(&self) -> &str {
        &self.uuid
    }
}
impl From<&ZfsVol> for ZvolPtpl {
    fn from(vol: &ZfsVol) -> Self {
        Self {
            pool: ZfsPool::pool_ptpl(vol.pool_name()),
            uuid: vol.uuid().to_string(),
        }
    }
}

impl PtplFileOps for ZvolPtpl {
    fn create(&self) -> Result<Option<PtplProps>, std::io::Error> {
        if let Some(path) = self.path() {
            self.pool().create()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return Ok(Some(PtplProps::new(path)));
        }
        Ok(None)
    }

    fn destroy(&self) -> Result<(), std::io::Error> {
        if let Some(path) = self.path() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    fn subpath(&self) -> std::path::PathBuf {
        self.pool()
            .subpath()
            .join("vol/")
            .join(self.uuid())
            .with_extension("json")
    }
}

impl crate::core::LogicalVolume for ZfsVol {
    fn name(&self) -> String {
        self.name().clone().unwrap_or_default()
    }

    fn uuid(&self) -> String {
        self.uuid.clone()
    }

    fn pool_name(&self) -> String {
        self.pool_name().into()
    }

    fn pool_uuid(&self) -> String {
        self.pool_uuid().into()
    }

    fn entity_id(&self) -> Option<String> {
        self.entity_id().cloned()
    }

    fn is_thin(&self) -> bool {
        self.thin()
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn committed(&self) -> u64 {
        self.size
    }

    fn allocated(&self) -> u64 {
        // `referenced` (not `used`) is the space occupied by the live zvol's
        // own data: it excludes the refreservation of a thick zvol (so it
        // stays <= volsize) and the snapshot-exclusive space that is reported
        // separately as `allocated_bytes_snapshots`. Using `used` here would
        // let allocated exceed capacity and double-count snapshot space.
        self.referenced
    }

    fn usage(&self) -> crate::core::logical_volume::LvolSpaceUsage {
        let cluster_size = self.volblocksize;
        crate::core::logical_volume::LvolSpaceUsage {
            capacity_bytes: self.size,
            allocated_bytes: self.referenced,
            cluster_size,
            num_clusters: self.size / cluster_size,
            num_allocated_clusters: self.referenced.div_ceil(cluster_size),
            allocated_bytes_snapshots: self.usedbysnapshots,
            num_allocated_clusters_snapshots: self.usedbysnapshots.div_ceil(cluster_size),
            allocated_bytes_snapshot_from_clone: None,
        }
    }

    fn is_snapshot(&self) -> bool {
        false
    }

    fn is_clone(&self) -> bool {
        self.snapshot_uuid.is_some() || self.origin.is_some()
    }

    fn backend(&self) -> PoolBackend {
        PoolBackend::Zfs
    }

    fn snapshot_uuid(&self) -> Option<String> {
        self.snapshot_uuid.clone()
    }

    fn share_protocol(&self) -> Protocol {
        self.share
    }

    fn bdev_share_uri(&self) -> Option<String> {
        self.uri().cloned()
    }

    fn nvmf_allowed_hosts(&self) -> Vec<String> {
        self.bdev_allowed_hosts().cloned().unwrap_or_default()
    }

    fn encrypted(&self) -> bool {
        self.encrypted
    }
}

#[async_trait::async_trait(?Send)]
impl Share for ZfsVol {
    type Error = Error;
    type Output = String;

    async fn share_nvmf(
        mut self: Pin<&mut Self>,
        props: Option<NvmfShareProps>,
    ) -> Result<Self::Output, Self::Error> {
        self.share_nvmf(props).await
    }
    fn create_ptpl(&self) -> Result<Option<PtplProps>, Self::Error> {
        self.ptpl().create().map_err(|source| Error::BdevShare {
            source: crate::core::CoreError::Ptpl {
                reason: source.to_string(),
            },
        })
    }

    async fn update_properties<P: Into<Option<UpdateProps>>>(
        mut self: Pin<&mut Self>,
        props: P,
    ) -> Result<(), Self::Error> {
        self.as_mut().update_share_props(props).await
    }

    async fn unshare(
        mut self: Pin<&mut Self>,
        opts: Option<UnshareProps>,
    ) -> Result<(), Self::Error> {
        self.deref_mut().unshare(opts).await
    }

    fn shared(&self) -> Option<Protocol> {
        self.share_proto()
    }

    fn share_uri(&self) -> Option<String> {
        self.uri().cloned()
    }

    fn allowed_hosts(&self) -> Vec<String> {
        self.bdev_allowed_hosts().cloned().unwrap_or_default()
    }

    fn bdev_uri(&self) -> Option<url::Url> {
        None
    }
    fn bdev_uri_original(&self) -> Option<url::Url> {
        None
    }
}
