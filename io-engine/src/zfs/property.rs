//! Properties are attributes which are persisted as ZFS user properties
//! (namespace "io.mayastor:") on a given ZFS dataset and which can be used to
//! identify or retrieve specific information from a resource, even across
//! reboots.
//!
//! # Warning
//! ZFS user properties are inherited by child datasets and snapshots, so any
//! ownership check MUST require the property source to be "local", see
//! `DsProps::local`.

use crate::core::Protocol;
use std::str::FromStr;

crate::impl_properties! {
    Zfs,                                        "io.mayastor",
    PoolUuid,          String,                  "io.mayastor:pool",
    PoolDisks,         String,                  "io.mayastor:disks",
    PoolVolBlockSize,  u64,                     "io.mayastor:volblocksize",
    VolUuid,           String,                  "io.mayastor:uuid",
    Name,              String,                  "io.mayastor:name",
    Share,             crate::core::Protocol,   "io.mayastor:share",
    AllowedHosts,      Vec<String>,             "io.mayastor:allowed_hosts",
    EntityId,          String,                  "io.mayastor:entity_id",
    SnapshotUuid,      String,                  "io.mayastor:snapshot_uuid",
    ParentId,          String,                  "io.mayastor:parent_id",
    TxnId,             String,                  "io.mayastor:txn_id",
    CreateTime,        String,                  "io.mayastor:create_time",
    Discarded,         bool,                    "io.mayastor:discarded",
}

impl Property {
    /// The value of this property as a string.
    pub(super) fn value(&self) -> Option<String> {
        match self {
            Property::Zfs => None,
            Property::PoolUuid(uuid) => Some(uuid.to_owned()),
            Property::PoolDisks(disks) => Some(disks.to_owned()),
            Property::PoolVolBlockSize(size) => Some(size.to_string()),
            Property::VolUuid(uuid) => Some(uuid.to_owned()),
            Property::Name(name) => Some(name.to_owned()),
            Property::Share(protocol) => Some(protocol_str(protocol).to_owned()),
            Property::AllowedHosts(hosts) => Some(hosts.join(",")),
            Property::EntityId(entity_id) => Some(entity_id.to_owned()),
            Property::SnapshotUuid(uuid) => Some(uuid.to_owned()),
            Property::ParentId(id) => Some(id.to_owned()),
            Property::TxnId(id) => Some(id.to_owned()),
            Property::CreateTime(time) => Some(time.to_owned()),
            Property::Discarded(discarded) => Some(discarded.to_string()),
            Property::Unknown(_, value) => Some(value.to_owned()),
        }
    }
    /// Format this property as a "zfs set" argument: key=value.
    pub(super) fn set_arg(&self) -> String {
        let key = self.key();
        match self.value() {
            None => key.to_string(),
            Some(value) => format!("{key}={value}"),
        }
    }
    /// Format this property as "zfs create/snapshot/clone" arguments:
    /// -o key=value.
    pub(super) fn create_arg(&self) -> [String; 2] {
        ["-o".to_string(), self.set_arg()]
    }
    /// Format this property as a "zfs inherit" argument, which effectively
    /// removes the local value of the property.
    pub(super) fn inherit_arg(&self) -> String {
        self.key().to_string()
    }

    /// Builds a property from the given key and value.
    /// If the pair is not valid then nothing is returned.
    fn new_known(key: &str, value: &str) -> Option<Self> {
        match PropertyType::from_str(key).ok()? {
            PropertyType::Zfs => Some(Self::Zfs),
            PropertyType::PoolUuid => Some(Self::PoolUuid(value.to_owned())),
            PropertyType::PoolDisks => Some(Self::PoolDisks(value.to_owned())),
            PropertyType::PoolVolBlockSize => {
                Some(Self::PoolVolBlockSize(value.parse::<u64>().ok()?))
            }
            PropertyType::VolUuid => Some(Self::VolUuid(value.to_owned())),
            PropertyType::Name => Some(Self::Name(value.to_owned())),
            PropertyType::Share => Some(Self::Share(protocol_from(value))),
            PropertyType::AllowedHosts => Some(Self::AllowedHosts(
                value
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_owned())
                    .collect::<Vec<_>>(),
            )),
            PropertyType::EntityId => Some(Self::EntityId(value.to_owned())),
            PropertyType::SnapshotUuid => Some(Self::SnapshotUuid(value.to_owned())),
            PropertyType::ParentId => Some(Self::ParentId(value.to_owned())),
            PropertyType::TxnId => Some(Self::TxnId(value.to_owned())),
            PropertyType::CreateTime => Some(Self::CreateTime(value.to_owned())),
            PropertyType::Discarded => Some(Self::Discarded(value == "true" || value == "on")),
            _ => None,
        }
    }

    /// Builds a property from the given key/value pair, which should be in
    /// the following format: key=value.
    /// If the pair is not a known property then `Property::Unknown` is
    /// returned.
    pub(super) fn new(tag: &str) -> Self {
        match tag.split_once('=') {
            Some((key, value)) => Self::new_known(key, value)
                .unwrap_or_else(|| Property::Unknown(key.to_string(), value.to_string())),
            None => Self::new_known(tag, "")
                .unwrap_or_else(|| Property::Unknown(tag.to_string(), "".to_string())),
        }
    }

    /// Builds a property from a key and value as returned separately by
    /// "zfs get" output.
    #[allow(unused)]
    pub(super) fn from_kv(key: &str, value: &str) -> Self {
        Self::new_known(key, value)
            .unwrap_or_else(|| Property::Unknown(key.to_string(), value.to_string()))
    }
}

/// The persisted string value for the given share protocol.
pub(super) fn protocol_str(protocol: &Protocol) -> &'static str {
    match protocol {
        Protocol::Off => "off",
        Protocol::Nvmf => "nvmf",
    }
}
/// Parse a share protocol from its persisted string value.
pub(super) fn protocol_from(value: &str) -> Protocol {
    match value {
        "nvmf" => Protocol::Nvmf,
        _ => Protocol::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_round_trip() {
        let props = vec![
            Property::PoolUuid("11ec0357-7a54-4837-b655-38a410f4f75e".to_string()),
            Property::PoolDisks("tank/data?compression=zstd".to_string()),
            Property::PoolVolBlockSize(16384),
            Property::VolUuid("22ec0357-7a54-4837-b655-38a410f4f75e".to_string()),
            Property::Name("replica-1".to_string()),
            Property::Share(Protocol::Nvmf),
            Property::AllowedHosts(vec!["nqn-1".to_string(), "nqn-2".to_string()]),
            Property::EntityId("volume-1".to_string()),
            Property::SnapshotUuid("33ec0357-7a54-4837-b655-38a410f4f75e".to_string()),
            Property::ParentId("44ec0357-7a54-4837-b655-38a410f4f75e".to_string()),
            Property::TxnId("1".to_string()),
            Property::CreateTime("2026-01-01T00:00:00Z".to_string()),
            Property::Discarded(true),
        ];
        for prop in props {
            let key = prop.key().to_string();
            let value = prop.value().unwrap();
            assert_eq!(Property::from_kv(&key, &value), prop);
            assert_eq!(Property::new(&prop.set_arg()), prop);
        }
    }

    #[test]
    fn property_unknown() {
        assert_eq!(
            Property::from_kv("io.unknown:key", "value"),
            Property::Unknown("io.unknown:key".to_string(), "value".to_string())
        );
        assert_eq!(
            Property::new("io.mayastor:share=off"),
            Property::Share(Protocol::Off)
        );
        assert_eq!(
            Property::new("io.mayastor:share=nvmf"),
            Property::Share(Protocol::Nvmf)
        );
    }

    #[test]
    fn property_args() {
        let prop = Property::VolUuid("uuid-1".to_string());
        assert_eq!(prop.set_arg(), "io.mayastor:uuid=uuid-1");
        assert_eq!(
            prop.create_arg(),
            ["-o".to_string(), "io.mayastor:uuid=uuid-1".to_string()]
        );
        assert_eq!(prop.inherit_arg(), "io.mayastor:uuid");
        assert_eq!(
            Property::Discarded(false).set_arg(),
            "io.mayastor:discarded=false"
        );
    }
}
