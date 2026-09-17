//! `b070` — command-line management for Tripp Lite NetCommander IP KVM switches.
//!
//! This talks to the device's HTTPS management API only. It does not open a KVM
//! session; that needs the TCP 900 protocol, which is still being reversed.

use anyhow::{Context, Result};
use b070_mgmt::{Action, Client, Resource};
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

    /// Username, if the device requires one.
    #[arg(long, short = 'u', env = "B070_USER")]
    user: Option<String>,

    /// Password. Prefer the environment variable over the flag.
    #[arg(long, short = 'p', env = "B070_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// Reject the device's self-signed certificate.
    ///
    /// Off by default: these devices ship a self-signed `*.kvm.net` cert and
    /// cannot be issued a trusted one for a bare IP address.
    #[arg(long)]
    verify_tls: bool,

    /// Emit raw XML instead of a formatted summary.
    #[arg(long)]
    raw: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show firmware version and the last session.
    Status,
    /// Show network and connection configuration.
    Config,
    /// List KVM ports and their names.
    Targets,
    /// List local user accounts.
    Users {
        /// Also print stored passwords. The device keeps them in cleartext.
        #[arg(long)]
        show_passwords: bool,
    },
    /// List the switch definitions the device knows about.
    Switches,
    /// Fetch one raw document.
    Fetch {
        #[arg(value_enum)]
        resource: RawResource,
    },
    /// End the current device session.
    Logoff,
    /// Reboot the device.
    Reboot {
        /// Required. Rebooting drops every active KVM session.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum RawResource {
    Config,
    Admin,
    Switch,
    Status,
}

impl From<RawResource> for Resource {
    fn from(r: RawResource) -> Self {
        match r {
            RawResource::Config => Resource::Config,
            RawResource::Admin => Resource::Admin,
            RawResource::Switch => Resource::Switch,
            RawResource::Status => Resource::Status,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "b070=info,b070_mgmt=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let mut builder = Client::builder(&cli.host)
        .port(cli.port)
        .danger_accept_invalid_certs(!cli.verify_tls);
    if let (Some(u), Some(p)) = (&cli.user, &cli.password) {
        builder = builder.credentials(u, p);
    }
    let client = builder.build().context("building HTTPS client")?;

    match cli.command {
        Command::Status => {
            if cli.raw {
                print!("{}", client.fetch_raw(Resource::Status).await?);
            } else {
                let s = client.status().await.context("fetching status")?;
                print_kv("Severity", s.severity.as_deref());
                print_kv("Firmware", s.firmware_version.as_deref());
                print_kv("Hardware", s.hardware_version.as_deref());
                print_kv("SDF", s.sdf_version.as_deref());
                print_kv("Mode", s.mode.as_deref());
                print_kv("Last user", s.last_user.as_deref());
                print_kv("Last target", s.last_target.as_deref());
                print_kv(
                    "Last port",
                    s.last_port.map(|p| p.to_string()).as_deref(),
                );
                print_kv("Last duration", s.last_connection_duration.as_deref());
            }
        }

        Command::Config => {
            if cli.raw {
                print!("{}", client.fetch_raw(Resource::Config).await?);
            } else {
                let c = client.config().await.context("fetching config")?;
                print_kv("Device", c.device_description.as_deref());
                print_kv("Switch", c.switch_model.as_deref());
                print_kv("Ports", c.switch_ports.map(|v| v.to_string()).as_deref());
                print_kv(
                    "Virtual media",
                    c.virtual_media_enabled.map(yes_no),
                );
                println!();
                print_kv("MAC", c.network.mac.as_deref());
                print_kv(
                    "Addressing",
                    c.network
                        .use_static_ip
                        .map(|s| if s { "static" } else { "DHCP" }),
                );
                print_kv("IP", c.network.ip.as_deref());
                print_kv("Subnet", c.network.subnet.as_deref());
                print_kv("Gateway", c.network.gateway.as_deref());
                println!();
                let ports = c.connection.ports();
                if !ports.is_empty() {
                    print_kv(
                        "Session ports",
                        Some(
                            ports
                                .iter()
                                .map(u16::to_string)
                                .collect::<Vec<_>>()
                                .join(", "),
                        )
                        .as_deref(),
                    );
                }
                print_kv(
                    "Idle timeout",
                    c.connection
                        .idle_time_minutes
                        .map(|m| format!("{m} min"))
                        .as_deref(),
                );
                print_kv(
                    "HTTPS only",
                    c.connection.client_https_only.map(yes_no),
                );
                print_kv("LDAP", c.connection.ldap.map(yes_no));
            }
        }

        Command::Targets => {
            if cli.raw {
                print!("{}", client.fetch_raw(Resource::Config).await?);
            } else {
                let targets = client.targets().await.context("fetching targets")?;
                if targets.is_empty() {
                    println!("no targets configured");
                }
                for t in targets {
                    println!("{:>3}  {}", t.port, t.name);
                }
            }
        }

        Command::Users { show_passwords } => {
            if cli.raw {
                print!("{}", client.fetch_raw(Resource::Admin).await?);
            } else {
                let users = client.users().await.context("fetching users")?;
                for u in users {
                    let id = u.id.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
                    let perm = u
                        .permission
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".into());
                    let targets = u.targets.as_deref().unwrap_or("?");
                    let targets = if targets == "-1" { "all" } else { targets };
                    print!("id={id}  user={}  perm={perm}  targets={targets}", u.username);
                    if show_passwords {
                        print!("  password={}", u.password.as_deref().unwrap_or(""));
                    }
                    println!();
                }
                if !show_passwords {
                    eprintln!(
                        "\nnote: the device stores passwords in cleartext; \
                         pass --show-passwords to print them"
                    );
                }
            }
        }

        Command::Switches => {
            if cli.raw {
                print!("{}", client.fetch_raw(Resource::Switch).await?);
            } else {
                for s in client.switches().await.context("fetching switches")? {
                    println!(
                        "{:<10} {:<12} {:<16} ports={}",
                        s.name,
                        s.manufacturer.as_deref().unwrap_or("-"),
                        s.model.as_deref().unwrap_or("-"),
                        s.num_ports
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into()),
                    );
                }
            }
        }

        Command::Fetch { resource } => {
            print!("{}", client.fetch_raw(resource.into()).await?);
        }

        Command::Logoff => {
            client.perform(Action::LogOff).await.context("logging off")?;
            println!("logged off");
        }

        Command::Reboot { confirm } => {
            if !confirm {
                anyhow::bail!(
                    "refusing to reboot without --confirm; this drops every active KVM session"
                );
            }
            client.perform(Action::Reboot).await.context("rebooting")?;
            println!("reboot requested");
        }
    }

    Ok(())
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn print_kv(key: &str, value: Option<&str>) {
    if let Some(v) = value {
        if !v.is_empty() {
            println!("{key:<15} {v}");
        }
    }
}
