//! Pool and volume creation options for the ZFS backend.
//!
//! Pool level defaults are passed as query parameters on the single disks
//! entry, example: "tank/data?compression=zstd&volblocksize=16k&quota=200GiB".
//! These are applied to the pool container dataset at creation time, and ZFS
//! inheritance applies them to all child zvols automatically.
//!
//! Per replica overrides are passed as an opaque string map on the create
//! replica request, and are applied to the zvol at creation time, taking
//! precedence over the pool defaults.

use super::error::Error;

/// The default zvol volblocksize used when neither the create pool request
/// nor the disks query parameters specify one.
pub(super) const DEFAULT_VOLBLOCKSIZE: u64 = 16 * 1024;
/// Volblocksize validation bounds (both inclusive).
const VOLBLOCKSIZE_MIN: u64 = 512;
const VOLBLOCKSIZE_MAX: u64 = 128 * 1024;

/// ZFS dataset properties which may be set at pool level and are inherited
/// by the child zvols.
const POOL_PROP_KEYS: [&str; 4] = ["compression", "dedup", "logbias", "sync"];
/// ZFS dataset properties which may be set per replica.
const VOL_PROP_KEYS: [&str; 4] = ["compression", "dedup", "logbias", "sync"];

/// Pool creation options, parsed from the disks string of the create/import
/// pool requests.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ZfsPoolOpts {
    /// The parent dataset path, without any query parameters.
    parent: String,
    /// The verbatim disks entry, including any query parameters.
    disks: String,
    /// The default volblocksize for child zvols.
    volblocksize: Option<u64>,
    /// A quota for the pool container dataset.
    quota: Option<u64>,
    /// Allow-listed inheritable properties, passed verbatim to zfs create -o.
    props: Vec<(String, String)>,
}

impl ZfsPoolOpts {
    /// Parse the pool options from the disks entries of a create/import pool
    /// request. Exactly one disks entry is expected, in the form:
    /// <parent/dataset/path>[?key=value[&key=value]].
    pub(super) fn try_from_disks(disks: &[String]) -> Result<Self, Error> {
        let disk = match disks {
            [disk] => disk,
            [] => {
                return Err(Error::InvalidDisks {
                    error: "a ZFS pool requires exactly one disks entry, none given".to_string(),
                })
            }
            _ => {
                return Err(Error::InvalidDisks {
                    error: format!(
                        "a ZFS pool requires exactly one disks entry, {} given",
                        disks.len()
                    ),
                })
            }
        };
        let (parent, query) = match disk.split_once('?') {
            None => (disk.as_str(), ""),
            Some((parent, query)) => (parent, query),
        };
        super::is_valid_dataset_path(parent)?;

        let mut opts = ZfsPoolOpts {
            parent: parent.to_string(),
            disks: disk.to_string(),
            ..Default::default()
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(Error::InvalidOption {
                    error: format!("'{pair}' is not a key=value pair"),
                });
            };
            match key {
                "volblocksize" => {
                    let size = parse_size(value)?;
                    validate_volblocksize(size)?;
                    opts.volblocksize = Some(size);
                }
                "quota" => {
                    opts.quota = Some(parse_size(value)?);
                }
                key if POOL_PROP_KEYS.contains(&key) => {
                    validate_prop_value(key, value)?;
                    opts.props.push((key.to_string(), value.to_string()));
                }
                key => {
                    return Err(Error::InvalidOption {
                        error: format!("'{key}' is not a supported ZFS pool option"),
                    });
                }
            }
        }
        Ok(opts)
    }

    /// The parent dataset path, without any query parameters.
    pub(super) fn parent(&self) -> &str {
        &self.parent
    }
    /// The verbatim disks entry, including any query parameters.
    pub(super) fn disks(&self) -> &str {
        &self.disks
    }
    /// The default volblocksize for child zvols, if specified.
    pub(super) fn volblocksize(&self) -> Option<u64> {
        self.volblocksize
    }
    /// The pool container dataset quota, if specified.
    pub(super) fn quota(&self) -> Option<u64> {
        self.quota
    }
    /// Allow-listed inheritable properties for zfs create -o.
    pub(super) fn props(&self) -> &[(String, String)] {
        &self.props
    }
}

/// Zvol creation options, parsed from the properties map of a create replica
/// request. Values here override the pool level defaults.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ZvolCreateOpts {
    /// The volblocksize for the zvol, immutable after creation.
    volblocksize: Option<u64>,
    /// Allow-listed zvol properties, passed verbatim to zfs create -o.
    props: Vec<(String, String)>,
}

impl ZvolCreateOpts {
    /// Parse the zvol creation options from the properties of a create
    /// replica request. Unknown keys are rejected.
    pub(super) fn try_from_properties(properties: &[(String, String)]) -> Result<Self, Error> {
        let mut opts = ZvolCreateOpts::default();
        for (key, value) in properties {
            match key.as_str() {
                "volblocksize" => {
                    let size = parse_size(value)?;
                    validate_volblocksize(size)?;
                    opts.volblocksize = Some(size);
                }
                key if VOL_PROP_KEYS.contains(&key) => {
                    validate_prop_value(key, value)?;
                    opts.props.push((key.to_string(), value.to_string()));
                }
                key => {
                    return Err(Error::InvalidOption {
                        error: format!("'{key}' is not a supported ZFS replica property"),
                    });
                }
            }
        }
        Ok(opts)
    }

    /// The volblocksize override, if specified.
    pub(super) fn volblocksize(&self) -> Option<u64> {
        self.volblocksize
    }
    /// Allow-listed zvol properties for zfs create -o.
    pub(super) fn props(&self) -> &[(String, String)] {
        &self.props
    }
}

/// Parse a size given either as plain bytes or with a binary unit suffix,
/// ZFS style: K/M/G/T/P (case-insensitive), optionally followed by "iB"/"B".
pub(super) fn parse_size(value: &str) -> Result<u64, Error> {
    let error = |error: String| Error::InvalidOption { error };
    let stripped = value.trim();
    if stripped.is_empty() {
        return Err(error("empty size value".to_string()));
    }
    let digits_end = stripped
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(stripped.len());
    let (digits, suffix) = stripped.split_at(digits_end);
    let number = digits
        .parse::<u64>()
        .map_err(|e| error(format!("'{value}' is not a valid size: {e}")))?;
    let multiplier: u64 = match suffix
        .to_ascii_lowercase()
        .trim_end_matches("ib")
        .trim_end_matches('b')
    {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        "p" => 1 << 50,
        _ => {
            return Err(error(format!("'{value}' has an unknown size suffix")));
        }
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| error(format!("'{value}' overflows")))
}

/// Validate a zvol volblocksize: a power of two, within [512, 128KiB].
pub(super) fn validate_volblocksize(size: u64) -> Result<(), Error> {
    if !size.is_power_of_two() || !(VOLBLOCKSIZE_MIN..=VOLBLOCKSIZE_MAX).contains(&size) {
        return Err(Error::InvalidOption {
            error: format!(
                "volblocksize {size} must be a power of two between {VOLBLOCKSIZE_MIN} and {VOLBLOCKSIZE_MAX}"
            ),
        });
    }
    Ok(())
}

/// Round the size up to the next multiple of the volblocksize.
pub(super) fn round_up(size: u64, volblocksize: u64) -> u64 {
    size.div_ceil(volblocksize) * volblocksize
}

/// Validate a passthrough property value, which must be a simple word, as
/// one of the well-known zfs property values (on, off, zstd, lz4, ...).
/// The actual value validation is left to the zfs binary itself.
fn validate_prop_value(key: &str, value: &str) -> Result<(), Error> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::InvalidOption {
            error: format!("'{value}' is not a valid value for '{key}'"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disks(disk: &str) -> Vec<String> {
        vec![disk.to_string()]
    }

    #[test]
    fn pool_opts_parse_matrix() {
        let opts = ZfsPoolOpts::try_from_disks(&disks("tank/data")).unwrap();
        assert_eq!(opts.parent(), "tank/data");
        assert_eq!(opts.disks(), "tank/data");
        assert_eq!(opts.volblocksize(), None);
        assert_eq!(opts.quota(), None);
        assert!(opts.props().is_empty());

        let opts = ZfsPoolOpts::try_from_disks(&disks(
            "tank/data?compression=zstd&volblocksize=16k&quota=200GiB",
        ))
        .unwrap();
        assert_eq!(opts.parent(), "tank/data");
        assert_eq!(
            opts.disks(),
            "tank/data?compression=zstd&volblocksize=16k&quota=200GiB"
        );
        assert_eq!(opts.volblocksize(), Some(16 * 1024));
        assert_eq!(opts.quota(), Some(200 * 1024 * 1024 * 1024));
        assert_eq!(
            opts.props(),
            [("compression".to_string(), "zstd".to_string())]
        );

        let opts =
            ZfsPoolOpts::try_from_disks(&disks("tank?dedup=on&logbias=throughput&sync=disabled"))
                .unwrap();
        assert_eq!(opts.parent(), "tank");
        assert_eq!(
            opts.props(),
            [
                ("dedup".to_string(), "on".to_string()),
                ("logbias".to_string(), "throughput".to_string()),
                ("sync".to_string(), "disabled".to_string()),
            ]
        );

        // Errors.
        assert!(ZfsPoolOpts::try_from_disks(&[]).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&["a".to_string(), "b".to_string()]).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("tank/data?unknown=1")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("tank/data?compression")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("tank/data?volblocksize=3000")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("tank/data?volblocksize=256")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("tank/data?volblocksize=256KiB")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("/dev/sda")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("aio:///dev/sda")).is_err());
        assert!(ZfsPoolOpts::try_from_disks(&disks("")).is_err());
    }

    #[test]
    fn zvol_opts_parse_matrix() {
        let opts = ZvolCreateOpts::try_from_properties(&[]).unwrap();
        assert_eq!(opts, ZvolCreateOpts::default());

        let opts = ZvolCreateOpts::try_from_properties(&[
            ("volblocksize".to_string(), "8K".to_string()),
            ("compression".to_string(), "lz4".to_string()),
            ("sync".to_string(), "always".to_string()),
        ])
        .unwrap();
        assert_eq!(opts.volblocksize(), Some(8 * 1024));
        assert_eq!(
            opts.props(),
            [
                ("compression".to_string(), "lz4".to_string()),
                ("sync".to_string(), "always".to_string()),
            ]
        );

        // Unknown keys and invalid values must be rejected.
        assert!(
            ZvolCreateOpts::try_from_properties(&[("quota".to_string(), "1G".to_string())])
                .is_err()
        );
        assert!(ZvolCreateOpts::try_from_properties(&[(
            "volblocksize".to_string(),
            "12345".to_string()
        )])
        .is_err());
        assert!(ZvolCreateOpts::try_from_properties(&[(
            "compression".to_string(),
            "zstd fast".to_string()
        )])
        .is_err());
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("16k").unwrap(), 16 * 1024);
        assert_eq!(parse_size("16K").unwrap(), 16 * 1024);
        assert_eq!(parse_size("16KiB").unwrap(), 16 * 1024);
        assert_eq!(parse_size("16KB").unwrap(), 16 * 1024);
        assert_eq!(parse_size("4m").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_size("200GiB").unwrap(), 200 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2T").unwrap(), 2u64 << 40);
        assert!(parse_size("").is_err());
        assert!(parse_size("k").is_err());
        assert!(parse_size("1.5G").is_err());
        assert!(parse_size("16x").is_err());
    }

    #[test]
    fn size_round_up() {
        assert_eq!(round_up(0, 16384), 0);
        assert_eq!(round_up(1, 16384), 16384);
        assert_eq!(round_up(16384, 16384), 16384);
        assert_eq!(round_up(16385, 16384), 32768);
    }
}
