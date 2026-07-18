use crate::core::ToErrno;
use nix::errno::Errno;
use snafu::Snafu;
use tonic::Status;

/// Errors which can be encountered whilst using the ZFS backend module.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("Failed to parse {command} output: {error}"))]
    OutputParsing { command: String, error: String },
    #[snafu(display("{command} command failed: {error}"))]
    ZfsBinErr { command: String, error: String },
    #[snafu(display("Failed to spawn/wait for {command} command: {source}"))]
    ZfsBinSpawnErr {
        command: String,
        source: std::io::Error,
    },
    #[snafu(display("ZFS pool parent dataset mismatch, args:{args}, pool:{pool}"))]
    DisksMismatch { args: String, pool: String },
    #[snafu(display("ZFS pool uuid mismatch, args:{args}, pool:{pool}"))]
    UuidMismatch { args: String, pool: String },
    #[snafu(display("Invalid pool disks: {error}"))]
    InvalidDisks { error: String },
    #[snafu(display("Invalid option: {error}"))]
    InvalidOption { error: String },
    #[snafu(display("Invalid dataset name '{name}': {error}"))]
    InvalidName { name: String, error: String },
    #[snafu(display("Invalid value for property '{key}': {error}"))]
    InvalidPropertyValue { key: String, error: String },
    #[snafu(display("ZFS pool {query} not found"))]
    NotFound { query: String },
    #[snafu(display("ZFS volume with {query} not found"))]
    VolNotFound { query: String },
    #[snafu(display("ZFS snapshot with {query} not found"))]
    SnapNotFound { query: String },
    #[snafu(display("ZFS pool contains foreign datasets: {datasets:?}"))]
    ForeignDatasets { datasets: Vec<String> },
    #[snafu(display("Timed out waiting for the zvol device {path} to appear"))]
    DeviceWait { path: String },
    #[snafu(display("Failed to spawn reactor task"))]
    ReactorSpawn {},
    #[snafu(display("Failed to collect result of reactor spawn"))]
    ReactorSpawnChannel {},
    #[snafu(display("Failed to import the zvol as an spdk bdev: {source}"))]
    BdevImport { source: crate::bdev_api::BdevError },
    #[snafu(display("Failed to export the zvol's spdk bdev: {source}"))]
    BdevExport { source: crate::bdev_api::BdevError },
    #[snafu(display("{source}"))]
    BdevShare { source: crate::core::CoreError },
    #[snafu(display("Bdev is shared but no uri is found"))]
    BdevShareUri {},
    #[snafu(display("{source}"))]
    BdevUnshare { source: crate::core::CoreError },
    #[snafu(display("Bdev cannot be found after successful creation"))]
    BdevMissing {},
    #[snafu(display("Failed to update bdev's {name} properties: {source}"))]
    UpdateProps {
        source: crate::core::CoreError,
        name: String,
    },
    #[snafu(display("{error}"))]
    NoSpace { error: String },
    #[snafu(display("{error}"))]
    Exists { error: String },
    #[snafu(display("{error}"))]
    HasClones { error: String },
    #[snafu(display("Volume {volume} has live snapshots and cannot be destroyed"))]
    HasLiveSnapshots { volume: String },
    #[snafu(display("Cannot shrink volume {volume}"))]
    ShrinkNotSup { volume: String },
    #[snafu(display("Pool expansion is not currently supported for ZFS pools"))]
    GrowNotSup {},
    #[snafu(display("Encryption is not currently supported for ZFS pools"))]
    EncryptionNotSup {},
    #[snafu(display("{error}"))]
    Internal { error: String },
}

impl Error {
    /// Fail method is required by the snafu::ensure! macro.
    pub(crate) fn fail<T>(self) -> Result<T, Self> {
        Err(self)
    }

    /// Map a zfs binary failure to a more specific error, based on well known
    /// stderr messages, falling back to `Error::ZfsBinErr`.
    pub(super) fn from_zfs_stderr(command: &str, error: String) -> Self {
        let lower = error.to_lowercase();
        if lower.contains("does not exist") {
            Error::NotFound { query: error }
        } else if lower.contains("already exists") {
            Error::Exists { error }
        } else if lower.contains("out of space")
            || lower.contains("no space")
            || lower.contains("quota exceeded")
        {
            Error::NoSpace { error }
        } else if lower.contains("has dependent clones") {
            Error::HasClones { error }
        } else {
            Error::ZfsBinErr {
                command: command.to_string(),
                error,
            }
        }
    }
}

impl From<Error> for Status {
    fn from(e: Error) -> Self {
        match e {
            Error::DisksMismatch { .. }
            | Error::UuidMismatch { .. }
            | Error::InvalidDisks { .. }
            | Error::InvalidOption { .. }
            | Error::InvalidName { .. }
            | Error::InvalidPropertyValue { .. } => Status::invalid_argument(e.to_string()),
            Error::NotFound { .. } | Error::VolNotFound { .. } | Error::SnapNotFound { .. } => {
                Status::not_found(e.to_string())
            }
            Error::NoSpace { .. } => Status::resource_exhausted(e.to_string()),
            Error::Exists { .. } => Status::already_exists(e.to_string()),
            Error::HasClones { .. }
            | Error::HasLiveSnapshots { .. }
            | Error::ShrinkNotSup { .. }
            | Error::GrowNotSup { .. }
            | Error::ForeignDatasets { .. }
            | Error::EncryptionNotSup { .. } => Status::failed_precondition(e.to_string()),
            _ => Status::internal(e.to_string()),
        }
    }
}

impl ToErrno for Error {
    fn to_errno(&self) -> Errno {
        match self {
            Error::OutputParsing { .. } => Errno::EIO,
            Error::ZfsBinErr { .. } => Errno::EIO,
            Error::ZfsBinSpawnErr { .. } => Errno::EIO,
            Error::DisksMismatch { .. } => Errno::EINVAL,
            Error::UuidMismatch { .. } => Errno::EINVAL,
            Error::InvalidDisks { .. } => Errno::EINVAL,
            Error::InvalidOption { .. } => Errno::EINVAL,
            Error::InvalidName { .. } => Errno::EINVAL,
            Error::InvalidPropertyValue { .. } => Errno::EINVAL,
            Error::NotFound { .. } => Errno::ENOENT,
            Error::VolNotFound { .. } => Errno::ENOENT,
            Error::SnapNotFound { .. } => Errno::ENOENT,
            Error::ForeignDatasets { .. } => Errno::EBUSY,
            Error::DeviceWait { .. } => Errno::ETIMEDOUT,
            Error::ReactorSpawn { .. } => Errno::EXFULL,
            Error::ReactorSpawnChannel { .. } => Errno::EPIPE,
            Error::BdevImport { .. } => Errno::EIO,
            Error::BdevExport { .. } => Errno::EIO,
            Error::BdevShare { .. } => Errno::EFAULT,
            Error::BdevShareUri { .. } => Errno::EFAULT,
            Error::BdevUnshare { .. } => Errno::EFAULT,
            Error::BdevMissing { .. } => Errno::ENODEV,
            Error::UpdateProps { .. } => Errno::EIO,
            Error::NoSpace { .. } => Errno::ENOSPC,
            Error::Exists { .. } => Errno::EEXIST,
            Error::HasClones { .. } => Errno::EBUSY,
            Error::HasLiveSnapshots { .. } => Errno::EBUSY,
            Error::ShrinkNotSup { .. } => Errno::ENOTSUP,
            Error::GrowNotSup { .. } => Errno::ENOTSUP,
            Error::EncryptionNotSup { .. } => Errno::ENOTSUP,
            Error::Internal { .. } => Errno::EPIPE,
        }
    }
}
