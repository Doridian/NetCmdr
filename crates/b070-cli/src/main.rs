//! `b070` — command-line management for Tripp Lite NetCommander IP KVM switches.
//!
//! Talks to the device's `/message.pv` management API over HTTPS. It does not
//! open a KVM video session; that is a separate protocol (see `b070-proto`).

use anyhow::{Context, Result};
use b070_mgmt::{Client, PowerCommand};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "b070",
    about = "Manage a Tripp Lite NetCommander / Minicom Smart IP KVM switch",
    version
)]
struct Cli {
    /// Device hostname or IP address.
    #[arg(long, short = 'H', env = "B070_HOST")]
    host: String,

    /// HTTPS port.
    #[arg(long, default_value_t = b070_proto::DEFAULT_HTTPS_PORT, env = "B070_PORT")]
    port: u16,

    /// Username (default `admin`).
    #[arg(long, short = 'u', default_value = "admin", env = "B070_USER")]
    user: String,

    /// Password (default `access`, the factory default). Prefer the env var.
    #[arg(long, short = 'p', default_value = "access", env = "B070_PASSWORD", hide_env_values = true)]
    password: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show firmware, LAN state and per-port status.
    Status,
    /// Show device, network and user configuration.
    Config,
    /// List KVM ports, their names, and whether an SIU is attached.
    Targets,
    /// List local user accounts.
    Users,
    /// Fetch one raw document (config, status, switch, …).
    Fetch { value: String },
    /// Power-control a target's IP-PDU outlet.
    Power {
        #[arg(value_enum)]
        action: PowerAction,
        /// Target port number.
        port: u32,
    },
    /// Reboot the device.
    Reboot {
        /// Required. Rebooting drops every active session.
        #[arg(long)]
        confirm: bool,
    },
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "b070=info,b070_mgmt=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let client = Client::connect_with(
        &cli.host,
        cli.port,
        &cli.user,
        &cli.password,
        std::time::Duration::from_secs(20),
    )
    .with_context(|| format!("connecting to {}", cli.host))?;

    match cli.command {
        Command::Status => {
            let s = client.status().context("fetching status")?;
            kv("Device", s.name.as_deref());
            kv("Health", s.description.as_deref());
            kv("Severity", s.severity.as_deref());
            kv("Firmware", s.firmware_version.as_deref());
            kv("Hardware", s.hardware.as_deref());
            kv("LAN", s.lan_active.as_deref());
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
                    println!("  {:>2}  {:<12} {:<10} {}{}", p.number, p.name, p.status, siu, who);
                }
            }
        }

        Command::Config => {
            let c = client.config().context("fetching config")?;
            kv("Device", c.name.as_deref());
            kv("Hardware", c.hardware_type.as_deref());
            kv("Ports", c.switch_ports.map(|v| v.to_string()).as_deref());
            kv(
                "Idle timeout",
                c.idle_time_minutes.map(|m| format!("{m} min")).as_deref(),
            );
            if !c.session_ports.is_empty() {
                kv("Session port", Some(join_ports(&c.session_ports)).as_deref());
            }
            kv(
                "SSH",
                Some(if c.ssh_enabled {
                    format!("on ({})", join_ports(&c.ssh_ports))
                } else {
                    "off".into()
                })
                .as_deref(),
            );
            println!();
            kv("MAC", c.network.mac.as_deref());
            kv(
                "Addressing",
                Some(if c.network.use_static_ip { "static" } else { "DHCP" }),
            );
            kv("IPv4", c.network.ip.as_deref());
            kv("Subnet", c.network.subnet.as_deref());
            kv("Gateway", c.network.gateway.as_deref());
            kv("DNS", c.network.dns.as_deref());
            if c.network.ipv6_enabled {
                kv("IPv6", c.network.ipv6.as_deref());
            }
        }

        Command::Targets => {
            let c = client.config().context("fetching config")?;
            let status = client.status().ok();
            for t in c.targets {
                let siu = status
                    .as_ref()
                    .and_then(|s| s.ports.iter().find(|p| p.number == t.port && p.kind == "kvm"))
                    .map(|p| if p.has_siu() { p.rocc_type.as_str() } else { "empty" })
                    .unwrap_or("");
                println!("{:>2}  {:<14} {}", t.port, t.name, siu);
            }
        }

        Command::Users => {
            let c = client.config().context("fetching config")?;
            for u in c.users {
                println!(
                    "{:<16} {}{}  targets={}",
                    u.name,
                    if u.admin { "admin" } else { "user " },
                    if u.active { "" } else { " (disabled)" },
                    u.target_ports.as_deref().unwrap_or("-"),
                );
            }
        }

        Command::Fetch { value } => {
            print!("{}", client.fetch_raw(&value).context("fetching document")?);
        }

        Command::Power { action, port } => {
            client
                .power(port, action.into())
                .with_context(|| format!("power {:?} on port {port}", action_name(action)))?;
            println!("power {} sent to port {port}", action_name(action));
        }

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

fn action_name(a: PowerAction) -> &'static str {
    match a {
        PowerAction::Cycle => "cycle",
        PowerAction::Up => "up",
        PowerAction::Down => "down",
    }
}

fn join_ports(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn kv(key: &str, value: Option<&str>) {
    if let Some(v) = value {
        if !v.is_empty() {
            println!("{key:<14} {v}");
        }
    }
}
