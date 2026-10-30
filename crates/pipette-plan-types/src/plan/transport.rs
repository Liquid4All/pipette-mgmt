//! How the local-dispatch runner reaches each client: one
//! [`TransportConfig`] per `[[transports]]` entry.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::ClientId;

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
/// The shell a transport's remote command is built for.
pub enum ShellType {
    #[serde(rename = "posix")]
    Posix,
    #[serde(rename = "powershell")]
    PowerShell,
}

/// Per-transport orchestration config. `client_id` is the in-plan routing
/// handle (`variants.clients`) and the warehouse join key; it encodes the
/// device's owner server-side, so there is no `owner` field.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum TransportConfig {
    #[serde(rename = "adb")]
    Adb {
        client_id: ClientId,
        serial: String,
        binary_path: String,
        work_dir: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default = "default_shell")]
        shell: ShellType,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
    /// A device attached to *another* box's adb server: `adb` runs on `host`
    /// over ssh, so the driver needs only ssh. `port` is the ssh port and
    /// `adb_port` the adb server port on `host`; `--adb-port` does not apply,
    /// as there is no tunnel to retarget.
    ///
    /// `shell` is the device shell and must be posix. The intermediate host is
    /// assumed posix too, since the device command is single-quoted for it.
    #[serde(rename = "adb_over_ssh")]
    AdbOverSsh {
        client_id: ClientId,
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        serial: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adb_port: Option<u16>,
        /// Shell command run before `adb` on the intermediate host, e.g.
        /// `"export PATH=$PATH:$HOME/Android/Sdk/platform-tools"`, for when
        /// only the login profile, which non-interactive ssh skips, puts `adb`
        /// on PATH.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_exec: Option<String>,
        binary_path: String,
        work_dir: String,
        #[serde(default = "default_shell")]
        shell: ShellType,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
    #[serde(rename = "ssh")]
    Ssh {
        client_id: ClientId,
        host: String,
        binary_path: String,
        work_dir: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default = "default_shell")]
        shell: ShellType,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
    #[serde(rename = "local")]
    Local {
        client_id: ClientId,
        binary_path: String,
        work_dir: String,
        #[serde(default = "default_shell")]
        shell: ShellType,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
    /// Each cell as its own SLURM job via a local `srun`, with pipette-ctl on
    /// the login node. `parallelism` caps concurrent `srun` allocations.
    #[serde(rename = "slurm_local")]
    SlurmLocal {
        client_id: ClientId,
        binary_path: String,
        work_dir: String,
        /// Shell command run before `srun`, e.g.
        /// `". /etc/profile.d/modules.sh && module load slurm"`, to put
        /// `srun` on PATH in a non-login shell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_exec: Option<String>,
        /// `--partition`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partition: Option<String>,
        /// `--account`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        /// `--gres=gpu:N`. Omitted when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpus: Option<u32>,
        /// `--cpus-per-task`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpus: Option<u32>,
        /// `--time`, e.g. "02:00:00".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time_limit: Option<String>,
        /// `--mem`, e.g. "32G".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mem: Option<String>,
        /// Directory for per-job `--output`/`--error` files (slurm
        /// `%x-%j`). Omit to stream task output to the driver instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_dir: Option<String>,
        /// Extra `srun` flags appended verbatim.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_srun_args: Vec<String>,
        #[serde(default = "default_shell")]
        shell: ShellType,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
    /// Same as [`Self::SlurmLocal`], but reaches the cluster over SSH:
    /// runs `ssh [user@]host srun …` so pipette-ctl can drive a remote
    /// cluster from a laptop or CI box that is not the login node.
    #[serde(rename = "slurm_over_ssh")]
    SlurmOverSsh {
        client_id: ClientId,
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        binary_path: String,
        work_dir: String,
        /// Shell command run before `srun` on the remote side, e.g.
        /// `". /etc/profile.d/modules.sh && module load slurm"`. Required
        /// when only the login profile (skipped by non-interactive ssh)
        /// puts `srun` on PATH.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_exec: Option<String>,
        /// `--partition`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partition: Option<String>,
        /// `--account`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        /// `--gres=gpu:N`. Omitted when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpus: Option<u32>,
        /// `--cpus-per-task`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpus: Option<u32>,
        /// `--time`, e.g. "02:00:00".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time_limit: Option<String>,
        /// `--mem`, e.g. "32G".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mem: Option<String>,
        /// Directory for per-job `--output`/`--error` files (slurm
        /// `%x-%j`). Omit to stream task output to the driver instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_dir: Option<String>,
        /// Extra `srun` flags appended verbatim.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_srun_args: Vec<String>,
        #[serde(default = "default_shell")]
        shell: ShellType,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
    /// Drive an iOS device from the host Mac via `xcrun devicectl`, launching
    /// the Pipette app's `headlessrun` mode: one benchmark per launch, and the
    /// app uploads its own results (`submit=1`, so no `--sync`).
    ///
    /// Prerequisites the runner cannot bootstrap: the device build installed
    /// and signed via Xcode, and the device registered on-device
    /// (`headlessrun register`).
    #[serde(rename = "ios")]
    Ios {
        client_id: ClientId,
        /// Device UDID passed to `xcrun devicectl … --device <udid>`.
        device_udid: String,
        /// App bundle id to launch. Defaults to the Pipette app.
        #[serde(default = "default_ios_bundle_id")]
        bundle_id: String,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },

    /// [`TransportConfig::Ios`] run over ssh on the Mac the iPhones are paired
    /// to, since `devicectl` only reaches devices paired with its own machine.
    /// That host is assumed posix, as for `adb_over_ssh`.
    #[serde(rename = "ios_over_ssh")]
    IosOverSsh {
        client_id: ClientId,
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// Device UDID passed to `xcrun devicectl … --device <udid>`.
        device_udid: String,
        /// App bundle id to launch. Defaults to the Pipette app.
        #[serde(default = "default_ios_bundle_id")]
        bundle_id: String,
        #[serde(
            default = "default_parallelism",
            skip_serializing_if = "is_default_parallelism"
        )]
        parallelism: NonZeroUsize,
    },
}

impl TransportConfig {
    /// Unique routing handle for this transport (also the warehouse
    /// join key). Variants reference it via `variants.clients`.
    pub fn client_id(&self) -> &ClientId {
        match self {
            TransportConfig::Adb { client_id, .. }
            | TransportConfig::AdbOverSsh { client_id, .. }
            | TransportConfig::Ssh { client_id, .. }
            | TransportConfig::Local { client_id, .. }
            | TransportConfig::SlurmLocal { client_id, .. }
            | TransportConfig::SlurmOverSsh { client_id, .. }
            | TransportConfig::Ios { client_id, .. }
            | TransportConfig::IosOverSsh { client_id, .. } => client_id,
        }
    }

    /// The remote program. iOS has none; `"xcrun"` is only for the preview,
    /// as its `build_argv` branch never reads this.
    pub fn binary_path(&self) -> &str {
        match self {
            TransportConfig::Adb { binary_path, .. }
            | TransportConfig::AdbOverSsh { binary_path, .. }
            | TransportConfig::Ssh { binary_path, .. }
            | TransportConfig::Local { binary_path, .. }
            | TransportConfig::SlurmLocal { binary_path, .. }
            | TransportConfig::SlurmOverSsh { binary_path, .. } => binary_path,
            TransportConfig::Ios { .. } | TransportConfig::IosOverSsh { .. } => "xcrun",
        }
    }

    pub fn work_dir(&self) -> &str {
        match self {
            TransportConfig::Adb { work_dir, .. }
            | TransportConfig::AdbOverSsh { work_dir, .. }
            | TransportConfig::Ssh { work_dir, .. }
            | TransportConfig::Local { work_dir, .. }
            | TransportConfig::SlurmLocal { work_dir, .. }
            | TransportConfig::SlurmOverSsh { work_dir, .. } => work_dir,
            // iOS has no remote work dir; the app manages its own state.
            TransportConfig::Ios { .. } | TransportConfig::IosOverSsh { .. } => "",
        }
    }

    pub fn shell(&self) -> ShellType {
        match self {
            TransportConfig::Adb { shell, .. }
            | TransportConfig::AdbOverSsh { shell, .. }
            | TransportConfig::Ssh { shell, .. }
            | TransportConfig::Local { shell, .. }
            | TransportConfig::SlurmLocal { shell, .. }
            | TransportConfig::SlurmOverSsh { shell, .. } => *shell,
            // `xcrun` runs on the host Mac — its own, or the intermediate one.
            TransportConfig::Ios { .. } | TransportConfig::IosOverSsh { .. } => ShellType::Posix,
        }
    }

    /// Concurrent `transport.exec` calls (worker threads) this transport
    /// adds to its host's budget; default 1. The runner takes
    /// `max(parallelism)` across transports sharing a [`Self::physical_id`]
    /// as the box's slot capacity.
    pub fn parallelism(&self) -> NonZeroUsize {
        match self {
            TransportConfig::Adb { parallelism, .. }
            | TransportConfig::AdbOverSsh { parallelism, .. }
            | TransportConfig::Ssh { parallelism, .. }
            | TransportConfig::Local { parallelism, .. }
            | TransportConfig::SlurmLocal { parallelism, .. }
            | TransportConfig::SlurmOverSsh { parallelism, .. }
            | TransportConfig::Ios { parallelism, .. }
            | TransportConfig::IosOverSsh { parallelism, .. } => *parallelism,
        }
    }

    /// Stable id of the physical machine, so transports sharing a box
    /// serialize. The bare host, not `user@host:port`: the contention is the
    /// box, not the connection. Slurm is per-transport (`slurm:<client_id>`):
    /// each `srun` lands on its own node, so its budget is its `parallelism`.
    pub fn physical_id(&self) -> String {
        match self {
            TransportConfig::Adb { serial, .. } | TransportConfig::AdbOverSsh { serial, .. } => {
                serial.clone()
            }
            TransportConfig::Ssh { host, .. } => host.clone(),
            TransportConfig::Local { .. } => "local".to_string(),
            TransportConfig::SlurmLocal { client_id, .. }
            | TransportConfig::SlurmOverSsh { client_id, .. } => format!("slurm:{client_id}"),
            // The phone, not the intermediate host, is the contended resource.
            TransportConfig::Ios { device_udid, .. }
            | TransportConfig::IosOverSsh { device_udid, .. } => format!("ios:{device_udid}"),
        }
    }

    /// Whether the runner appends `--sync` to each cell command. Not for iOS:
    /// the app's `headlessrun` carries `submit=1` and rejects a bare `--sync`.
    pub fn appends_sync_flag(&self) -> bool {
        !matches!(
            self,
            TransportConfig::Ios { .. } | TransportConfig::IosOverSsh { .. }
        )
    }
}

fn default_parallelism() -> NonZeroUsize {
    NonZeroUsize::MIN
}

fn default_ios_bundle_id() -> String {
    "ai.liquid.liquid-pipette".to_string()
}

fn is_default_parallelism(n: &NonZeroUsize) -> bool {
    n.get() == 1
}

fn default_shell() -> ShellType {
    ShellType::Posix
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    const ADB: &str = "type = \"adb\"\nclient_id = \"c1\"\nserial = \"SERIAL1\"\nbinary_path = \"/opt/pipette/bin/pipette\"\nwork_dir = \"/opt/pipette\"";
    const ADB_OVER_SSH: &str = "type = \"adb_over_ssh\"\nclient_id = \"c1\"\nhost = \"box-1\"\nuser = \"liquid\"\nserial = \"SERIAL1\"\nbinary_path = \"/opt/pipette/bin/pipette\"\nwork_dir = \"/opt/pipette\"";
    const SSH: &str = "type = \"ssh\"\nclient_id = \"c1\"\nhost = \"box-1\"\nuser = \"liquid\"\nport = 2222\nbinary_path = \"/opt/pipette/bin/pipette\"\nwork_dir = \"/opt/pipette\"";
    const LOCAL: &str = "type = \"local\"\nclient_id = \"c1\"\nbinary_path = \"/opt/pipette/bin/pipette\"\nwork_dir = \"/opt/pipette\"";
    const SLURM_LOCAL: &str = "type = \"slurm_local\"\nclient_id = \"c1\"\nbinary_path = \"/opt/pipette/bin/pipette\"\nwork_dir = \"/opt/pipette\"";
    const SLURM_OVER_SSH: &str = "type = \"slurm_over_ssh\"\nclient_id = \"c1\"\nhost = \"box-1\"\nbinary_path = \"/opt/pipette/bin/pipette\"\nwork_dir = \"/opt/pipette\"";
    const IOS: &str = "type = \"ios\"\nclient_id = \"c1\"\ndevice_udid = \"UDID-1\"";
    const IOS_OVER_SSH: &str =
        "type = \"ios_over_ssh\"\nclient_id = \"c1\"\nhost = \"box-1\"\ndevice_udid = \"UDID-1\"";

    /// Each kind's smallest entry: what every accessor answers, with `shell` and
    /// `parallelism` left to their defaults, and that the entry round-trips.
    #[rstest::rstest]
    #[case::adb(ADB, "/opt/pipette/bin/pipette", "/opt/pipette", "SERIAL1", true)]
    // The contended box is the handset, not the box hosting adb.
    #[case::adb_over_ssh(
        ADB_OVER_SSH,
        "/opt/pipette/bin/pipette",
        "/opt/pipette",
        "SERIAL1",
        true
    )]
    // The bare host, so entries reaching one box as different users share it.
    #[case::ssh(SSH, "/opt/pipette/bin/pipette", "/opt/pipette", "box-1", true)]
    #[case::local(LOCAL, "/opt/pipette/bin/pipette", "/opt/pipette", "local", true)]
    // Each `srun` lands on a node of its own, so there is no shared box.
    #[case::slurm_local(
        SLURM_LOCAL,
        "/opt/pipette/bin/pipette",
        "/opt/pipette",
        "slurm:c1",
        true
    )]
    #[case::slurm_over_ssh(
        SLURM_OVER_SSH,
        "/opt/pipette/bin/pipette",
        "/opt/pipette",
        "slurm:c1",
        true
    )]
    // The app uploads through `submit=1` and would refuse a bare `--sync`.
    #[case::ios(IOS, "xcrun", "", "ios:UDID-1", false)]
    #[case::ios_over_ssh(IOS_OVER_SSH, "xcrun", "", "ios:UDID-1", false)]
    fn every_kind_answers_its_accessors(
        #[case] entry: &str,
        #[case] binary_path: &str,
        #[case] work_dir: &str,
        #[case] physical_id: &str,
        #[case] appends_sync_flag: bool,
    ) -> anyhow::Result<()> {
        let cfg: TransportConfig = toml::from_str(entry)?;
        assert_eq!(cfg.client_id().as_str(), "c1");
        assert_eq!(cfg.binary_path(), binary_path);
        assert_eq!(cfg.work_dir(), work_dir);
        assert_eq!(cfg.shell(), ShellType::Posix);
        assert_eq!(cfg.parallelism().get(), 1);
        assert_eq!(cfg.physical_id(), physical_id);
        assert_eq!(cfg.appends_sync_flag(), appends_sync_flag);
        assert_eq!(
            toml::from_str::<TransportConfig>(&toml::to_string(&cfg)?)?,
            cfg
        );
        Ok(())
    }

    /// iOS takes no `shell`: `xcrun` runs on a posix Mac either way.
    #[rstest::rstest]
    #[case::adb(ADB, true)]
    #[case::adb_over_ssh(ADB_OVER_SSH, true)]
    #[case::ssh(SSH, true)]
    #[case::local(LOCAL, true)]
    #[case::slurm_local(SLURM_LOCAL, true)]
    #[case::slurm_over_ssh(SLURM_OVER_SSH, true)]
    #[case::ios(IOS, false)]
    #[case::ios_over_ssh(IOS_OVER_SSH, false)]
    fn an_explicit_shell_and_parallelism_are_kept(
        #[case] entry: &str,
        #[case] takes_a_shell: bool,
    ) -> anyhow::Result<()> {
        let parallel: TransportConfig = toml::from_str(&format!("{entry}\nparallelism = 4"))?;
        assert_eq!(parallel.parallelism().get(), 4);
        assert_eq!(
            toml::from_str::<TransportConfig>(&toml::to_string(&parallel)?)?,
            parallel
        );

        let powershell =
            toml::from_str::<TransportConfig>(&format!("{entry}\nshell = \"powershell\""));
        if takes_a_shell {
            let powershell = powershell?;
            assert_eq!(powershell.shell(), ShellType::PowerShell);
            assert_eq!(
                toml::from_str::<TransportConfig>(&toml::to_string(&powershell)?)?,
                powershell
            );
        } else {
            let err = powershell.err().context("an iOS kind has no shell")?;
            assert!(err.to_string().contains("unknown field `shell`"), "{err}");
        }
        Ok(())
    }

    #[rstest::rstest]
    #[case::misspelled_field(SSH, "hostname = \"box-2\"", "unknown field `hostname`")]
    #[case::another_kinds_field(LOCAL, "partition = \"gpu\"", "unknown field `partition`")]
    #[case::adb_port_without_the_hop(ADB, "adb_port = 5038", "unknown field `adb_port`")]
    #[case::zero_parallelism(LOCAL, "parallelism = 0", "nonzero")]
    fn an_entry_the_kind_does_not_take_is_refused(
        #[case] entry: &str,
        #[case] extra: &str,
        #[case] message: &str,
    ) -> anyhow::Result<()> {
        let err = toml::from_str::<TransportConfig>(&format!("{entry}\n{extra}"))
            .err()
            .context("the entry should be refused")?;
        assert!(err.to_string().contains(message), "{err}");
        Ok(())
    }

    #[test]
    fn an_unknown_kind_is_refused() -> anyhow::Result<()> {
        let err = toml::from_str::<TransportConfig>("type = \"telnet\"\nclient_id = \"c1\"")
            .err()
            .context("an unknown kind should be refused")?;
        assert!(
            err.to_string().contains("unknown variant `telnet`"),
            "{err}"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::ios(IOS)]
    #[case::ios_over_ssh(IOS_OVER_SSH)]
    fn an_ios_kind_launches_the_pipette_app_unless_told_otherwise(
        #[case] entry: &str,
    ) -> anyhow::Result<()> {
        let bundle_id = |cfg: TransportConfig| {
            if let TransportConfig::Ios { bundle_id, .. }
            | TransportConfig::IosOverSsh { bundle_id, .. } = cfg
            {
                Some(bundle_id)
            } else {
                None
            }
        };
        assert_eq!(
            bundle_id(toml::from_str(entry)?).as_deref(),
            Some("ai.liquid.liquid-pipette")
        );
        assert_eq!(
            bundle_id(toml::from_str(&format!(
                "{entry}\nbundle_id = \"com.example.app\""
            ))?)
            .as_deref(),
            Some("com.example.app")
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::posix("posix", Some(ShellType::Posix))]
    #[case::powershell("powershell", Some(ShellType::PowerShell))]
    #[case::variant_name("PowerShell", None)]
    #[case::another_shell("bash", None)]
    fn shell_type_wire_spelling(
        #[case] wire: &str,
        #[case] shell: Option<ShellType>,
    ) -> anyhow::Result<()> {
        assert_eq!(serde_json::from_value::<ShellType>(wire.into()).ok(), shell);
        if let Some(shell) = shell {
            assert_eq!(serde_json::to_value(shell)?, wire);
        }
        Ok(())
    }

    /// `port` is the ssh port and `adb_port` the adb server port; a plan that
    /// mixed them up would silently ssh to the adb port.
    #[test]
    fn adb_over_ssh_separates_ssh_and_adb_ports() -> anyhow::Result<()> {
        let cfg: TransportConfig = toml::from_str(
            "type = \"adb_over_ssh\"\n\
             client_id = \"phone-1\"\n\
             host = \"controller\"\n\
             port = 2222\n\
             serial = \"S1\"\n\
             adb_port = 5038\n\
             binary_path = \"/tmp/pipette\"\n\
             work_dir = \"/tmp\"\n",
        )?;
        assert!(matches!(
            &cfg,
            TransportConfig::AdbOverSsh {
                port: Some(2222),
                adb_port: Some(5038),
                ..
            }
        ));
        Ok(())
    }
}
