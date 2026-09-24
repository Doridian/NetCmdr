// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `netcmdr` — command-line management for Tripp Lite NetCommander IP KVM switches.
//!
//! Uses the `/message.pv` API over HTTPS; no KVM session.
//!
//! Each area has `show` and `set`; `netcmdr config` shows all. Each `set` is a
//! full-config read-modify-write (see [`netcmdr_mgmt::config`]); for several
//! changes at once, `export`, edit, and `import`.

mod admin;
mod args;
mod edit;
mod show;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use netcmdr_mgmt::{Client, PowerCommand};

const CONNECTION: &str = "Connection";

#[derive(Parser)]
#[command(
    name = "netcmdr",
    about = "Manage a Tripp Lite NetCommander / Minicom Smart IP KVM switch",
    version
)]
// Global so they can follow the subcommand. Each sets its own heading, since a
// command-level heading doesn't propagate to subcommands.
struct Cli {
    /// Device hostname or IP address.
    #[arg(long, short = 'H', env = "NETCMDR_HOST", global = true, help_heading = CONNECTION)]
    host: Option<String>,

    /// HTTPS port.
    #[arg(long, default_value_t = netcmdr_proto::DEFAULT_HTTPS_PORT, env = "NETCMDR_PORT", global = true, help_heading = CONNECTION)]
    port: u16,

    /// Username (default `admin`).
    #[arg(
        long,
        short = 'u',
        default_value = "admin",
        env = "NETCMDR_USER",
        global = true,
        help_heading = CONNECTION
    )]
    user: String,

    /// Password (default `access`, the factory default). Prefer the env var.
    #[arg(
        long,
        short = 'p',
        default_value = "access",
        env = "NETCMDR_PASSWORD",
        hide_env_values = true,
        global = true,
        help_heading = CONNECTION
    )]
    password: String,

    /// Print the stored LDAP password, RADIUS secrets and SNMP community
    /// instead of masking them.
    #[arg(long, global = true, help_heading = CONNECTION)]
    show_secrets: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show firmware, LAN state and per-port status.
    Status,
    /// Show every setting.
    Config,
    /// List KVM ports and their names.
    Targets,
    /// List local user accounts.
    Users,
    /// Fetch one raw document (config, status, switch, …).
    Fetch { value: String },

    /// Device name and session port.
    Device {
        #[command(subcommand)]
        cmd: SetOrShow<edit::DeviceArgs>,
    },
    /// IPv4 and IPv6 addressing.
    Network {
        #[command(subcommand)]
        cmd: SetOrShow<edit::NetworkArgs>,
    },
    /// Serial-over-SSH passthrough.
    Ssh {
        #[command(subcommand)]
        cmd: SetOrShow<edit::SshArgs>,
    },
    /// Lockout policy, password rules and idle timeout.
    Security {
        #[command(subcommand)]
        cmd: SetOrShow<edit::SecurityArgs>,
    },
    /// Which authentication sources are tried, and in what order.
    Auth {
        #[command(subcommand)]
        cmd: SetOrShow<edit::AuthArgs>,
    },
    /// LDAP directory authentication.
    Ldap {
        #[command(subcommand)]
        cmd: LdapCmd,
    },
    /// RADIUS authentication.
    Radius {
        #[command(subcommand)]
        cmd: RadiusCmd,
    },
    /// SNMP traps.
    Snmp {
        #[command(subcommand)]
        cmd: SetOrShow<edit::SnmpArgs>,
    },
    /// The device clock.
    Time {
        #[command(subcommand)]
        cmd: SetOrShow<edit::TimeArgs>,
    },
    /// The two serial ports.
    Serial {
        #[command(subcommand)]
        cmd: SerialCmd,
    },
    /// Central KVM management appliance.
    KvmManager {
        #[command(subcommand)]
        cmd: SetOrShow<edit::KvmManagerArgs>,
    },
    /// Names of the machines on the KVM ports.
    Target {
        #[command(subcommand)]
        cmd: TargetCmd,
    },
    /// Local user accounts.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Power distribution units and their outlets.
    Pdu {
        #[command(subcommand)]
        cmd: PduCmd,
    },

    /// Power-control a target's IP-PDU outlet.
    Power {
        #[arg(value_enum)]
        action: PowerAction,
        /// Target port number.
        #[arg(id = "power_port", value_name = "PORT")]
        port: u32,
    },
    /// Disconnect whoever is using a KVM port.
    Disconnect {
        #[arg(id = "disconnect_port", value_name = "PORT")]
        port: u32,
    },
    /// Lock every other user out of the device.
    Lock,
    /// Undo `lock`.
    Unlock,
    /// Release a configuration lock left behind by an interrupted save.
    ReleaseLock,

    /// Write the configuration document to a file, or to standard output.
    Export {
        file: Option<std::path::PathBuf>,
        /// Overwrite the file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Apply a configuration document written by `export`.
    Import {
        file: std::path::PathBuf,
        /// Required. This replaces every setting at once.
        #[arg(long)]
        confirm: bool,
        /// Also set the clock from the file's `<system_time>` (exports carry
        /// a stale time).
        #[arg(long)]
        set_clock: bool,
    },
    /// Download the device's own backup blob.
    Backup {
        file: std::path::PathBuf,
        /// Overwrite the file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// Restore a backup taken with `backup`.
    Restore(admin::RestoreArgs),
    /// Erase every setting and return to factory defaults.
    FactoryReset(admin::FactoryResetArgs),

    /// Install a TLS certificate for the management server.
    Certificate(admin::CertificateArgs),
    /// Upload device firmware and start the upgrade. UNTESTED — see its help.
    Upgrade(admin::UpgradeArgs),
    /// Progress of a device firmware upgrade.
    UpgradeStatus,
    /// The Server Interface Units on the KVM ports.
    Siu {
        #[command(subcommand)]
        cmd: SiuCmd,
    },

    /// Reboot the device.
    Reboot {
        /// Required. Rebooting drops every active session.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum SetOrShow<T: clap::Args> {
    /// Print the current settings.
    Show,
    /// Change settings. Options not given are left alone.
    Set(T),
}

#[derive(Subcommand)]
enum LdapCmd {
    /// Print the LDAP settings.
    Show,
    /// Change the LDAP settings.
    Set(edit::LdapArgs),
    /// The directory servers to ask.
    Server {
        #[command(subcommand)]
        cmd: LdapServerCmd,
    },
}

#[derive(Subcommand)]
enum LdapServerCmd {
    /// List the configured servers.
    List,
    /// Add a server.
    Add(edit::LdapServerArgs),
    /// Remove every server with this hostname.
    Remove {
        #[arg(id = "ldap_remove_host", value_name = "HOST")]
        host: String,
    },
}

#[derive(Subcommand)]
enum RadiusCmd {
    /// Print the RADIUS settings.
    Show,
    /// Change the RADIUS settings.
    Set(edit::RadiusArgs),
    /// The RADIUS servers to ask.
    Server {
        #[command(subcommand)]
        cmd: RadiusServerCmd,
    },
}

#[derive(Subcommand)]
enum RadiusServerCmd {
    /// List the configured servers.
    List,
    /// Add a server.
    Add(edit::RadiusServerArgs),
    /// Remove every server with this hostname.
    Remove {
        #[arg(id = "radius_remove_host", value_name = "HOST")]
        host: String,
    },
}

#[derive(Subcommand)]
enum SerialCmd {
    /// Print both serial ports' settings.
    Show,
    /// Change one serial port.
    Set(edit::SerialArgs),
}

#[derive(Subcommand)]
enum TargetCmd {
    /// List the KVM ports and their names.
    List,
    /// Rename a KVM port.
    Set(edit::TargetArgs),
}

#[derive(Subcommand)]
enum UserCmd {
    /// List the accounts and what they may reach.
    List,
    /// Create an account.
    Add(edit::UserAddArgs),
    /// Delete an account.
    Remove { name: String },
    /// Change an account's rights.
    Set(edit::UserSetArgs),
    /// Change an account's password.
    Password(edit::UserPasswordArgs),
}

#[derive(Subcommand)]
enum PduCmd {
    /// List the configured power strips and their outlet mapping.
    List,
    /// List the power strip models this firmware can drive.
    Models,
    /// Add a power strip.
    Add(edit::PduAddArgs),
    /// Remove a power strip.
    Remove { name: String },
    /// Map an outlet to the KVM port it powers.
    Outlet(edit::PduOutletArgs),
}

#[derive(Subcommand)]
enum SiuCmd {
    /// Read the SIUs' hardware and firmware revisions.
    Versions {
        /// Ports to ask: `all`, or e.g. `1,3-5`.
        #[arg(default_value = "all")]
        ports: String,
    },
    /// Upload SIU firmware and start the upgrade. UNTESTED — see its help.
    Upgrade(admin::SiuUpgradeArgs),
    /// Progress of a SIU firmware upgrade.
    UpgradeStatus,
}

#[derive(Copy, Clone, ValueEnum)]
enum PowerAction {
    Cycle,
    Up,
    Down,
}

impl From<PowerAction> for PowerCommand {
    fn from(a: PowerAction) -> Self {
        match a {
            PowerAction::Cycle => PowerCommand::Cycle,
            PowerAction::Up => PowerCommand::Up,
            PowerAction::Down => PowerCommand::Down,
        }
    }
}

impl PowerAction {
    fn name(self) -> &'static str {
        match self {
            PowerAction::Cycle => "cycle",
            PowerAction::Up => "up",
            PowerAction::Down => "down",
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "netcmdr=info,netcmdr_mgmt=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let host = cli
        .host
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no device given; pass --host or set NETCMDR_HOST"))?;

    let client = Client::connect_with(
        host,
        cli.port,
        &cli.user,
        &cli.password,
        std::time::Duration::from_secs(20),
    )
    .with_context(|| format!("connecting to {host}"))?;

    run(&client, &cli)
}

fn run(client: &Client, cli: &Cli) -> Result<()> {
    let secrets = cli.show_secrets;

    macro_rules! config {
        () => {
            client.config().context("fetching the configuration")?
        };
    }

    match &cli.command {
        Command::Status => status(client)?,
        Command::Config => show::all(&config!(), secrets),
        Command::Targets => show::targets(&config!()),
        Command::Users => show::users(&config!()),
        Command::Fetch { value } => {
            print!("{}", client.fetch_raw(value).context("fetching document")?)
        }

        Command::Device { cmd } => match cmd {
            SetOrShow::Show => show::device(&config!()),
            SetOrShow::Set(a) => edit::device(client, a)?,
        },
        Command::Network { cmd } => match cmd {
            SetOrShow::Show => show::network(&config!()),
            SetOrShow::Set(a) => edit::network(client, a)?,
        },
        Command::Ssh { cmd } => match cmd {
            SetOrShow::Show => show::ssh(&config!()),
            SetOrShow::Set(a) => edit::ssh(client, a)?,
        },
        Command::Security { cmd } => match cmd {
            SetOrShow::Show => show::security(&config!()),
            SetOrShow::Set(a) => edit::security(client, a)?,
        },
        Command::Auth { cmd } => match cmd {
            SetOrShow::Show => show::auth(&config!(), secrets),
            SetOrShow::Set(a) => edit::auth(client, a)?,
        },
        Command::Ldap { cmd } => match cmd {
            LdapCmd::Show => show::ldap(&config!(), secrets),
            LdapCmd::Set(a) => edit::ldap(client, a)?,
            LdapCmd::Server { cmd } => match cmd {
                LdapServerCmd::List => show::ldap(&config!(), secrets),
                LdapServerCmd::Add(a) => edit::ldap_server_add(client, a)?,
                LdapServerCmd::Remove { host } => edit::ldap_server_remove(client, host)?,
            },
        },
        Command::Radius { cmd } => match cmd {
            RadiusCmd::Show => show::radius(&config!(), secrets),
            RadiusCmd::Set(a) => edit::radius(client, a)?,
            RadiusCmd::Server { cmd } => match cmd {
                RadiusServerCmd::List => show::radius(&config!(), secrets),
                RadiusServerCmd::Add(a) => edit::radius_server_add(client, a)?,
                RadiusServerCmd::Remove { host } => edit::radius_server_remove(client, host)?,
            },
        },
        Command::Snmp { cmd } => match cmd {
            SetOrShow::Show => show::snmp(&config!(), secrets),
            SetOrShow::Set(a) => edit::snmp(client, a)?,
        },
        Command::Time { cmd } => match cmd {
            SetOrShow::Show => show::time(&config!()),
            SetOrShow::Set(a) => edit::time(client, a)?,
        },
        Command::Serial { cmd } => match cmd {
            SerialCmd::Show => show::serial(&config!()),
            SerialCmd::Set(a) => edit::serial(client, a)?,
        },
        Command::KvmManager { cmd } => match cmd {
            SetOrShow::Show => show::kvm_manager(&config!()),
            SetOrShow::Set(a) => edit::kvm_manager(client, a)?,
        },
        Command::Target { cmd } => match cmd {
            TargetCmd::List => show::targets(&config!()),
            TargetCmd::Set(a) => edit::target(client, a)?,
        },
        Command::User { cmd } => match cmd {
            UserCmd::List => show::users(&config!()),
            UserCmd::Add(a) => edit::user_add(client, a)?,
            UserCmd::Remove { name } => edit::user_remove(client, name)?,
            UserCmd::Set(a) => edit::user_set(client, a)?,
            UserCmd::Password(a) => edit::user_password(client, a)?,
        },
        Command::Pdu { cmd } => match cmd {
            PduCmd::List => show::power(&config!()),
            PduCmd::Models => show::pdu_models(&config!()),
            PduCmd::Add(a) => edit::pdu_add(client, a)?,
            PduCmd::Remove { name } => edit::pdu_remove(client, name)?,
            PduCmd::Outlet(a) => edit::pdu_outlet(client, a)?,
        },

        Command::Power { action, port } => {
            client
                .power(*port, (*action).into())
                .with_context(|| format!("power {} on port {port}", action.name()))?;
            println!("power {} sent to port {port}", action.name());
        }
        Command::Disconnect { port } => admin::disconnect(client, *port)?,
        Command::Lock => admin::lock(client)?,
        Command::Unlock => admin::unlock(client)?,
        Command::ReleaseLock => admin::release_lock(client)?,

        Command::Export { file, force } => admin::export(client, file.as_ref(), *force)?,
        Command::Import {
            file,
            confirm,
            set_clock,
        } => admin::import(client, file, *confirm, *set_clock)?,
        Command::Backup { file, force } => admin::backup(client, file, *force)?,
        Command::Restore(a) => admin::restore(client, a)?,
        Command::FactoryReset(a) => admin::factory_reset(client, a)?,

        Command::Certificate(a) => admin::install_certificate(client, a)?,
        Command::Upgrade(a) => admin::upgrade(client, a)?,
        Command::UpgradeStatus => admin::upgrade_status(client)?,
        Command::Siu { cmd } => match cmd {
            SiuCmd::Versions { ports } => admin::siu_versions(client, ports, port_count(client)?)?,
            SiuCmd::Upgrade(a) => admin::siu_upgrade(client, a, port_count(client)?)?,
            SiuCmd::UpgradeStatus => admin::siu_upgrade_status(client)?,
        },

        Command::Reboot { confirm } => {
            if !confirm {
                anyhow::bail!(
                    "refusing to reboot without --confirm; this drops every active session"
                );
            }
            client.reboot().context("rebooting")?;
            println!("reboot requested");
        }
    }

    Ok(())
}

/// KVM port count, for validation.
fn port_count(client: &Client) -> Result<u32> {
    Ok(client
        .config()
        .context("fetching the configuration")?
        .switch_ports
        .unwrap_or(0))
}

fn status(client: &Client) -> Result<()> {
    let s = client.status().context("fetching status")?;
    show::kv("Device", args::or_dash(s.name.as_deref()));
    show::kv("Health", args::or_dash(s.description.as_deref()));
    show::kv("Severity", args::or_dash(s.severity.as_deref()));
    show::kv("Firmware", args::or_dash(s.firmware_version.as_deref()));
    show::kv("Hardware", args::or_dash(s.hardware.as_deref()));
    show::kv("LAN", args::or_dash(s.lan_active.as_deref()));

    let kvm: Vec<_> = s.ports.iter().filter(|p| p.kind == "kvm").collect();
    if !kvm.is_empty() {
        println!("\nPorts:");
        for p in kvm {
            let siu = if p.has_siu() {
                format!("SIU:{}", p.rocc_type)
            } else {
                "empty".into()
            };
            let who = if p.user.is_empty() {
                String::new()
            } else {
                format!("  user={}", p.user)
            };
            println!(
                "  {:>2}  {:<12} {:<10} {}{}",
                p.number, p.name, p.status, siu, who
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap silently merges arguments sharing an id (`ldap server add HOST`
    /// once landed in `--host`); `debug_assert` catches that.
    #[test]
    fn command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }
}
