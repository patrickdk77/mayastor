use super::{
    cli::{DsProps, ZfsCmd, ZfsQueryArgs},
    error::Error,
    property::{Property, PropertyType},
    zvol_replica::{wait_for_zvol_device, VolQueryArgs, ZfsVol},
};
use crate::core::{
    snapshot::{ISnapshotDescriptor, SnapshotDescriptor, SnapshotInfo},
    CloneParams, Protocol, SnapshotParams,
};

/// The properties fetched for the snapshot bulk query.
const SNAP_PROPS: &str = "used,referenced,volsize,volblocksize,clones,defer_destroy,\
io.mayastor:pool,io.mayastor:snapshot_uuid,io.mayastor:name,io.mayastor:entity_id,\
io.mayastor:parent_id,io.mayastor:txn_id,io.mayastor:create_time,io.mayastor:discarded";

/// Different list options for a zvol snapshot.
#[derive(Default, Debug)]
pub(crate) struct SnapQueryArgs {
    /// Find the snapshot with the given snapshot uuid.
    uuid: Option<String>,
    /// Find the snapshots of the given source replica uuid.
    source_uuid: Option<String>,
    /// Pertaining the pool container.
    pool: ZfsQueryArgs,
    /// Scope the query to the snapshots of the given zvol dataset.
    dataset: Option<String>,
}
impl SnapQueryArgs {
    /// Builder-like creating a default `Self`.
    pub(crate) fn new() -> Self {
        Self::default()
    }
    /// Find the snapshot with the given snapshot uuid.
    pub(crate) fn uuid(self, uuid: &str) -> Self {
        Self {
            uuid: Some(uuid.to_string()),
            ..self
        }
    }
    /// Find the snapshot with the given snapshot uuid.
    pub(crate) fn uuid_opt(self, uuid: &Option<String>) -> Self {
        let Some(uuid) = uuid else {
            return self;
        };
        self.uuid(uuid)
    }
    /// Find the snapshots of the given source replica uuid.
    pub(crate) fn source_opt(self, source_uuid: &Option<String>) -> Self {
        let Some(source_uuid) = source_uuid else {
            return self;
        };
        Self {
            source_uuid: Some(source_uuid.to_string()),
            ..self
        }
    }
    /// Add the pool query args.
    #[allow(unused)]
    pub(crate) fn with_pool(self, pool: ZfsQueryArgs) -> Self {
        Self { pool, ..self }
    }
    /// Scope the query to the snapshots of the given zvol dataset.
    pub(crate) fn scoped(self, dataset: &str) -> Self {
        Self {
            dataset: Some(dataset.to_string()),
            ..self
        }
    }
    /// Get a display string of the query, for error messages.
    pub(super) fn query(&self) -> String {
        format!(
            "uuid={},source={}",
            self.uuid.as_deref().unwrap_or_default(),
            self.source_uuid.as_deref().unwrap_or_default()
        )
    }
}

/// A mayastor snapshot which is a native ZFS snapshot of the replica zvol:
/// <container>/<replica-uuid>@<snapshot-uuid>.
/// The `SnapshotParams` are persisted atomically as user properties of the
/// snapshot itself (zfs snapshot -o), which are the analogs of the LVS blob
/// xattrs. A deleted snapshot which still has clones is kept as "discarded"
/// (io.mayastor:discarded=true + zfs deferred destroy) until its last clone
/// goes away.
#[derive(Debug, Clone)]
pub struct ZfsSnapshot {
    /// The full snapshot path: <dataset>@<snapshot-uuid>.
    path: String,
    /// The parent zvol dataset path.
    dataset: String,
    /// The pool container dataset path.
    container: String,
    /// The pool name (last path component of the container).
    pool_name: String,
    /// The pool uuid, inherited from the io.mayastor:pool user property.
    pool_uuid: String,
    /// The snapshot uuid, from the local io.mayastor:snapshot_uuid property.
    uuid: String,
    /// The snapshot creation parameters, reconstructed from the user
    /// properties.
    params: SnapshotParams,
    /// Space uniquely allocated by this snapshot.
    used: u64,
    /// Space referenced by this snapshot.
    referenced: u64,
    /// The volsize of the snapshotted zvol.
    volsize: u64,
    /// The volblocksize of the snapshotted zvol.
    volblocksize: u64,
    /// The clones created from this snapshot (full dataset paths).
    clones: Vec<String>,
    /// Whether the snapshot has been discarded (deleted whilst still having
    /// clones).
    discarded: bool,
}

impl ZfsSnapshot {
    /// Lookup a single snapshot.
    pub(crate) async fn lookup(args: &SnapQueryArgs) -> Result<Self, Error> {
        let snaps = Self::list(args).await?;
        snaps.into_iter().next().ok_or(Error::SnapNotFound {
            query: args.query(),
        })
    }

    /// List snapshots using the provided options as query criteria.
    /// The query is a single bulk "zfs get" whose output is filtered
    /// client-side. Ours = snapshots with a LOCAL io.mayastor:snapshot_uuid
    /// property (set atomically at snapshot creation).
    pub(crate) async fn list(opts: &SnapQueryArgs) -> Result<Vec<ZfsSnapshot>, Error> {
        let mut cmd = ZfsCmd::zfs("get")
            .args(["-Hp", "-t", "snapshot", "-o", "name,property,value,source"])
            .arg(SNAP_PROPS);
        if let Some(dataset) = &opts.dataset {
            cmd = cmd.args(["-d", "1"]).arg(dataset);
        }
        let all = cmd.prop_map().await?;
        Ok(all
            .iter()
            .filter_map(Self::from_props)
            .filter(|snap| snap.matches(opts))
            .collect())
    }

    /// Build a `ZfsSnapshot` from its bulk query properties, yielding nothing
    /// if the snapshot is not owned by us.
    fn from_props(props: &DsProps) -> Option<ZfsSnapshot> {
        let path = props.name().to_string();
        let (dataset, _) = path.split_once('@')?;
        let (container, _) = dataset.rsplit_once('/')?;
        // Ours-check: the snapshot uuid property MUST be local; a snapshot
        // taken out-of-band would only inherit it from its dataset.
        let uuid = props.local(PropertyType::SnapshotUuid.value())?.to_string();
        let pool_uuid = props.value(PropertyType::PoolUuid.value())?.to_string();
        let pool_name = container.rsplit('/').next().unwrap_or(container);
        let discarded_prop = props
            .local(PropertyType::Discarded.value())
            .map(|v| v == "true" || v == "on")
            .unwrap_or(false);
        let params = SnapshotParams::new(
            props
                .local(PropertyType::EntityId.value())
                .map(String::from),
            props
                .local(PropertyType::ParentId.value())
                .map(String::from),
            props.local(PropertyType::TxnId.value()).map(String::from),
            props.local(PropertyType::Name.value()).map(String::from),
            Some(uuid.clone()),
            props
                .local(PropertyType::CreateTime.value())
                .map(String::from),
            discarded_prop,
        );
        Some(ZfsSnapshot {
            dataset: dataset.to_string(),
            container: container.to_string(),
            pool_name: pool_name.to_string(),
            pool_uuid,
            uuid,
            params,
            used: props.u64("used").unwrap_or_default(),
            referenced: props.u64("referenced").unwrap_or_default(),
            volsize: props.u64("volsize").unwrap_or_default(),
            volblocksize: props
                .u64("volblocksize")
                .unwrap_or(super::options::DEFAULT_VOLBLOCKSIZE),
            clones: props
                .value("clones")
                .map(|clones| {
                    clones
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            discarded: discarded_prop || props.value("defer_destroy") == Some("on"),
            path,
        })
    }

    /// Check if the snapshot matches the list options.
    fn matches(&self, opts: &SnapQueryArgs) -> bool {
        let named = |name: Option<&String>, value: &str| name.map(|n| n == value).unwrap_or(true);
        named(opts.uuid.as_ref(), &self.uuid)
            && opts
                .source_uuid
                .as_ref()
                .map(|source| {
                    self.params.parent_id().as_deref() == Some(source.as_str())
                        || self.dataset.rsplit('/').next() == Some(source.as_str())
                })
                .unwrap_or(true)
            && named(opts.pool.name.as_ref(), &self.pool_name)
            && named(opts.pool.uuid.as_ref(), &self.pool_uuid)
    }

    /// Create a snapshot of the given zvol, persisting the snapshot
    /// parameters atomically as user properties of the snapshot.
    pub(crate) async fn create(vol: &ZfsVol, params: SnapshotParams) -> Result<Self, Error> {
        let Some(uuid) = params.snapshot_uuid() else {
            return Err(Error::InvalidOption {
                error: "a snapshot uuid must be provided".to_string(),
            });
        };
        super::is_valid_dataset_component(&uuid)?;
        let path = format!("{}@{uuid}", vol.dataset());
        info!(uuid, path, "Creating ZFS Snapshot");

        let mut cmd = ZfsCmd::zfs("snapshot")
            .prop(Property::SnapshotUuid(uuid.clone()))
            .prop(Property::ParentId(
                params.parent_id().unwrap_or_else(|| vol.uuid().to_string()),
            ))
            .prop(Property::CreateTime(
                params
                    .create_time()
                    .unwrap_or_else(|| chrono::Utc::now().to_string()),
            ))
            .prop(Property::Discarded(params.discarded_snapshot()));
        if let Some(name) = params.name() {
            cmd = cmd.prop(Property::Name(name));
        }
        if let Some(entity_id) = params.entity_id() {
            cmd = cmd.prop(Property::EntityId(entity_id));
        }
        if let Some(txn_id) = params.txn_id() {
            cmd = cmd.prop(Property::TxnId(txn_id));
        }
        cmd.arg(&path).run().await?;

        let snapshot =
            Self::lookup(&SnapQueryArgs::new().uuid(&uuid).scoped(vol.dataset())).await?;

        info!(uuid, path, "ZFS Snapshot created successfully");
        Ok(snapshot)
    }

    /// Destroy the snapshot.
    /// When clones still reference the snapshot's data, it is marked as
    /// discarded and destroyed with zfs deferred destroy (-d), so ZFS itself
    /// removes it when the last clone goes away.
    pub(crate) async fn destroy(self) -> Result<(), Error> {
        if self.clones.is_empty() {
            ZfsCmd::zfs("destroy").arg(&self.path).run().await?;
            info!("ZFS snapshot '{}' deleted", self.path);
        } else {
            ZfsCmd::zfs("set")
                .arg(Property::Discarded(true).set_arg())
                .arg(&self.path)
                .run()
                .await?;
            ZfsCmd::zfs("destroy")
                .arg("-d")
                .arg(&self.path)
                .run()
                .await?;
            info!(
                "ZFS snapshot '{}' discarded (deferred destroy, has clones: {:?})",
                self.path, self.clones
            );
        }
        Ok(())
    }

    /// Creates a **COW** clone zvol from the snapshot, in the same pool
    /// container. Encryption properties are never passed: they are read-only
    /// on clones and inherited from the origin. The volsize/volblocksize are
    /// likewise inherited from the origin.
    pub(crate) async fn create_clone(&self, params: CloneParams) -> Result<ZfsVol, Error> {
        let (Some(clone_uuid), Some(clone_name)) = (params.clone_uuid(), params.clone_name())
        else {
            return Err(Error::InvalidOption {
                error: "a clone uuid and name must be provided".to_string(),
            });
        };
        super::is_valid_dataset_component(&clone_uuid)?;
        let dataset = format!("{}/{clone_uuid}", self.container);
        info!(source = self.path, dataset, "Creating ZFS Clone");

        ZfsCmd::zfs("clone")
            .prop(Property::VolUuid(clone_uuid.clone()))
            .prop(Property::Name(clone_name))
            .prop(Property::SnapshotUuid(self.uuid.clone()))
            .prop(Property::Share(Protocol::Off))
            .arg(&self.path)
            .arg(&dataset)
            .run()
            .await?;

        wait_for_zvol_device(format!("/dev/zvol/{dataset}")).await?;

        let clone = ZfsVol::lookup(
            &VolQueryArgs::new()
                .with_vol(ZfsQueryArgs::any().uuid(&clone_uuid))
                .scoped(&self.container),
        )
        .await?;

        info!(
            source = self.path,
            dataset, "ZFS Clone created successfully"
        );
        Ok(clone)
    }

    /// Gets the `SnapshotDescriptor` which contains all snapshot related
    /// information.
    pub(crate) fn descriptor(&self) -> SnapshotDescriptor {
        let valid = self.params.name().is_some()
            && self.params.parent_id().is_some()
            && self.params.entity_id().is_some()
            && self.params.txn_id().is_some()
            && self.params.create_time().is_some();
        let info = SnapshotInfo::new(
            self.source_uuid(),
            self.referenced,
            self.params.clone(),
            self.clones.len() as u64,
            valid,
        );
        SnapshotDescriptor::new(self.clone(), info)
    }

    /// The uuid of the replica this snapshot was created from.
    pub(crate) fn source_uuid(&self) -> String {
        self.params.parent_id().unwrap_or_else(|| {
            self.dataset
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string()
        })
    }
    /// The snapshot uuid.
    pub(crate) fn uuid(&self) -> &str {
        &self.uuid
    }
    /// The full snapshot path: <dataset>@<snapshot-uuid>.
    #[allow(unused)]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
    /// The clones created from this snapshot (full dataset paths).
    pub(crate) fn clones(&self) -> &Vec<String> {
        &self.clones
    }
    /// Check if the snapshot has been discarded: either explicitly via our
    /// user property or via the zfs deferred destroy flag.
    pub(crate) fn discarded(&self) -> bool {
        self.discarded
    }
}

impl crate::core::LogicalVolume for ZfsSnapshot {
    fn name(&self) -> String {
        self.params.name().unwrap_or_else(|| self.uuid.clone())
    }

    fn uuid(&self) -> String {
        self.uuid.clone()
    }

    fn pool_name(&self) -> String {
        self.pool_name.clone()
    }

    fn pool_uuid(&self) -> String {
        self.pool_uuid.clone()
    }

    fn entity_id(&self) -> Option<String> {
        self.params.entity_id()
    }

    fn is_thin(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn size(&self) -> u64 {
        self.volsize
    }

    fn committed(&self) -> u64 {
        self.volsize
    }

    fn allocated(&self) -> u64 {
        self.referenced
    }

    fn usage(&self) -> crate::core::logical_volume::LvolSpaceUsage {
        let cluster_size = self.volblocksize;
        crate::core::logical_volume::LvolSpaceUsage {
            capacity_bytes: self.volsize,
            allocated_bytes: self.referenced,
            cluster_size,
            num_clusters: self.volsize / cluster_size,
            num_allocated_clusters: self.referenced.div_ceil(cluster_size),
            allocated_bytes_snapshots: self.used,
            num_allocated_clusters_snapshots: self.used.div_ceil(cluster_size),
            allocated_bytes_snapshot_from_clone: None,
        }
    }

    fn is_snapshot(&self) -> bool {
        true
    }

    fn is_clone(&self) -> bool {
        false
    }

    fn backend(&self) -> crate::pool_backend::PoolBackend {
        crate::pool_backend::PoolBackend::Zfs
    }

    fn snapshot_uuid(&self) -> Option<String> {
        None
    }

    fn share_protocol(&self) -> Protocol {
        Protocol::Off
    }

    fn bdev_share_uri(&self) -> Option<String> {
        None
    }

    fn nvmf_allowed_hosts(&self) -> Vec<String> {
        vec![]
    }

    fn encrypted(&self) -> bool {
        false
    }
}
