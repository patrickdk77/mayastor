use super::error::{self, Error};

use nix::errno::Errno;
use snafu::ResultExt;
use std::{collections::HashMap, convert::TryFrom};
use tokio::{io::AsyncWriteExt, process::Command};

/// Common set of query options for a ZFS pool or volume.
/// ZFS has no server-side selection (unlike LVM's --select), so the filtering
/// is done client-side after the bulk queries.
#[derive(Default, Debug, Clone)]
pub(crate) struct ZfsQueryArgs {
    /// Find entries with the given name.
    pub(super) name: Option<String>,
    /// Find entries with the given uuid.
    pub(super) uuid: Option<String>,
}
impl ZfsQueryArgs {
    /// Find any and all entries in the system.
    pub(crate) fn any() -> Self {
        Self::default()
    }
    /// Find entries with the given name.
    pub(crate) fn named_opt(self, name: &Option<String>) -> Self {
        let Some(name) = name else {
            return self;
        };
        Self {
            name: Some(name.to_string()),
            ..self
        }
    }
    /// Find the entry with the given uuid.
    pub(crate) fn uuid_opt(self, uuid: &Option<String>) -> Self {
        let Some(uuid) = uuid else {
            return self;
        };
        Self {
            uuid: Some(uuid.to_string()),
            ..self
        }
    }
    /// Find entries with the given name.
    pub(crate) fn named(self, name: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            ..self
        }
    }
    /// Find the entry with the given uuid.
    pub(crate) fn uuid(self, uuid: &str) -> Self {
        Self {
            uuid: Some(uuid.to_string()),
            ..self
        }
    }
    /// Get a display string of the query, for error messages.
    pub(super) fn query(&self) -> String {
        let mut query = String::new();
        if let Some(name) = &self.name {
            query.push_str(&format!("name={name},"));
        }
        if let Some(uuid) = &self.uuid {
            query.push_str(&format!("uuid={uuid},"));
        }
        query.trim_end_matches(',').to_string()
    }
}

/// ZFS wrapper over `Command` with added qol such as error mapping and
/// decoding of the -Hp tab-separated output.
pub(super) struct ZfsCmd {
    cmd: &'static str,
    argv: Vec<String>,
    input: Option<String>,
    /// A validation error, deferred from a builder method (e.g. `prop`) so it
    /// can be surfaced when the command is run rather than silently dropped.
    deferred: Option<Error>,
}

impl ZfsCmd {
    /// Prepare a `Command` for the given zfs subcommand.
    pub(super) fn zfs(sub_cmd: &str) -> Self {
        Self {
            cmd: "zfs",
            argv: vec![sub_cmd.to_string()],
            input: None,
            deferred: None,
        }
    }
    /// Prepare a `Command` for "zpool get".
    pub(super) fn zpool_get() -> Self {
        Self {
            cmd: "zpool",
            argv: vec!["get".to_string()],
            input: None,
            deferred: None,
        }
    }
    /// See help for `Command::arg`.
    pub(super) fn arg<S: Into<String>>(mut self, arg: S) -> Self {
        self.argv.push(arg.into());
        self
    }
    /// See help for `Command::args`.
    pub(super) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.extend(args.into_iter().map(Into::into));
        self
    }
    /// Add the argument only if the condition is true.
    pub(super) fn arg_if<S: Into<String>>(self, cond: bool, arg: S) -> Self {
        if cond {
            self.arg(arg)
        } else {
            self
        }
    }
    /// Add the given `Property` as a creation argument: -o key=value.
    ///
    /// The property value is rejected if it contains a control character.
    /// ZFS user properties accept arbitrary strings, but a tab or newline in a
    /// value would desynchronize the tab/newline-delimited `-Hp` output parser
    /// (`parse_rows`/`parse_props`) and break listing/import for the whole
    /// pool, so free-form values (name, entity_id, ...) are validated here, at
    /// the single choke point every property flows through.
    pub(super) fn prop(mut self, property: super::property::Property) -> Self {
        if self.deferred.is_none() {
            if let Some(value) = property.value() {
                if let Some(bad) = value.chars().find(|c| c.is_control()) {
                    self.deferred = Some(Error::InvalidPropertyValue {
                        key: property.key().to_string(),
                        error: format!("value contains a control character ({:#04x})", bad as u32),
                    });
                    return self;
                }
            }
        }
        self.args(property.create_arg())
    }
    /// Add the given `Property` as a creation argument, if the condition is
    /// true.
    pub(super) fn prop_if(self, cond: bool, property: super::property::Property) -> Self {
        if cond {
            self.prop(property)
        } else {
            self
        }
    }
    /// Run the command with the given input on stdin.
    #[allow(unused)]
    pub(super) fn input(mut self, input: String) -> Self {
        self.input = Some(input);
        self
    }
    /// The full argument vector of the command, for unit testing.
    #[cfg(test)]
    pub(super) fn argv(&self) -> Vec<String> {
        let mut argv = vec![self.cmd.to_string()];
        argv.extend(self.argv.iter().cloned());
        argv
    }

    /// Runs the ZFS command with the provided `Command` arguments et al.
    ///
    /// # Errors
    ///
    /// `Error::ZfsBinSpawnErr` => Failed to execute or await for completion.
    /// Stderr-mapped errors, see `Error::from_zfs_stderr`.
    pub(super) async fn run(self) -> Result<(), Error> {
        self.output().await.map(|_| ())
    }

    /// Runs the ZFS command and returns its stdout as a lossy utf-8 string.
    pub(super) async fn stdout(self) -> Result<String, Error> {
        let output = self.output().await?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Runs the ZFS command, which must produce -Hp tab-separated output, and
    /// decodes it into rows of exactly `ncols` columns.
    pub(super) async fn rows(self, ncols: usize) -> Result<Vec<Vec<String>>, Error> {
        let cmd = self.cmd;
        let stdout = self.stdout().await?;
        parse_rows(cmd, &stdout, ncols)
    }

    /// Runs the ZFS command, which must produce -Hp tab-separated output with
    /// the "name,property,value,source" columns, and decodes it into one
    /// `DsProps` property map per dataset.
    pub(super) async fn prop_map(self) -> Result<Vec<DsProps>, Error> {
        let cmd = self.cmd;
        let stdout = self.stdout().await?;
        parse_props(cmd, &stdout)
    }

    /// Runs the ZFS command with the provided `Command` arguments et all and
    /// returns the `std::process::Output` in case of success.
    ///
    /// # Errors
    ///
    /// `Error::ZfsBinSpawnErr` => Failed to execute or await for completion.
    /// Stderr-mapped errors, see `Error::from_zfs_stderr`.
    pub(super) async fn output(mut self) -> Result<std::process::Output, Error> {
        if let Some(error) = self.deferred.take() {
            return Err(error);
        }
        tracing::trace!("{} {:?}", self.cmd, self.argv);

        let in_spdk = spdk_rs::Thread::is_spdk_thread();
        let fut = async move {
            let output = self.cmder().await.context(error::ZfsBinSpawnErrSnafu {
                command: self.cmd.to_string(),
            })?;
            if !output.status.success() {
                let error = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(Error::from_zfs_stderr(
                    self.cmd,
                    error.trim_start().trim_end().to_string(),
                ));
            }
            Ok(output)
        };
        if in_spdk {
            crate::zfs_tokio_run!(fut)
        } else {
            fut.await
        }
    }

    async fn cmder(&self) -> std::io::Result<std::process::Output> {
        let mut cmder = Command::new(self.cmd);
        cmder.args(&self.argv);
        unsafe {
            cmder.pre_exec(|| {
                Self::close_range().ok();
                Ok(())
            });
        }

        let Some(input) = self.input.clone() else {
            return cmder.output().await;
        };

        let mut child = cmder.stdin(std::process::Stdio::piped()).spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        child.wait_with_output().await
    }

    /// The close_range system call closes all open file descriptors from first to last (included).
    /// Here close from 3 to 1024.
    /// todo: find a better way such as querying /proc/self/fd ?.
    fn close_range() -> nix::Result<()> {
        let res = unsafe { libc::close_range(3, 1024, libc::CLOSE_RANGE_CLOEXEC as i32) };
        Errno::result(res).map(drop)
    }
}

/// The source of a ZFS property value, as reported in the SOURCE column of
/// "zfs get". User properties inherit, so ownership checks must require
/// `PropSource::Local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropSource {
    /// The property is set locally on the dataset itself.
    Local,
    /// The property is inherited from an ancestor dataset.
    Inherited,
    /// The property is at its default value.
    Default,
    /// The property was received via zfs receive.
    Received,
    /// Native statistic properties ("-") and unset properties ("none").
    None,
}

impl PropSource {
    /// Parse the SOURCE column of "zfs get" output.
    pub(super) fn parse(source: &str) -> Self {
        match source {
            "local" => Self::Local,
            "default" => Self::Default,
            "received" => Self::Received,
            source if source.starts_with("inherited") => Self::Inherited,
            _ => Self::None,
        }
    }
}

/// The properties of a single dataset, decoded from the bulk
/// "zfs get -Hp -o name,property,value,source" output.
#[derive(Debug, Default, Clone)]
pub(crate) struct DsProps {
    /// The full dataset name, eg: "tank/data/pool-1/replica-1".
    name: String,
    /// Property name => (value, source).
    props: HashMap<String, (String, PropSource)>,
}

impl DsProps {
    /// The full dataset name.
    pub(super) fn name(&self) -> &str {
        &self.name
    }
    /// Get the value of the given property, from any source.
    /// Unset properties (value "-") yield `None`.
    pub(super) fn value(&self, key: &str) -> Option<&str> {
        match self.props.get(key) {
            Some((value, _)) if value != "-" => Some(value.as_str()),
            _ => None,
        }
    }
    /// Get the value of the given property, only if it is set locally on the
    /// dataset itself. This is the ownership primitive: inherited mayastor
    /// user properties do NOT mark a dataset as ours.
    pub(super) fn local(&self, key: &str) -> Option<&str> {
        match self.props.get(key) {
            Some((value, PropSource::Local)) if value != "-" => Some(value.as_str()),
            _ => None,
        }
    }
    /// Get the value of the given property as a u64, from any source.
    pub(super) fn u64(&self, key: &str) -> Option<u64> {
        self.value(key)?.parse::<u64>().ok()
    }
    /// Check if the given property is set to "on", from any source.
    #[allow(unused)]
    pub(super) fn bool_on(&self, key: &str) -> bool {
        self.value(key) == Some("on")
    }
}

/// Parse -Hp tab-separated output into rows of exactly `ncols` columns.
pub(super) fn parse_rows(
    command: &str,
    output: &str,
    ncols: usize,
) -> Result<Vec<Vec<String>>, Error> {
    let mut rows = Vec::new();
    for line in output.lines().filter(|l| !l.is_empty()) {
        let cols = line.split('\t').map(String::from).collect::<Vec<_>>();
        if cols.len() != ncols {
            return Err(Error::OutputParsing {
                command: command.to_string(),
                error: format!("expected {ncols} columns, got {}: '{line}'", cols.len()),
            });
        }
        rows.push(cols);
    }
    Ok(rows)
}

/// Parse the bulk "zfs get -Hp -o name,property,value,source" output into a
/// `DsProps` per dataset, preserving the dataset output order.
pub(super) fn parse_props(command: &str, output: &str) -> Result<Vec<DsProps>, Error> {
    let rows = parse_rows(command, output, 4)?;
    let mut list: Vec<DsProps> = Vec::new();
    for row in rows {
        let [name, property, value, source] = match <[String; 4]>::try_from(row) {
            Ok(row) => row,
            Err(_) => unreachable!("column count verified by parse_rows"),
        };
        let entry = match list.last_mut() {
            Some(last) if last.name == name => last,
            _ => match list.iter_mut().find(|e| e.name == name) {
                Some(entry) => entry,
                None => {
                    list.push(DsProps {
                        name,
                        props: HashMap::new(),
                    });
                    list.last_mut().unwrap()
                }
            },
        };
        entry
            .props
            .insert(property, (value, PropSource::parse(&source)));
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_argv_building() {
        let cmd = ZfsCmd::zfs("create")
            .arg_if(true, "-s")
            .args(["-V", "1073741824", "-b", "16384"])
            .prop(super::super::property::Property::VolUuid(
                "uuid-1".to_string(),
            ))
            .prop_if(
                false,
                super::super::property::Property::EntityId("e-1".to_string()),
            )
            .arg("tank/data/pool-1/uuid-1");
        assert_eq!(
            cmd.argv(),
            vec![
                "zfs",
                "create",
                "-s",
                "-V",
                "1073741824",
                "-b",
                "16384",
                "-o",
                "io.mayastor:uuid=uuid-1",
                "tank/data/pool-1/uuid-1",
            ]
        );

        let cmd = ZfsCmd::zpool_get()
            .args(["-Hp", "-o", "name,value", "size"])
            .arg("tank");
        assert_eq!(
            cmd.argv(),
            vec!["zpool", "get", "-Hp", "-o", "name,value", "size", "tank"]
        );
    }

    #[test]
    fn prop_source_parsing() {
        assert_eq!(PropSource::parse("local"), PropSource::Local);
        assert_eq!(PropSource::parse("default"), PropSource::Default);
        assert_eq!(PropSource::parse("received"), PropSource::Received);
        assert_eq!(
            PropSource::parse("inherited from tank/data"),
            PropSource::Inherited
        );
        assert_eq!(PropSource::parse("-"), PropSource::None);
        assert_eq!(PropSource::parse("none"), PropSource::None);
    }

    #[test]
    fn ds_props_local_vs_inherited() {
        let output = "tank/data/p1\tio.mayastor:pool\t11ec0357\tlocal\n\
                      tank/data/p1\tused\t2048\t-\n\
                      tank/data/p1/r1\tio.mayastor:pool\t11ec0357\tinherited from tank/data/p1\n\
                      tank/data/p1/r1\tio.mayastor:uuid\tr1-uuid\tlocal\n\
                      tank/data/p1/r1\tvolsize\t1073741824\tlocal\n\
                      tank/data/p1/r1\torigin\t-\t-\n";
        let props = parse_props("zfs", output).unwrap();
        assert_eq!(props.len(), 2);

        let pool = &props[0];
        assert_eq!(pool.name(), "tank/data/p1");
        assert_eq!(pool.local("io.mayastor:pool"), Some("11ec0357"));
        assert_eq!(pool.u64("used"), Some(2048));

        let vol = &props[1];
        assert_eq!(vol.name(), "tank/data/p1/r1");
        // The pool uuid is visible as a value but is NOT local.
        assert_eq!(vol.value("io.mayastor:pool"), Some("11ec0357"));
        assert_eq!(vol.local("io.mayastor:pool"), None);
        assert_eq!(vol.local("io.mayastor:uuid"), Some("r1-uuid"));
        assert_eq!(vol.u64("volsize"), Some(1073741824));
        // Unset native properties ("-") yield None.
        assert_eq!(vol.value("origin"), None);
        assert_eq!(vol.value("missing"), None);
    }

    #[test]
    fn rows_parsing() {
        let rows = parse_rows("zfs", "a\tb\nc\td\n", 2).unwrap();
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
        assert!(parse_rows("zfs", "a\tb\tc\n", 2).is_err());
        assert!(parse_rows("zfs", "", 2).unwrap().is_empty());
    }

    #[tokio::test]
    async fn prop_rejects_control_characters() {
        use super::super::property::Property;
        // A tab, newline or other control character in a free-form value would
        // corrupt the -Hp output parser, so it is rejected before the command
        // is run (output() short-circuits without spawning the zfs binary).
        for bad in ["foo\tbar", "foo\nbar", "foo\rbar"] {
            let cmd = ZfsCmd::zfs("create").prop(Property::Name(bad.to_string()));
            assert!(
                matches!(cmd.output().await, Err(Error::InvalidPropertyValue { .. })),
                "value {:?} must be rejected",
                bad
            );
        }
        // The first offending property short-circuits the whole command.
        let cmd = ZfsCmd::zfs("create")
            .prop(Property::EntityId("bad\nid".to_string()))
            .prop(Property::Name("good".to_string()));
        assert!(matches!(
            cmd.output().await,
            Err(Error::InvalidPropertyValue { .. })
        ));
    }

    #[test]
    fn prop_accepts_ordinary_values() {
        use super::super::property::Property;
        // A well-formed value is not deferred: it is placed on the argv.
        let cmd = ZfsCmd::zfs("create").prop(Property::Name("replica-1".to_string()));
        assert!(cmd.deferred.is_none());
        assert!(cmd
            .argv()
            .contains(&"io.mayastor:name=replica-1".to_string()));
    }
}
