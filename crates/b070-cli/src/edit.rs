// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Changing the device's settings.
//!
//! The device has no partial write: `CMA_CONFIG` replaces the whole
//! configuration. So every command here is fetch, edit, save — see
//! [`b070_mgmt::config`] for why, and [`Edit`] for the shape it takes.
//!
//! Each command applies only the options it was actually given, and reports
//! what it changed. Giving no options changes nothing and saves nothing, which
//! makes `b070 <area> set` safe to run to see what it would ask for.
//!
//! Validation mirrors the checks the vendor's own dialogs make (`SecurityPanel`,
//! `PvUserDlg`, `SerialPortPanel`), because the device itself accepts values it
//! then cannot boot with.

use anyhow::{bail, Context, Result};
use clap::Args;

use b070_mgmt::model::DeviceConfig;
use b070_mgmt::{
    AuthMethod, Client, ConfigDoc, Ipv6Mode, LdapMode, LdapProtocol, Parity, SerialAssign,
    SystemTime, BAUD_RATES, DATA_BITS, STOP_BITS, USER_LEVEL_ADMIN, USER_LEVEL_USER,
};

use crate::args::{local_now, parse_datetime, parse_duration, parse_port_list, yes_no, OnOff};

/// How a new value reads in the report: `= value`, or "cleared" when it is the
/// empty string, which is how the device stores "nothing set".
fn to(value: &str) -> String {
    if value.is_empty() {
        "cleared".to_string()
    } else {
        format!("= {value}")
    }
}

/// One read-modify-write of the configuration.
///
/// Holds the editable document, the typed view of the same fetch for
/// validation, and the list of changes to report. Nothing is sent until
/// [`Edit::commit`], and nothing is sent at all if no change was recorded.
pub struct Edit {
    pub doc: ConfigDoc,
    pub current: DeviceConfig,
    changes: Vec<String>,
}

impl Edit {
    /// Fetch the configuration, once, in both views.
    pub fn open(client: &Client) -> Result<Edit> {
        let xml = client.config_raw().context("fetching the configuration")?;
        Ok(Edit {
            doc: ConfigDoc::parse(&xml).context("parsing the configuration")?,
            current: b070_mgmt::parse_config(&xml).context("parsing the configuration")?,
            changes: Vec::new(),
        })
    }

    /// Record a change, for the report after the save.
    pub fn note(&mut self, what: impl Into<String>) {
        self.changes.push(what.into());
    }

    /// How many KVM ports this device has, for validating port lists.
    pub fn port_count(&self) -> u32 {
        self.current.switch_ports.unwrap_or(0)
    }

    /// How many serial ports it has. Two on this hardware — and not the same
    /// number as the KVM ports, so `--serial all` must not use `port_count`.
    pub fn serial_count(&self) -> u32 {
        self.current.serial_devices.len() as u32
    }

    /// Save, unless nothing changed.
    pub fn commit(self, client: &Client) -> Result<()> {
        if self.changes.is_empty() {
            println!("nothing to change");
            return Ok(());
        }
        let outcome = client
            .save_config(&self.doc)
            .context("saving the configuration")?;
        for change in &self.changes {
            println!("set {change}");
        }
        if outcome.rebooting {
            println!("the device is rebooting to apply this; it will be back in a minute or two");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- device ---

#[derive(Args)]
pub struct DeviceArgs {
    /// Name the device reports to clients and to central management.
    #[arg(long)]
    pub name: Option<String>,

    /// TCP port the KVM session connects to. Changing this reboots the device.
    #[arg(long)]
    pub session_port: Option<u16>,
}

pub fn device(client: &Client, a: &DeviceArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(name) = a.name.as_deref() {
        if name.trim().is_empty() {
            bail!("the device name cannot be empty");
        }
        e.doc.device_mut().set("name", name.trim());
        e.note(format!("name {}", to(name.trim())));
    }

    if let Some(port) = a.session_port {
        if port < 1024 {
            bail!("the session port must be 1024 or above");
        }
        e.doc.connection().set("ip_port", port.to_string());
        e.note(format!("session TCP port = {port}"));
    }

    e.commit(client)
}

// --------------------------------------------------------------- network ---

#[derive(Args)]
pub struct NetworkArgs {
    /// Take the IPv4 address from DHCP, or use the static one below.
    #[arg(long, value_name = "dhcp|static")]
    pub ipv4: Option<Addressing>,

    #[arg(long, value_name = "ADDRESS")]
    pub address: Option<String>,

    #[arg(long, value_name = "MASK")]
    pub subnet: Option<String>,

    #[arg(long, value_name = "ADDRESS")]
    pub gateway: Option<String>,

    /// Use the DNS server given with `--dns`, or the one DHCP offers.
    #[arg(long, value_name = "dhcp|static")]
    pub dns_source: Option<Addressing>,

    #[arg(long, value_name = "ADDRESS")]
    pub dns: Option<String>,

    /// How the device gets an IPv6 address; `none` turns IPv6 off.
    #[arg(long, value_name = "MODE", value_parser = clap::builder::PossibleValuesParser::new(Ipv6Mode::VARIANTS))]
    pub ipv6: Option<String>,

    #[arg(long, value_name = "ADDRESS")]
    pub ipv6_address: Option<String>,

    /// Prefix length in bits, e.g. 64.
    #[arg(long, value_name = "BITS")]
    pub ipv6_prefix: Option<u32>,

    #[arg(long, value_name = "ADDRESS")]
    pub ipv6_gateway: Option<String>,

    #[arg(long, value_name = "auto|static")]
    pub ipv6_dns_source: Option<Ipv6DnsSource>,

    #[arg(long, value_name = "ADDRESS")]
    pub ipv6_dns: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum Addressing {
    Dhcp,
    Static,
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum Ipv6DnsSource {
    Auto,
    Static,
}

pub fn network(client: &Client, a: &NetworkArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(mode) = a.ipv4 {
        let static_ip = mode == Addressing::Static;
        // Going static with nothing configured and nothing given would strand
        // the device on 0.0.0.0, so insist on the three that matter.
        if static_ip {
            let have = |given: &Option<String>, current: &Option<String>| {
                given.is_some() || current.as_deref().is_some_and(|v| !v.is_empty())
            };
            if !have(&a.address, &e.current.network.ip)
                || !have(&a.subnet, &e.current.network.subnet)
                || !have(&a.gateway, &e.current.network.gateway)
            {
                bail!(
                    "a static address needs --address, --subnet and --gateway \
                     (unless the device already has them)"
                );
            }
        }
        e.doc.network()?.set_bool("use_static_ip", static_ip);
        e.note(format!(
            "IPv4 addressing = {}",
            if static_ip { "static" } else { "DHCP" }
        ));
    }

    for (flag, value, attr) in [
        ("address", &a.address, "ip"),
        ("subnet mask", &a.subnet, "subnet"),
        ("gateway", &a.gateway, "gateway"),
    ] {
        if let Some(v) = value.as_deref() {
            e.doc.network()?.set(attr, v.trim());
            e.note(format!("IPv4 {flag} {}", to(v.trim())));
        }
    }

    if let Some(source) = a.dns_source {
        let static_dns = source == Addressing::Static;
        e.doc.network()?.set_bool("staticdns", static_dns);
        e.note(format!(
            "IPv4 DNS = {}",
            if static_dns { "static" } else { "from DHCP" }
        ));
    }
    if let Some(v) = a.dns.as_deref() {
        e.doc.network()?.set("dns", v.trim());
        e.note(format!("IPv4 DNS server {}", to(v.trim())));
    }

    if let Some(mode) = a.ipv6.as_deref() {
        let mode: Ipv6Mode = mode.parse()?;
        let on = mode != Ipv6Mode::None;
        e.doc.network()?.set_bool("enableipv6", on);
        e.doc.network()?.set("autoconfigv6", mode.as_str());
        e.note(format!("IPv6 = {}", mode.as_str()));
    }
    if let Some(v) = a.ipv6_address.as_deref() {
        e.doc.network()?.set("ipv6", v.trim());
        e.note(format!("IPv6 address {}", to(v.trim())));
    }
    if let Some(bits) = a.ipv6_prefix {
        if bits > 128 {
            bail!("an IPv6 prefix is at most 128 bits");
        }
        e.doc.network()?.set("subnetv6", bits.to_string());
        e.note(format!("IPv6 prefix length = {bits}"));
    }
    if let Some(v) = a.ipv6_gateway.as_deref() {
        e.doc.network()?.set("gatewayv6", v.trim());
        e.note(format!("IPv6 gateway {}", to(v.trim())));
    }
    if let Some(source) = a.ipv6_dns_source {
        let static_dns = source == Ipv6DnsSource::Static;
        e.doc.network()?.set_bool("staticdnsv6", static_dns);
        e.note(format!(
            "IPv6 DNS = {}",
            if static_dns {
                "static"
            } else {
                "autoconfigured"
            }
        ));
    }
    if let Some(v) = a.ipv6_dns.as_deref() {
        e.doc.network()?.set("dnsv6", v.trim());
        e.note(format!("IPv6 DNS server {}", to(v.trim())));
    }

    e.commit(client)
}

// ------------------------------------------------------------------- SSH ---

#[derive(Args)]
pub struct SshArgs {
    /// Offer the serial ports over SSH.
    #[arg(long, value_name = "on|off")]
    pub enabled: Option<OnOff>,

    /// TCP port that reaches serial device 1.
    #[arg(long)]
    pub port1: Option<u16>,

    /// TCP port that reaches serial device 2.
    #[arg(long)]
    pub port2: Option<u16>,
}

pub fn ssh(client: &Client, a: &SshArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    // The two ports and the session port all have to differ, and the device
    // rejects anything privileged. Check against what the save will produce,
    // not against what is there now.
    let port1 = a.port1.or(e.current.ssh.port1);
    let port2 = a.port2.or(e.current.ssh.port2);
    if a.port1.is_some() || a.port2.is_some() {
        for (which, port) in [("serial 1", port1), ("serial 2", port2)] {
            if let Some(p) = port {
                if p < 1024 {
                    bail!("the {which} SSH port must be 1024 or above");
                }
                if e.current.session_ports.contains(&p) {
                    bail!("the {which} SSH port cannot be the session port ({p})");
                }
            }
        }
        if port1.is_some() && port1 == port2 {
            bail!("the two serial ports need different TCP ports");
        }
    }

    if let Some(v) = a.port1 {
        e.doc.ssh().set("port1", v.to_string());
        e.note(format!("serial 1 SSH port = {v}"));
    }
    if let Some(v) = a.port2 {
        e.doc.ssh().set("port2", v.to_string());
        e.note(format!("serial 2 SSH port = {v}"));
    }
    if let Some(v) = a.enabled {
        e.doc.ssh().set_bool("enable", v.into());
        e.note(format!("serial over SSH = {}", yes_no(v.into())));
    }

    e.commit(client)
}

// -------------------------------------------------------------- security ---

#[derive(Args)]
pub struct SecurityArgs {
    /// Failed logins that block an account, 1 to 99.
    #[arg(long, value_name = "N")]
    pub max_attempts: Option<u32>,

    /// Window those failures have to fall inside, e.g. `3m` or `1h`.
    #[arg(long, value_name = "DURATION")]
    pub attempt_window: Option<String>,

    /// How long a blocked account stays blocked, e.g. `30m`.
    #[arg(long, value_name = "DURATION", conflicts_with = "block_forever")]
    pub block_for: Option<String>,

    /// Block until an administrator clears it.
    #[arg(long)]
    pub block_forever: bool,

    /// Require long, mixed-character passwords ("high security").
    #[arg(long, value_name = "on|off")]
    pub password_rules: Option<OnOff>,

    /// Require the OSD password at the local console.
    #[arg(long, value_name = "on|off")]
    pub osd_password: Option<OnOff>,

    /// Disconnect an idle session after this many minutes; `0` never does.
    #[arg(long, value_name = "MINUTES")]
    pub idle_timeout: Option<u32>,
}

pub fn security(client: &Client, a: &SecurityArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(n) = a.max_attempts {
        // The vendor's dialog enforces this range; the device does not.
        if !(1..=99).contains(&n) {
            bail!("--max-attempts must be between 1 and 99");
        }
        e.doc.security().set("max_try", n.to_string());
        e.note(format!("block after {n} failed logins"));
    }

    if let Some(spec) = a.attempt_window.as_deref() {
        let secs = parse_duration(spec)?;
        if secs > 23 * 3600 + 59 * 60 {
            bail!("--attempt-window cannot be longer than 23h 59m");
        }
        e.doc.security().set("try_time", secs.to_string());
        e.note(format!("failed logins counted over {spec}"));
    }

    if a.block_forever {
        e.doc
            .security()
            .set_bool("forever", true)
            .set("lock_time", "0");
        e.note("blocked accounts stay blocked until cleared");
    } else if let Some(spec) = a.block_for.as_deref() {
        let secs = parse_duration(spec)?;
        if secs > 23 * 3600 + 59 * 60 {
            bail!("--block-for cannot be longer than 23h 59m");
        }
        e.doc
            .security()
            .set_bool("forever", false)
            .set("lock_time", secs.to_string());
        e.note(format!("blocked accounts stay blocked for {spec}"));
    }

    if let Some(v) = a.password_rules {
        e.doc.security().set_bool("pw_restrictions", v.into());
        e.note(format!("strict password rules = {}", yes_no(v.into())));
    }
    if let Some(v) = a.osd_password {
        e.doc.security().set_bool("osd_password", v.into());
        e.note(format!("OSD password = {}", yes_no(v.into())));
    }
    if let Some(m) = a.idle_timeout {
        e.doc.connection().set("idle_time", m.to_string());
        e.note(match m {
            0 => "no idle timeout".to_string(),
            m => format!("idle timeout = {m} min"),
        });
    }

    e.commit(client)
}

// -------------------------------------------------------- authentication ---

#[derive(Args)]
pub struct AuthArgs {
    /// Sources to try, in order, e.g. `local` or `local,radius`.
    ///
    /// `local` is always tried first; the device's own interface does not
    /// allow otherwise, and locking out the local admin would leave no way in.
    #[arg(long, value_name = "LIST")]
    pub order: Option<String>,
}

pub fn auth(client: &Client, a: &AuthArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(spec) = a.order.as_deref() {
        let mut order = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let m: AuthMethod = part.parse().map_err(|_| {
                anyhow::anyhow!(
                    "unknown authentication source {part:?}; pick from {}",
                    AuthMethod::VARIANTS.join(", ")
                )
            })?;
            if order.contains(&m) {
                bail!("{part} is listed twice");
            }
            order.push(m);
        }
        if order.first() != Some(&AuthMethod::Local) {
            bail!(
                "local authentication must come first, so that the local admin can always log in"
            );
        }
        if order.contains(&AuthMethod::Ldap) && e.current.aaa.ldap.servers.is_empty() {
            bail!("no LDAP server is configured; add one with `b070 ldap server add` first");
        }
        if order.contains(&AuthMethod::Radius) && e.current.aaa.radius.servers.is_empty() {
            bail!("no RADIUS server is configured; add one with `b070 radius server add` first");
        }

        e.doc
            .security()
            .set("aaa_order", AuthMethod::format_order(&order));
        e.note(format!(
            "authentication order = {}",
            order
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    e.commit(client)
}

// ------------------------------------------------------------------ LDAP ---

#[derive(Args)]
pub struct LdapArgs {
    /// Where a user's permissions come from once the directory has
    /// authenticated them.
    #[arg(long, value_name = "MODE", value_parser = clap::builder::PossibleValuesParser::new(LdapMode::VARIANTS))]
    pub mode: Option<String>,

    /// DN the device binds as to search the directory.
    #[arg(long, value_name = "DN")]
    pub search_dn: Option<String>,

    /// Password for `--search-dn`. Stored on the device in the clear.
    #[arg(long, value_name = "PASSWORD")]
    pub search_password: Option<String>,

    /// Subtree to search for users.
    #[arg(long, value_name = "DN")]
    pub search_base: Option<String>,

    /// Attribute holding the login name, e.g. `cn` or `sAMAccountName`.
    #[arg(long, value_name = "ATTR")]
    pub user_attribute: Option<String>,

    /// Attribute holding the device permissions, in `user` and `group` mode.
    #[arg(long, value_name = "ATTR")]
    pub access_attribute: Option<String>,
}

pub fn ldap(client: &Client, a: &LdapArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(v) = a.mode.as_deref() {
        let mode: LdapMode = v.parse()?;
        e.doc.ldap().set("mode", mode.as_str());
        e.note(format!("LDAP permission source = {}", mode.as_str()));
    }
    for (label, value, attr) in [
        ("search DN", &a.search_dn, "search_dn"),
        ("search base", &a.search_base, "search_base"),
        ("user attribute", &a.user_attribute, "user_attrib"),
        ("access attribute", &a.access_attribute, "acl_attrib"),
    ] {
        if let Some(v) = value.as_deref() {
            e.doc.ldap().set(attr, v.trim());
            e.note(format!("LDAP {label} {}", to(v.trim())));
        }
    }
    if let Some(v) = a.search_password.as_deref() {
        e.doc.ldap().set("search_passw", v);
        // Never echo it back.
        e.note("LDAP search password");
    }

    e.commit(client)
}

#[derive(Args)]
pub struct LdapServerArgs {
    /// Hostname or address of the directory server.
    #[arg(id = "ldap_server_host", value_name = "HOST")]
    pub host: String,

    /// TCP port the directory listens on. Defaults to 636 for `ldaps`, 389
    /// otherwise. Spelled `--server-port` because the global `--port` is the
    /// KVM's own HTTPS port.
    #[arg(id = "ldap_server_port", long = "server-port")]
    pub port: Option<u16>,

    /// How to reach it.
    #[arg(long, default_value = "ldaps", value_parser = clap::builder::PossibleValuesParser::new(LdapProtocol::VARIANTS))]
    pub protocol: String,
}

pub fn ldap_server_add(client: &Client, a: &LdapServerArgs) -> Result<()> {
    let mut e = Edit::open(client)?;
    let protocol: LdapProtocol = a.protocol.parse()?;
    let port = a.port.unwrap_or(match protocol {
        LdapProtocol::Ldaps => 636,
        _ => 389,
    });

    if e.current
        .aaa
        .ldap
        .servers
        .iter()
        .any(|s| s.host.eq_ignore_ascii_case(&a.host) && s.port == Some(port))
    {
        bail!("{}:{port} is already configured", a.host);
    }

    let mut server = b070_mgmt::xml::Element::new("server");
    server
        .set("host", a.host.trim())
        .set("port", port.to_string())
        .set("proto", protocol.as_str());
    e.doc.ldap().push(server);
    e.note(format!(
        "LDAP server {}:{port} ({})",
        a.host.trim(),
        protocol.as_str()
    ));

    e.commit(client)
}

pub fn ldap_server_remove(client: &Client, host: &str) -> Result<()> {
    let mut e = Edit::open(client)?;
    let before = e.doc.ldap().children_named("server").count();
    e.doc
        .ldap()
        .children
        .retain(|n| !is_server_for_host(n, host));
    let after = e.doc.ldap().children_named("server").count();
    if before == after {
        bail!("no LDAP server called {host}");
    }
    e.note(format!("LDAP server {host} removed"));
    e.commit(client)
}

fn is_server_for_host(node: &b070_mgmt::xml::Node, host: &str) -> bool {
    matches!(
        node,
        b070_mgmt::xml::Node::Element(e)
            if e.name == "server" && e.attr_or_empty("host").eq_ignore_ascii_case(host)
    )
}

// ---------------------------------------------------------------- RADIUS ---

#[derive(Args)]
pub struct RadiusArgs {
    /// Realm appended to the username, if the server wants one.
    #[arg(long, value_name = "REALM")]
    pub realm: Option<String>,

    /// Send RADIUS accounting records.
    #[arg(long, value_name = "on|off")]
    pub accounting: Option<OnOff>,
}

pub fn radius(client: &Client, a: &RadiusArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(v) = a.realm.as_deref() {
        e.doc.radius().set("realm", v.trim());
        e.note(format!("RADIUS realm {}", to(v.trim())));
    }
    if let Some(v) = a.accounting {
        e.doc.radius().set_bool("accounting", v.into());
        e.note(format!("RADIUS accounting = {}", yes_no(v.into())));
    }

    e.commit(client)
}

#[derive(Args)]
pub struct RadiusServerArgs {
    /// Hostname or address of the RADIUS server.
    #[arg(id = "radius_server_host", value_name = "HOST")]
    pub host: String,

    /// Authentication port. Spelled `--server-port` because the global
    /// `--port` is the KVM's own HTTPS port.
    #[arg(
        id = "radius_server_port",
        long = "server-port",
        default_value_t = 1812
    )]
    pub port: u16,

    /// Accounting port.
    #[arg(long, default_value_t = 1813)]
    pub accounting_port: u16,

    /// The shared secret. Stored on the device in the clear.
    #[arg(long, value_name = "SECRET")]
    pub secret: String,
}

pub fn radius_server_add(client: &Client, a: &RadiusServerArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if e.current
        .aaa
        .radius
        .servers
        .iter()
        .any(|s| s.host.eq_ignore_ascii_case(&a.host) && s.port == Some(a.port))
    {
        bail!("{}:{} is already configured", a.host, a.port);
    }

    let mut server = b070_mgmt::xml::Element::new("server");
    server
        .set("host", a.host.trim())
        .set("port", a.port.to_string())
        .set("acc_port", a.accounting_port.to_string())
        .set("secret", a.secret.as_str());
    e.doc.radius().push(server);
    e.note(format!("RADIUS server {}:{}", a.host.trim(), a.port));

    e.commit(client)
}

pub fn radius_server_remove(client: &Client, host: &str) -> Result<()> {
    let mut e = Edit::open(client)?;
    let before = e.doc.radius().children_named("server").count();
    e.doc
        .radius()
        .children
        .retain(|n| !is_server_for_host(n, host));
    let after = e.doc.radius().children_named("server").count();
    if before == after {
        bail!("no RADIUS server called {host}");
    }
    e.note(format!("RADIUS server {host} removed"));
    e.commit(client)
}

// ------------------------------------------------------------------ SNMP ---

#[derive(Args)]
pub struct SnmpArgs {
    /// Send SNMP traps.
    #[arg(long, value_name = "on|off")]
    pub traps: Option<OnOff>,

    /// Where to send them. An empty value clears it.
    #[arg(long, value_name = "ADDRESS")]
    pub destination: Option<String>,

    /// Community string the traps carry.
    #[arg(long, value_name = "COMMUNITY")]
    pub community: Option<String>,
}

pub fn snmp(client: &Client, a: &SnmpArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(v) = a.destination.as_deref() {
        // The device's own dialog treats these two as "nowhere".
        let v = match v.trim() {
            "255.255.255.255" | "0.0.0.0" => "",
            other => other,
        };
        e.doc.snmp().set("mngr_ip", v);
        e.note(match v {
            "" => "no SNMP trap destination".to_string(),
            v => format!("SNMP trap destination = {v}"),
        });
    }
    if let Some(v) = a.community.as_deref() {
        e.doc.snmp().set("community", v.trim());
        e.note("SNMP community");
    }
    if let Some(v) = a.traps {
        let on: bool = v.into();
        if on && e.doc.snmp().attr_or_empty("mngr_ip").is_empty() {
            bail!("set a trap destination with --destination before turning traps on");
        }
        e.doc.snmp().set_bool("enable_traps", on);
        e.note(format!("SNMP traps = {}", yes_no(on)));
    }

    e.commit(client)
}

// ----------------------------------------------------- central management ---

#[derive(Args)]
pub struct KvmManagerArgs {
    /// Report to a central KVM management appliance.
    #[arg(long, value_name = "on|off")]
    pub enabled: Option<OnOff>,

    /// The appliance's address. Use `--discover` instead to find it by broadcast.
    #[arg(long, value_name = "ADDRESS", conflicts_with = "discover")]
    pub address: Option<String>,

    /// Find the appliance by broadcast rather than a fixed address.
    #[arg(long)]
    pub discover: bool,
}

pub fn kvm_manager(client: &Client, a: &KvmManagerArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(v) = a.address.as_deref() {
        e.doc
            .kvm_manager()
            .set_bool("fixed_ip", true)
            .set("ip", v.trim());
        e.note(format!("management appliance at {}", v.trim()));
    } else if a.discover {
        e.doc.kvm_manager().set_bool("fixed_ip", false);
        e.note("management appliance found by discovery");
    }
    if let Some(v) = a.enabled {
        e.doc.kvm_manager().set_bool("enable", v.into());
        e.note(format!("central management = {}", yes_no(v.into())));
    }

    e.commit(client)
}

// ----------------------------------------------------------------- clock ---

#[derive(Args)]
pub struct TimeArgs {
    /// Set the device clock from this computer's clock.
    #[arg(long, conflicts_with = "to")]
    pub now: bool,

    /// Set it to a specific time, as `YYYY-MM-DD HH:MM:SS`.
    #[arg(long, value_name = "WHEN")]
    pub to: Option<String>,
}

pub fn time(client: &Client, a: &TimeArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    let when: Option<SystemTime> = match (a.now, a.to.as_deref()) {
        (true, _) => Some(local_now()),
        (false, Some(spec)) => Some(parse_datetime(spec)?),
        (false, None) => None,
    };

    if let Some(t) = when {
        e.doc.set_system_time(t);
        e.note(format!(
            "clock = {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        ));
    }

    e.commit(client)
}

// ---------------------------------------------------------- serial ports ---

#[derive(Args)]
pub struct SerialArgs {
    /// Which serial port, 1 or 2.
    #[arg(id = "serial_port", value_name = "PORT")]
    pub port: u32,

    #[arg(long)]
    pub name: Option<String>,

    #[arg(long, value_name = "RATE", value_parser = clap::builder::PossibleValuesParser::new(BAUD_RATES))]
    pub baud: Option<String>,

    #[arg(long, value_name = "N", value_parser = clap::builder::PossibleValuesParser::new(DATA_BITS))]
    pub data_bits: Option<String>,

    #[arg(long, value_name = "N", value_parser = clap::builder::PossibleValuesParser::new(STOP_BITS))]
    pub stop_bits: Option<String>,

    #[arg(long, value_name = "PARITY", value_parser = clap::builder::PossibleValuesParser::new(Parity::VARIANTS))]
    pub parity: Option<String>,

    /// Offer this port to users.
    ///
    /// Hiding a port also strips it from every account's serial access, and
    /// showing it again does not put those grants back — see the note the
    /// command prints.
    #[arg(long, value_name = "on|off")]
    pub visible: Option<OnOff>,

    /// What is wired to the port: a console, or a power strip.
    #[arg(long, value_name = "KIND", value_parser = clap::builder::PossibleValuesParser::new(SerialAssign::VARIANTS))]
    pub assign: Option<String>,
}

pub fn serial(client: &Client, a: &SerialArgs) -> Result<()> {
    let mut e = Edit::open(client)?;
    let port = a.port;

    if let Some(v) = a.name.as_deref() {
        e.doc.serial(port)?.set("name", v.trim());
        e.note(format!("serial {port} name {}", to(v.trim())));
    }
    for (label, value, attr) in [
        ("baud rate", &a.baud, "baud_rate"),
        ("data bits", &a.data_bits, "data_bits"),
        ("stop bits", &a.stop_bits, "stop_bits"),
    ] {
        if let Some(v) = value.as_deref() {
            e.doc.serial(port)?.set(attr, v);
            e.note(format!("serial {port} {label} = {v}"));
        }
    }
    if let Some(v) = a.parity.as_deref() {
        let parity: Parity = v.parse()?;
        e.doc.serial(port)?.set("parity", parity.as_str());
        e.note(format!("serial {port} parity = {}", parity.name()));
    }
    if let Some(v) = a.visible {
        e.doc.serial(port)?.set_bool("active", v.into());
        e.note(format!(
            "serial {port} = {}",
            if bool::from(v) { "visible" } else { "hidden" }
        ));
    }
    if let Some(v) = a.assign.as_deref() {
        let assign: SerialAssign = v.parse()?;
        e.doc.serial(port)?.set("PDU", assign.as_str());
        e.note(format!("serial {port} assigned to {}", assign.name()));
    }

    // Hiding a port revokes it from every account, and showing it again does
    // not restore them — measured on firmware 2.2.1258.1.0. Name the accounts
    // now, while they still have the grant, so it can be put back.
    let losing: Vec<String> = match a.visible {
        Some(OnOff::Off) => e
            .current
            .users
            .iter()
            .filter(|u| grants_port(u.serial_target_ports.as_deref(), port))
            .map(|u| u.name.clone())
            .collect(),
        _ => Vec::new(),
    };

    e.commit(client)?;

    if !losing.is_empty() {
        eprintln!(
            "note: hiding serial {port} also removes it from {}; showing the port again \
             does not put that back, so re-grant it with `b070 user set <name> --serial …`",
            losing.join(", ")
        );
    }
    Ok(())
}

/// Whether a stored port list grants `port`. The firmware leaves trailing
/// commas in these lists, so split rather than substring-match.
fn grants_port(list: Option<&str>, port: u32) -> bool {
    list.is_some_and(|l| {
        l.split(',')
            .filter_map(|p| p.trim().parse::<u32>().ok())
            .any(|p| p == port)
    })
}

// --------------------------------------------------------------- targets ---

#[derive(Args)]
pub struct TargetArgs {
    /// KVM port number.
    #[arg(id = "target_port", value_name = "PORT")]
    pub port: u32,

    /// Name shown for the machine on that port.
    #[arg(long)]
    pub name: Option<String>,
}

pub fn target(client: &Client, a: &TargetArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    if let Some(v) = a.name.as_deref() {
        if v.trim().is_empty() {
            bail!("a target name cannot be empty");
        }
        e.doc.target(a.port)?.set("name", v.trim());
        e.note(format!("port {} name {}", a.port, to(v.trim())));
    }

    e.commit(client)
}

// ----------------------------------------------------------------- users ---

#[derive(Args)]
pub struct UserAddArgs {
    /// Login name.
    pub name: String,

    #[command(flatten)]
    pub password: PasswordArgs,

    /// Give the account full administrative rights.
    #[arg(long)]
    pub admin: bool,

    /// Create the account blocked.
    #[arg(long)]
    pub blocked: bool,

    /// KVM ports the user may reach: `all`, `none`, or e.g. `1,3-5`.
    #[arg(long, value_name = "PORTS", default_value = "none")]
    pub targets: String,

    /// Ports the user may mount virtual media on.
    #[arg(long, value_name = "PORTS", default_value = "none")]
    pub virtual_media: String,

    /// Serial ports the user may reach.
    #[arg(long, value_name = "PORTS", default_value = "none")]
    pub serial: String,
}

#[derive(Args)]
pub struct PasswordArgs {
    /// The account's password. Spelled `--new-password` because the global
    /// `--password` is the one this tool logs in to the device with.
    ///
    /// Visible to anyone who can read this machine's process list; prefer
    /// `--new-password-stdin`.
    #[arg(long, value_name = "PASSWORD", conflicts_with = "new_password_stdin")]
    pub new_password: Option<String>,

    /// Read the account's password from standard input, up to the first newline.
    #[arg(long)]
    pub new_password_stdin: bool,
}

impl PasswordArgs {
    /// The password, or `None` if neither option was given.
    pub fn read(&self) -> Result<Option<String>> {
        if let Some(p) = self.new_password.as_deref() {
            return Ok(Some(p.to_string()));
        }
        if !self.new_password_stdin {
            return Ok(None);
        }
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .context("reading the password from standard input")?;
        Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
    }
}

/// The rules the vendor's own dialog applies before it will save a password.
fn check_password(password: &str, user: &str, strict: bool) -> Result<()> {
    if password.is_empty() {
        bail!("the password cannot be empty");
    }
    if password.trim() != password {
        bail!("the password cannot start or end with a space");
    }
    if password.contains(['<', '>']) {
        bail!("the password cannot contain < or >");
    }
    if password.to_lowercase().contains(&user.to_lowercase()) {
        bail!("the password cannot contain the user name");
    }
    if !strict && password.len() < 6 {
        bail!("the password must be at least 6 characters");
    }
    if strict && password.len() < 8 {
        bail!("strict password rules are on, so the password must be at least 8 characters");
    }
    if strict
        && !(password.chars().any(|c| c.is_ascii_lowercase())
            && password.chars().any(|c| c.is_ascii_uppercase())
            && password.chars().any(|c| c.is_ascii_digit()))
    {
        bail!(
            "strict password rules are on, so the password needs an upper-case letter, \
             a lower-case letter and a digit"
        );
    }
    Ok(())
}

pub fn user_add(client: &Client, a: &UserAddArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    let name = a.name.trim();
    if name.is_empty() {
        bail!("the user name cannot be empty");
    }
    if name != a.name {
        bail!("a user name cannot start or end with a space");
    }
    if e.current
        .users
        .iter()
        .any(|u| u.name.eq_ignore_ascii_case(name))
    {
        bail!("there is already a user called {name}");
    }

    let password = a.password.read()?.ok_or_else(|| {
        anyhow::anyhow!("a new account needs --new-password or --new-password-stdin")
    })?;
    check_password(&password, name, e.current.security.password_restrictions)?;

    let ports = e.port_count();
    let targets = parse_port_list(&a.targets, ports, "KVM")?;
    let virtual_media = parse_port_list(&a.virtual_media, ports, "KVM")?;
    let serial = parse_port_list(&a.serial, e.serial_count(), "serial")?;

    let mut user = b070_mgmt::xml::Element::new("user");
    user.set("name", name)
        .set("password", password)
        .set_bool("active", !a.blocked)
        .set_bool("admin", a.admin)
        .set(
            "userlevel",
            if a.admin {
                USER_LEVEL_ADMIN
            } else {
                USER_LEVEL_USER
            },
        )
        .set("target_ports", targets)
        .set("vmtarget_ports", virtual_media)
        .set("starget_ports", serial);
    e.doc.users().push(user);

    e.note(format!(
        "new {} account {name}",
        if a.admin { "administrator" } else { "user" }
    ));
    e.commit(client)
}

pub fn user_remove(client: &Client, name: &str) -> Result<()> {
    let mut e = Edit::open(client)?;

    // Removing the last administrator would lock everyone out for good.
    let admins = e.current.users.iter().filter(|u| u.admin).count();
    let target = e
        .current
        .users
        .iter()
        .find(|u| u.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("no such user: {name}"))?;
    if target.admin && admins <= 1 {
        bail!(
            "{} is the only administrator; the device would have no way in",
            target.name
        );
    }

    e.doc.users().children.retain(|n| {
        !matches!(
            n,
            b070_mgmt::xml::Node::Element(el)
                if el.name == "user" && el.attr_or_empty("name").eq_ignore_ascii_case(name)
        )
    });
    e.note(format!("{name} removed"));
    e.commit(client)
}

#[derive(Args)]
pub struct UserSetArgs {
    /// Login name of an existing account.
    pub name: String,

    /// Rename the account. The password and permissions come with it: the
    /// vendor's own dialog renames without resending a password, so the device
    /// carries it across.
    #[arg(long, value_name = "NAME")]
    pub rename: Option<String>,

    /// Administrative rights.
    #[arg(long, value_name = "on|off")]
    pub admin: Option<OnOff>,

    /// Whether the account may log in.
    #[arg(long, value_name = "on|off")]
    pub active: Option<OnOff>,

    /// KVM ports the user may reach: `all`, `none`, or e.g. `1,3-5`.
    #[arg(long, value_name = "PORTS")]
    pub targets: Option<String>,

    /// Ports the user may mount virtual media on.
    #[arg(long, value_name = "PORTS")]
    pub virtual_media: Option<String>,

    /// Serial ports the user may reach.
    #[arg(long, value_name = "PORTS")]
    pub serial: Option<String>,
}

pub fn user_set(client: &Client, a: &UserSetArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    let existing = e
        .current
        .users
        .iter()
        .find(|u| u.name.eq_ignore_ascii_case(&a.name))
        .ok_or_else(|| anyhow::anyhow!("no such user: {}", a.name))?
        .clone();

    if let Some(new_name) = a.rename.as_deref() {
        if new_name.trim().is_empty() {
            bail!("the new user name cannot be empty");
        }
        if new_name != new_name.trim() {
            bail!("a user name cannot start or end with a space");
        }
        if !new_name.eq_ignore_ascii_case(&existing.name)
            && e.current
                .users
                .iter()
                .any(|u| u.name.eq_ignore_ascii_case(new_name))
        {
            bail!("there is already a user called {new_name}");
        }
        e.doc.user(&a.name)?.set("name", new_name);
        e.note(format!("{} renamed to {new_name}", existing.name));
    }

    // Later edits have to find the account under whatever it is called now.
    let name: &str = a.rename.as_deref().unwrap_or(&a.name);

    if let Some(v) = a.admin {
        let admin: bool = v.into();
        if !admin && existing.admin && e.current.users.iter().filter(|u| u.admin).count() <= 1 {
            bail!(
                "{} is the only administrator; the device would have no way in",
                existing.name
            );
        }
        e.doc.user(name)?.set(
            "userlevel",
            if admin {
                USER_LEVEL_ADMIN
            } else {
                USER_LEVEL_USER
            },
        );
        // The device sends `admin` alongside `userlevel`; keep them agreeing.
        e.doc.user(name)?.set_bool("admin", admin);
        e.note(format!(
            "{name} is {}",
            if admin { "an administrator" } else { "a user" }
        ));
    }

    if let Some(v) = a.active {
        e.doc.user(name)?.set_bool("active", v.into());
        e.note(format!(
            "{name} is {}",
            if bool::from(v) { "active" } else { "blocked" }
        ));
    }

    let ports = e.port_count();
    let serial_ports = e.serial_count();
    for (label, value, attr, count, kind) in [
        ("KVM ports", &a.targets, "target_ports", ports, "KVM"),
        (
            "virtual media ports",
            &a.virtual_media,
            "vmtarget_ports",
            ports,
            "KVM",
        ),
        (
            "serial ports",
            &a.serial,
            "starget_ports",
            serial_ports,
            "serial",
        ),
    ] {
        if let Some(spec) = value.as_deref() {
            let list = parse_port_list(spec, count, kind)?;
            e.doc.user(name)?.set(attr, list.clone());
            e.note(format!(
                "{name} {label} = {}",
                if list.is_empty() { "none" } else { &list }
            ));
        }
    }

    e.commit(client)
}

#[derive(Args)]
pub struct UserPasswordArgs {
    /// Login name of an existing account.
    pub name: String,

    #[command(flatten)]
    pub password: PasswordArgs,
}

pub fn user_password(client: &Client, a: &UserPasswordArgs) -> Result<()> {
    let mut e = Edit::open(client)?;
    let name = a.name.as_str();

    let existing = e
        .current
        .users
        .iter()
        .find(|u| u.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("no such user: {name}"))?
        .name
        .clone();

    let password = a
        .password
        .read()?
        .ok_or_else(|| anyhow::anyhow!("give it with --new-password or --new-password-stdin"))?;
    check_password(
        &password,
        &existing,
        e.current.security.password_restrictions,
    )?;

    // A password only exists on the wire when it is being changed: the device
    // never sends one back, and an absent attribute leaves it alone.
    e.doc.user(name)?.set("password", password);
    e.note(format!("{existing}'s password"));
    e.commit(client)
}

// ----------------------------------------------------------------- power ---

#[derive(Args)]
pub struct PduAddArgs {
    /// A name for this power strip.
    pub name: String,

    /// Model, exactly as `b070 pdu models` lists it.
    #[arg(long)]
    pub model: String,

    /// Address the device reaches it at over SNMP.
    #[arg(long, value_name = "ADDRESS")]
    pub address: String,
}

pub fn pdu_add(client: &Client, a: &PduAddArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    let name = a.name.trim();
    if name.is_empty() {
        bail!("the power strip needs a name");
    }
    if e.current
        .power
        .pdus
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(name))
    {
        bail!("there is already a power strip called {name}");
    }

    let model = e
        .current
        .power
        .types
        .iter()
        .find(|t| t.model == a.model)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this firmware does not know the model {:?}; `b070 pdu models` lists what it does",
                a.model
            )
        })?
        .clone();

    let mut pdu = b070_mgmt::xml::Element::new("pdu");
    pdu.set("name", name)
        .set("model", &model.model)
        .set(
            "outlets",
            model.outlets.map_or(String::new(), |n| n.to_string()),
        )
        .set("ip", a.address.trim());
    e.doc.power().push(pdu);

    e.note(format!(
        "power strip {name} ({}) at {}",
        model.model,
        a.address.trim()
    ));
    e.commit(client)
}

pub fn pdu_remove(client: &Client, name: &str) -> Result<()> {
    let mut e = Edit::open(client)?;
    let before = e.doc.power().children_named("pdu").count();
    e.doc.power().children.retain(|n| {
        !matches!(
            n,
            b070_mgmt::xml::Node::Element(el)
                if el.name == "pdu" && el.attr_or_empty("name").eq_ignore_ascii_case(name)
        )
    });
    if e.doc.power().children_named("pdu").count() == before {
        bail!("no power strip called {name}");
    }
    e.note(format!("power strip {name} removed"));
    e.commit(client)
}

#[derive(Args)]
pub struct PduOutletArgs {
    /// Name of the power strip.
    pub pdu: String,

    /// Outlet number on that strip.
    pub outlet: u32,

    /// KVM port the outlet powers. Omit to clear the mapping.
    #[arg(long, value_name = "PORT", conflicts_with = "clear")]
    pub target: Option<u32>,

    /// Leave the outlet unassigned.
    #[arg(long)]
    pub clear: bool,
}

pub fn pdu_outlet(client: &Client, a: &PduOutletArgs) -> Result<()> {
    let mut e = Edit::open(client)?;

    let known = e
        .current
        .power
        .pdus
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(&a.pdu))
        .ok_or_else(|| anyhow::anyhow!("no power strip called {}", a.pdu))?
        .clone();
    if let Some(count) = known.outlets {
        if a.outlet == 0 || a.outlet > count {
            bail!("{} has outlets 1 to {count}", known.name);
        }
    }
    if let Some(port) = a.target {
        if !e.current.targets.iter().any(|t| t.port == port) {
            bail!("this device has no KVM port {port}");
        }
        if let Some(other) = known
            .outlet_map
            .iter()
            .find(|o| o.target_port == port && o.outlet != a.outlet)
        {
            bail!(
                "outlet {} on {} already powers port {port}",
                other.outlet,
                known.name
            );
        }
    }

    let outlet_key = a.outlet.to_string();
    let pdu = e
        .doc
        .power()
        .child_where_mut("pdu", "name", &known.name)
        .ok_or_else(|| anyhow::anyhow!("no power strip called {}", a.pdu))?;

    match a.target {
        Some(port) => {
            match pdu.child_where_mut("outlet", "port", &outlet_key) {
                Some(o) => {
                    o.set("target_port", port.to_string());
                }
                None => {
                    let mut o = b070_mgmt::xml::Element::new("outlet");
                    o.set("port", &outlet_key)
                        .set("target_port", port.to_string());
                    pdu.push(o);
                }
            }
            e.note(format!(
                "outlet {} on {} powers KVM port {port}",
                a.outlet, known.name
            ));
        }
        None => {
            if !a.clear {
                bail!("give --target PORT to assign the outlet, or --clear to unassign it");
            }
            let before = pdu.children_named("outlet").count();
            pdu.children.retain(|n| {
                !matches!(
                    n,
                    b070_mgmt::xml::Node::Element(el)
                        if el.name == "outlet" && el.attr("port") == Some(outlet_key.as_str())
                )
            });
            if pdu.children_named("outlet").count() == before {
                bail!("outlet {} on {} is not assigned", a.outlet, known.name);
            }
            e.note(format!("outlet {} on {} unassigned", a.outlet, known.name));
        }
    }

    e.commit(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_stored_port_grant() {
        assert!(grants_port(Some("1,2"), 2));
        assert!(!grants_port(Some("1"), 2));
        assert!(!grants_port(None, 2));
        // A trailing comma, as the firmware writes it, is not a port.
        assert!(grants_port(Some("1,2,"), 2));
        // "1" must not match inside "11".
        assert!(!grants_port(Some("11"), 1));
    }

    #[test]
    fn password_rules_match_the_vendor_dialog() {
        assert!(check_password("correcthorse", "alice", false).is_ok());
        assert!(check_password("short", "alice", false).is_err());
        assert!(check_password("", "alice", false).is_err());
        assert!(check_password(" leading", "alice", false).is_err());
        assert!(check_password("has<angle>", "alice", false).is_err());
        // The user name must not appear anywhere in the password.
        assert!(check_password("xxalicexx", "alice", false).is_err());
        assert!(check_password("xxALICExx", "alice", false).is_err());
    }

    #[test]
    fn strict_rules_ask_for_more() {
        assert!(check_password("correcthorse", "alice", true).is_err());
        assert!(check_password("Correct1horse", "alice", true).is_ok());
        assert!(check_password("Corr1ct", "alice", true).is_err());
    }
}
