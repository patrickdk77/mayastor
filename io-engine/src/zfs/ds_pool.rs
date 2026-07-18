use super::{
    cli::{DsProps, ZfsCmd, ZfsQueryArgs},
    error::Error,
    options::{ZfsPoolOpts, DEFAULT_VOLBLOCKSIZE},
    property::{Property, PropertyType},
    zvol_replica::VolQueryArgs,
    ZfsVol,
};
use crate::{bdev::PtplFileOps, pool_backend::PoolArgs};
use std::{collections::BTreeSet, convert::TryFrom};

/// The properties fetched for the pool container dataset detail query.
const POOL_PROPS: &str =
    "used,available,quota,encryption,io.mayastor:pool,io.mayastor:disks,io.mayastor:volblocksize";

/// A mayastor pool which is a ZFS filesystem dataset acting as the container
/// for the replica zvols, eg: creating the pool "p1" on the disks entry
/// "tank/data" creates the container dataset "tank/data/p1".
/// Ownership is persisted as the local user property "io.mayastor:pool",
/// whose value is the pool uuid.
#[derive(Debug, Clone)]
pub struct ZfsPool {
    /// The pool name, which is the last path component of the dataset.
    name: String,
    /// The full container dataset path, eg: "tank/data/p1".
    dataset: String,
    /// The parent dataset path, eg: "tank/data".
    parent: String,
    /// The verbatim disks entry, including any query parameters.
    disks: String,
    /// The pool uuid, persisted as the io.mayastor:pool user property.
    uuid: String,
    /// Space used by the container dataset and all its children.
    used: u64,
    /// Space available to the container dataset (quota-aware).
    available: u64,
    /// Sum of the child zvol volsizes.
    committed: u64,
    /// The size of the underlying zpool.
    zpool_size: u64,
    /// The default volblocksize for child zvols.
    volblocksize: u64,
    /// Whether the container dataset is encrypted.
    encrypted: bool,
}

impl ZfsPool {
    /// Lookup a single ZFS pool owned by us.
    pub(crate) async fn lookup(query: ZfsQueryArgs) -> Result<Self, Error> {
        let pools = Self::list(&query).await?;
        pools.into_iter().next().ok_or(Error::NotFound {
            query: query.query(),
        })
    }

    /// List all the ZFS pools owned by us, using the provided query options.
    /// A dataset is a mayastor pool iff it has a LOCAL io.mayastor:pool user
    /// property (inherited copies on child datasets do not count).
    pub(crate) async fn list(query: &ZfsQueryArgs) -> Result<Vec<ZfsPool>, Error> {
        // Discovery: every filesystem dataset with a local io.mayastor:pool
        // property is a mayastor pool; the property value is the pool uuid.
        let rows = ZfsCmd::zfs("get")
            .args(["-Hp", "-t", "filesystem", "-s", "local", "-o", "name,value"])
            .arg(PropertyType::PoolUuid.value())
            .rows(2)
            .await?;

        let matches = |dataset: &str, uuid: &str| {
            let name = dataset.rsplit('/').next().unwrap_or(dataset);
            query.name.as_deref().map(|n| n == name).unwrap_or(true)
                && query.uuid.as_deref().map(|u| u == uuid).unwrap_or(true)
        };
        let candidates = rows
            .into_iter()
            .filter_map(|row| {
                let [dataset, uuid] = <[String; 2]>::try_from(row).ok()?;
                matches(&dataset, &uuid).then_some((dataset, uuid))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let datasets = candidates
            .iter()
            .map(|(dataset, _)| dataset.clone())
            .collect::<Vec<_>>();

        // Detail: bulk fetch the pool properties for all matched datasets.
        let details = ZfsCmd::zfs("get")
            .args(["-Hp", "-o", "name,property,value,source"])
            .arg(POOL_PROPS)
            .args(datasets.clone())
            .prop_map()
            .await?;

        // Committed: sum of the child zvol volsizes, per container dataset.
        let vol_rows = ZfsCmd::zfs("list")
            .args(["-Hp", "-r", "-t", "volume", "-o", "name,volsize"])
            .args(datasets.clone())
            .rows(2)
            .await?;

        // Disk capacity: the size of the underlying zpool, which is the first
        // path component of the container dataset.
        let zpools = datasets
            .iter()
            .map(|ds| ds.split('/').next().unwrap_or(ds).to_string())
            .collect::<BTreeSet<_>>();
        let zpool_rows = ZfsCmd::zpool_get()
            .args(["-Hp", "-o", "name,value", "size"])
            .args(zpools)
            .rows(2)
            .await?;

        let mut pools = Vec::with_capacity(candidates.len());
        for (dataset, uuid) in candidates {
            let Some(props) = details.iter().find(|d| d.name() == dataset) else {
                continue;
            };
            let committed = vol_rows
                .iter()
                .filter(|row| {
                    row[0]
                        .rsplit_once('/')
                        .map(|(parent, _)| parent == dataset)
                        .unwrap_or(false)
                })
                .filter_map(|row| row[1].parse::<u64>().ok())
                .sum::<u64>();
            let zpool = dataset.split('/').next().unwrap_or(&dataset);
            let zpool_size = zpool_rows
                .iter()
                .find(|row| row[0] == zpool)
                .and_then(|row| row[1].parse::<u64>().ok())
                .unwrap_or_default();
            pools.push(Self::from_props(
                dataset, uuid, props, committed, zpool_size,
            ));
        }
        Ok(pools)
    }

    /// Build a `ZfsPool` from the detail properties of its container dataset.
    fn from_props(
        dataset: String,
        uuid: String,
        props: &DsProps,
        committed: u64,
        zpool_size: u64,
    ) -> Self {
        let (parent, name) = match dataset.rsplit_once('/') {
            Some((parent, name)) => (parent.to_string(), name.to_string()),
            None => (String::new(), dataset.clone()),
        };
        let disks = props
            .local(PropertyType::PoolDisks.value())
            .map(|d| d.to_string())
            .unwrap_or_else(|| dataset.clone());
        let volblocksize = props
            .local(PropertyType::PoolVolBlockSize.value())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_VOLBLOCKSIZE);
        Self {
            name,
            parent,
            disks,
            uuid,
            used: props.u64("used").unwrap_or_default(),
            available: props.u64("available").unwrap_or_default(),
            committed,
            zpool_size,
            volblocksize,
            encrypted: props
                .value("encryption")
                .map(|e| e != "off")
                .unwrap_or(false),
            dataset,
        }
    }

    /// Create a ZFS pool (or import an existing dataset with the same path,
    /// mirroring the LVM create-on-existing behaviour).
    pub async fn create(args: PoolArgs) -> Result<ZfsPool, Error> {
        tracing::info!(?args, "Creating/Importing ZFS Pool");
        if args.enc_key.is_some() {
            return Err(Error::EncryptionNotSup {});
        }
        super::is_valid_dataset_component(&args.name)?;
        let opts = ZfsPoolOpts::try_from_disks(&args.disks)?;
        let dataset = format!("{}/{}", opts.parent(), args.name);

        if Self::dataset_exists(&dataset).await? {
            let pool = Self::import_inner(&args, &opts).await?;
            info!(name = pool.name(), "ZFS Pool imported successfully");
            return Ok(pool);
        }

        let uuid = match &args.uuid {
            Some(uuid) => uuid.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let volblocksize = Self::default_volblocksize(&args, &opts)?;

        let mut cmd = ZfsCmd::zfs("create")
            .prop(Property::PoolUuid(uuid.clone()))
            .prop(Property::PoolDisks(opts.disks().to_string()))
            .prop(Property::PoolVolBlockSize(volblocksize));
        if let Some(quota) = opts.quota() {
            cmd = cmd.args(["-o".to_string(), format!("quota={quota}")]);
        }
        for (key, value) in opts.props() {
            cmd = cmd.args(["-o".to_string(), format!("{key}={value}")]);
        }
        cmd.arg(&dataset).run().await?;

        info!(name = args.name, dataset, "ZFS Pool created successfully");
        Self::lookup(ZfsQueryArgs::any().named(&args.name).uuid(&uuid)).await
    }

    /// Import a ZFS pool: the container dataset must already exist.
    /// The local io.mayastor:pool property is (re-)asserted, adopting a
    /// previously exported pool, and all owned zvols are eagerly imported as
    /// spdk bdevs (re-sharing them over nvmf from the persisted properties).
    pub(crate) async fn import(args: PoolArgs) -> Result<ZfsPool, Error> {
        let opts = ZfsPoolOpts::try_from_disks(&args.disks)?;
        let pool = Self::import_inner(&args, &opts).await?;
        info!(name = pool.name(), "ZFS Pool imported successfully");
        pool.import_vols().await?;
        Ok(pool)
    }

    /// Import a ZFS pool by asserting ownership of its container dataset.
    async fn import_inner(args: &PoolArgs, opts: &ZfsPoolOpts) -> Result<ZfsPool, Error> {
        tracing::info!(?args, "Importing ZFS Pool");
        super::is_valid_dataset_component(&args.name)?;
        let dataset = format!("{}/{}", opts.parent(), args.name);

        let details = ZfsCmd::zfs("get")
            .args([
                "-Hp",
                "-t",
                "filesystem",
                "-o",
                "name,property,value,source",
            ])
            .arg(POOL_PROPS)
            .arg(&dataset)
            .prop_map()
            .await?;
        let props = details
            .iter()
            .find(|d| d.name() == dataset)
            .ok_or(Error::NotFound {
                query: dataset.clone(),
            })?;

        // The parsed parent must match the parent persisted in the
        // io.mayastor:disks property, if any.
        if let Some(disks) = props.local(PropertyType::PoolDisks.value()) {
            let persisted = ZfsPoolOpts::try_from_disks(&[disks.to_string()])?;
            if persisted.parent() != opts.parent() {
                return Err(Error::DisksMismatch {
                    args: opts.disks().to_string(),
                    pool: disks.to_string(),
                });
            }
        }

        let mut set_props = Vec::new();
        let uuid = match props.local(PropertyType::PoolUuid.value()) {
            Some(uuid) => {
                if matches!(&args.uuid, Some(arg_uuid) if arg_uuid != uuid) {
                    return Err(Error::UuidMismatch {
                        args: args.uuid.clone().unwrap_or_default(),
                        pool: uuid.to_string(),
                    });
                }
                uuid.to_string()
            }
            None => {
                // A previously exported (or brand new) dataset: adopt it by
                // asserting the local pool property.
                let uuid = match &args.uuid {
                    Some(uuid) => uuid.clone(),
                    None => uuid::Uuid::new_v4().to_string(),
                };
                set_props.push(Property::PoolUuid(uuid.clone()));
                uuid
            }
        };
        if props.local(PropertyType::PoolDisks.value()).is_none() {
            set_props.push(Property::PoolDisks(opts.disks().to_string()));
        }
        if props
            .local(PropertyType::PoolVolBlockSize.value())
            .is_none()
        {
            set_props.push(Property::PoolVolBlockSize(Self::default_volblocksize(
                args, opts,
            )?));
        }
        if !set_props.is_empty() {
            let mut cmd = ZfsCmd::zfs("set");
            for prop in set_props {
                cmd = cmd.arg(prop.set_arg());
            }
            cmd.arg(&dataset).run().await?;
        }

        Self::lookup(ZfsQueryArgs::any().named(&args.name).uuid(&uuid)).await
    }

    /// The default volblocksize for the pool's child zvols: the create pool
    /// request cluster_size if set, else the ?volblocksize= disks query
    /// parameter, else 16KiB.
    fn default_volblocksize(args: &PoolArgs, opts: &ZfsPoolOpts) -> Result<u64, Error> {
        match args.cluster_size {
            Some(cluster_size) => {
                let volblocksize = cluster_size as u64;
                super::options::validate_volblocksize(volblocksize)?;
                Ok(volblocksize)
            }
            None => Ok(opts.volblocksize().unwrap_or(DEFAULT_VOLBLOCKSIZE)),
        }
    }

    /// Check if the given filesystem dataset exists.
    async fn dataset_exists(dataset: &str) -> Result<bool, Error> {
        match ZfsCmd::zfs("list")
            .args(["-Hp", "-t", "filesystem", "-o", "name"])
            .arg(dataset)
            .run()
            .await
        {
            Ok(()) => Ok(true),
            Err(Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Delete the ZFS pool along with all its replicas and snapshots.
    /// > Note: The pool is kept if it contains foreign child datasets.
    pub async fn destroy(mut self) -> Result<(), Error> {
        // Any child dataset lacking a local io.mayastor:uuid property was not
        // created by us, so refuse the destroy rather than eat foreign data.
        let children = ZfsCmd::zfs("get")
            .args([
                "-Hp",
                "-r",
                "-t",
                "filesystem,volume",
                "-o",
                "name,property,value,source",
            ])
            .arg(PropertyType::VolUuid.value())
            .arg(&self.dataset)
            .prop_map()
            .await?;
        let foreign = children
            .iter()
            .filter(|d| {
                d.name() != self.dataset && d.local(PropertyType::VolUuid.value()).is_none()
            })
            .map(|d| d.name().to_string())
            .collect::<Vec<_>>();
        if !foreign.is_empty() {
            warn!(
                "ZFS pool '{}' is not destroyed as it contains foreign datasets: {foreign:?}",
                self.name()
            );
            return Err(Error::ForeignDatasets { datasets: foreign });
        }

        self.export().await?;

        ZfsCmd::zfs("destroy")
            .arg("-r")
            .arg(&self.dataset)
            .run()
            .await?;
        self.ptpl().destroy().ok();

        info!("ZFS pool '{}' has been destroyed successfully", self.name());
        Ok(())
    }

    /// Exports the ZFS pool by unloading all zvol bdevs and finally removing
    /// the local io.mayastor:pool property from the container dataset.
    /// The pool will no longer be listable until it is imported again.
    pub(crate) async fn export(&mut self) -> Result<(), Error> {
        let vols = self.list_vols().await?;
        for mut vol in vols {
            vol.export_bdev().await?;
        }

        ZfsCmd::zfs("inherit")
            .arg(Property::PoolUuid(String::new()).inherit_arg())
            .arg(&self.dataset)
            .run()
            .await?;

        info!("ZFS pool '{}' has been exported successfully", self.name);
        Ok(())
    }

    /// Export all ZFS pool instances.
    pub(crate) async fn export_all() {
        let Ok(pools) = ZfsPool::list(&ZfsQueryArgs::any()).await else {
            return;
        };

        for mut pool in pools {
            pool.export().await.ok();
        }
    }

    /// Creates a [`ZfsVol`] replica from this [`ZfsPool`].
    pub async fn create_zvol(
        &self,
        args: crate::pool_backend::ReplicaArgs,
    ) -> Result<ZfsVol, Error> {
        ZfsVol::create(self, args).await
    }

    /// List the zvol replicas owned by this pool, importing their bdevs.
    pub async fn list_vols(&self) -> Result<Vec<ZfsVol>, Error> {
        ZfsVol::list(&VolQueryArgs::new().scoped(&self.dataset)).await
    }
    /// Import all owned zvols as spdk bdevs.
    async fn import_vols(&self) -> Result<(), Error> {
        self.list_vols().await?;
        Ok(())
    }

    /// Get the pool name (the last path component of the dataset).
    pub(crate) fn name(&self) -> &str {
        self.name.as_str()
    }
    /// Get the pool uuid.
    pub(crate) fn uuid(&self) -> &str {
        &self.uuid
    }
    /// Get the full container dataset path.
    pub(crate) fn dataset(&self) -> &str {
        &self.dataset
    }
    /// Get the parent dataset path.
    #[allow(unused)]
    pub(crate) fn parent(&self) -> &str {
        &self.parent
    }
    /// Get the verbatim disks entry, including any query parameters.
    pub(crate) fn disks(&self) -> String {
        self.disks.clone()
    }
    /// Get the pool capacity in bytes.
    /// This is quota-aware if a ?quota= parameter was given; otherwise it
    /// floats with the free space of the shared underlying zpool.
    pub(crate) fn capacity(&self) -> u64 {
        self.used + self.available
    }
    /// Get the pool used bytes.
    pub(crate) fn used(&self) -> u64 {
        self.used
    }
    /// Get the pool committed bytes (sum of child zvol volsizes).
    pub(crate) fn committed(&self) -> u64 {
        self.committed
    }
    /// Get the size of the underlying zpool.
    pub(crate) fn zpool_size(&self) -> u64 {
        self.zpool_size
    }
    /// Get the default volblocksize for the pool's child zvols.
    pub(crate) fn volblocksize(&self) -> u64 {
        self.volblocksize
    }
    /// Check if the container dataset is encrypted.
    pub(crate) fn encrypted(&self) -> bool {
        self.encrypted
    }

    /// Get a `PtplFileOps` from `&self`.
    pub(crate) fn ptpl(&self) -> impl PtplFileOps {
        ZfsPoolPtpl::from(self.name())
    }
    /// Get a `PtplFileOps` from a pool name.
    pub(super) fn pool_ptpl(name: &str) -> ZfsPoolPtpl {
        ZfsPoolPtpl::from(name)
    }
}

/// Persist through power loss implementation for a ZFS pool.
pub(super) struct ZfsPoolPtpl {
    name: String,
}

impl From<&str> for ZfsPoolPtpl {
    fn from(pool: &str) -> Self {
        Self {
            name: pool.to_string(),
        }
    }
}
impl PtplFileOps for ZfsPoolPtpl {
    fn destroy(&self) -> Result<(), std::io::Error> {
        if let Some(path) = self.path() {
            if path.exists() {
                std::fs::remove_dir_all(path)?;
            }
        }
        Ok(())
    }

    fn subpath(&self) -> std::path::PathBuf {
        std::path::PathBuf::from("pool/zfs/").join(&self.name)
    }
}
