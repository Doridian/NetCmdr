// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Parsed representations of the device's `PV_*` management XML.
//!
//! Schema from live captures (firmware 2.1.1184 and 2.2.1258.1.0) in
//! `research/notes/analysis/live-fixtures/`.
//!
//! Parsing is lenient (unknown content is ignored), and strings are trimmed
//! because the firmware adds stray newlines (`name="D1214403\n"`).

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::config::{AuthMethod, SerialAssign};
use crate::Error;
use crate::{SiuUpgradeStatus, SiuVersion, UpgradeStatus};

fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Collect an element's attributes as `(lowercased-name, trimmed-value)` pairs.
fn attrs_of(e: &quick_xml::events::BytesStart<'_>) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    for a in e.attributes().with_checks(false) {
        let a = a?;
        out.push((
            String::from_utf8_lossy(local_name(a.key.as_ref())).to_ascii_lowercase(),
            a.unescape_value()?.trim().to_string(),
        ));
    }
    Ok(out)
}

fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// `"true"`/`"1"` → true; else false.
fn as_bool(v: &str) -> bool {
    v.eq_ignore_ascii_case("true") || v == "1"
}

/// Run `f` for the first element whose local name matches `want`.
fn on_first<F>(xml: &str, want: &str, mut f: F) -> Result<(), Error>
where
    F: FnMut(&[(String, String)]) -> Result<(), Error>,
{
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e)
                if local_name(e.name().as_ref()) == want.as_bytes() =>
            {
                f(&attrs_of(e)?)?;
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Run `f` for every element whose local name matches `want`.
fn on_each<F>(xml: &str, want: &str, mut f: F) -> Result<(), Error>
where
    F: FnMut(&[(String, String)]) -> Result<(), Error>,
{
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e)
                if local_name(e.name().as_ref()) == want.as_bytes() =>
            {
                f(&attrs_of(e)?)?;
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// `CMA_AUTHORIZE` reply (`PV_REPLY` / `PV_reply`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthReply {
    /// `"ok"` or `"error"`.
    pub status: String,
    /// Only on success; also used by the KVM session handshake.
    pub session_id: Option<String>,
    /// AAA scheme in force (`SESSION_AAA_TYPE`); `3` is local auth.
    pub aaa_type: Option<String>,
    /// Hardware family code, e.g. `108`.
    pub hardware: Option<String>,
    /// KVM session port.
    pub port: Option<u16>,
    pub has_fast_vm: bool,
    pub has_ps_redundancy: bool,
    pub has_giga_ethernet: bool,
    /// On failure: the `reply_error` text and id.
    pub error: Option<(String, String)>,
}

impl AuthReply {
    pub fn is_ok(&self) -> bool {
        self.status.eq_ignore_ascii_case("ok")
    }
}

pub fn parse_auth_reply(xml: &str) -> Result<AuthReply, Error> {
    let mut r = AuthReply::default();
    on_first(xml, "PV_reply", |a| {
        r.status = get(a, "status").unwrap_or_default().to_owned();
        r.session_id = get(a, "sessionid").map(str::to_owned);
        r.aaa_type = get(a, "session_aaa_type").map(str::to_owned);
        r.hardware = get(a, "hardware").map(str::to_owned);
        r.port = get(a, "port").and_then(|v| v.parse().ok());
        r.has_fast_vm = get(a, "hasfastvm").map(as_bool).unwrap_or(false);
        r.has_ps_redundancy = get(a, "haspsredundancy").map(as_bool).unwrap_or(false);
        r.has_giga_ethernet = get(a, "hasgigaethernet").map(as_bool).unwrap_or(false);
        Ok(())
    })?;
    on_first(xml, "reply_error", |a| {
        let desc = get(a, "description").unwrap_or_default();
        let id = get(a, "id").unwrap_or_default();
        // 1000 means "no error".
        if id != "1000" && !desc.is_empty() {
            r.error = Some((desc.to_owned(), id.to_owned()));
        }
        Ok(())
    })?;
    Ok(r)
}

/// One KVM port target.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Target {
    pub port: u32,
    pub name: String,
    /// Points of control on a cascaded switch; `0`/absent if none.
    pub num_pocs: Option<u32>,
}

/// Network settings from `<network_adapter>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkAdapter {
    pub name: Option<String>,
    pub mac: Option<String>,
    pub ip: Option<String>,
    pub subnet: Option<String>,
    pub gateway: Option<String>,
    pub dns: Option<String>,
    /// `use_static_ip="false"` means DHCP.
    pub use_static_ip: bool,
    /// Use `dns` instead of DHCP's.
    pub static_dns: bool,
    pub ipv6_enabled: bool,
    pub ipv6: Option<String>,
    /// Prefix length in bits.
    pub ipv6_prefix: Option<u32>,
    pub ipv6_gateway: Option<String>,
    /// `dhcp`, `stateless`, `static` or `none`.
    pub ipv6_mode: Option<String>,
    pub ipv6_dns: Option<String>,
    pub ipv6_static_dns: bool,
}

/// Serial-over-SSH passthrough, from `<SSH>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ssh {
    pub enabled: bool,
    /// Port for serial device 1.
    pub port1: Option<u16>,
    /// Port for serial device 2.
    pub port2: Option<u16>,
}

/// Login policy, from `<security_policy>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SecurityPolicy {
    /// "High security" password rules.
    pub password_restrictions: bool,
    /// Lock out until an admin clears it.
    pub lock_forever: bool,
    /// Lockout duration in seconds (ignored with `lock_forever`).
    pub lock_time_secs: Option<u32>,
    /// Require the OSD password on the local console.
    pub osd_password: bool,
    /// Failed logins that trigger a lockout.
    pub max_try: Option<u32>,
    /// Failure window in seconds.
    pub try_time_secs: Option<u32>,
    /// Authentication sources in order.
    pub auth_order: Vec<AuthMethod>,
}

/// One LDAP server, from `<aaa>/<ldap>/<server>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LdapServer {
    pub host: String,
    pub port: Option<u16>,
    /// `ldap`, `ldaps` or `tls`.
    pub protocol: Option<String>,
}

/// LDAP settings, from `<aaa>/<ldap>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ldap {
    /// `basic`, `user` or `group`.
    pub mode: Option<String>,
    /// Bind DN.
    pub search_dn: Option<String>,
    /// Stored and returned in the clear.
    pub search_password: Option<String>,
    pub search_base: Option<String>,
    /// Attribute holding the login name, e.g. `cn` or `sAMAccountName`.
    pub user_attribute: Option<String>,
    /// Permissions attribute (`user` and `group` modes).
    pub acl_attribute: Option<String>,
    pub servers: Vec<LdapServer>,
}

/// One RADIUS server, from `<aaa>/<radius>/<server>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RadiusServer {
    pub host: String,
    pub port: Option<u16>,
    pub accounting_port: Option<u16>,
    /// Returned in the clear.
    pub secret: Option<String>,
}

/// RADIUS settings, from `<aaa>/<radius>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Radius {
    pub realm: Option<String>,
    pub accounting: bool,
    pub servers: Vec<RadiusServer>,
}

/// External authentication, from `<aaa>` (absent until configured).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Aaa {
    pub ldap: Ldap,
    pub radius: Radius,
}

/// The device clock, from `<system_time>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Central-management appliance, from `<kvm_manager>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KvmManager {
    pub enabled: bool,
    /// Use `ip` instead of discovery.
    pub fixed_ip: bool,
    pub ip: Option<String>,
}

/// One serial device, from `<serial_devices>/<serial_device>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SerialDevice {
    pub port: u32,
    pub name: String,
    pub active: bool,
    pub baud_rate: Option<String>,
    pub data_bits: Option<String>,
    pub stop_bits: Option<String>,
    /// `N`, `O`, `E`, `M` or `S`.
    pub parity: Option<String>,
    /// Console or power strip.
    pub assign: Option<SerialAssign>,
    pub charset: Option<String>,
}

/// SNMP trap settings, from `<snmp>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snmp {
    pub traps_enabled: bool,
    /// Trap destination; empty disables traps regardless of the flag.
    pub manager_ip: Option<String>,
    pub community: Option<String>,
}

/// A supported PDU model, from `<power>/<pdu_types>/<entry>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PduType {
    pub manufacturer: String,
    /// Value for `<pdu model>`.
    pub model: String,
    pub outlets: Option<u32>,
    /// Always `snmp` in current firmware.
    pub mode: Option<String>,
}

/// One outlet's mapping, from `<pdu>/<outlet>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outlet {
    pub outlet: u32,
    /// KVM port powered.
    pub target_port: u32,
}

/// A configured power distribution unit, from `<power>/<pdu>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pdu {
    pub name: String,
    /// A [`PduType::model`].
    pub model: Option<String>,
    pub outlets: Option<u32>,
    pub ip: Option<String>,
    pub outlet_map: Vec<Outlet>,
}

/// Power configuration, from `<power>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Power {
    /// Supported models (read-only).
    pub types: Vec<PduType>,
    pub pdus: Vec<Pdu>,
}

/// Read-only device config (`PV_CONFIG`). Lossy; save via
/// [`crate::config::ConfigDoc`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceConfig {
    pub config_id: Option<String>,
    /// e.g. `D1214403`.
    pub name: Option<String>,
    /// Hardware family code, e.g. `108`.
    pub hardware_type: Option<String>,
    pub switch_ports: Option<u32>,
    /// Cascaded analog switch model, if any.
    pub cascade: Option<String>,
    pub idle_time_minutes: Option<u32>,
    /// Raw `<connection ip_port>`.
    pub session_ports: Vec<u16>,
    pub ssh_enabled: bool,
    pub ssh_ports: Vec<u16>,
    pub ssh: Ssh,
    pub security: SecurityPolicy,
    pub aaa: Aaa,
    pub system_time: Option<DeviceTime>,
    pub kvm_manager: KvmManager,
    pub serial_devices: Vec<SerialDevice>,
    pub network: NetworkAdapter,
    pub snmp: Snmp,
    pub users: Vec<User>,
    pub targets: Vec<Target>,
    pub power: Power,
}

/// A local user account from `<users>/<user>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct User {
    pub name: String,
    pub active: bool,
    pub admin: bool,
    pub user_level: Option<u32>,
    /// e.g. `"1,2,3,4,5,6,7,8"`.
    pub target_ports: Option<String>,
    /// Virtual media ports.
    pub vm_target_ports: Option<String>,
    pub serial_target_ports: Option<String>,
}

/// Parse a `PV_CONFIG` document.
pub fn parse_config(xml: &str) -> Result<DeviceConfig, Error> {
    let Some(data) = crate::xml::find_element(xml, "config_data")? else {
        return Ok(DeviceConfig::default());
    };
    let Some(pv) = data.child("proxyview") else {
        return Ok(DeviceConfig::default());
    };

    let mut c = DeviceConfig {
        config_id: opt(data.attr("config_id")),
        name: opt(pv.attr("name")),
        hardware_type: opt(pv.attr("type")),
        switch_ports: pv.attr_parsed("switch_ports"),
        cascade: opt(pv.attr("switch")),
        ..DeviceConfig::default()
    };

    if let Some(e) = pv.child("connection") {
        c.idle_time_minutes = e.attr_parsed("idle_time");
        c.session_ports = split_ports(e.attr_or_empty("ip_port"));
    }

    if let Some(e) = pv.child("SSH") {
        c.ssh = Ssh {
            enabled: e.attr_bool("enable"),
            port1: e.attr_parsed("port1"),
            port2: e.attr_parsed("port2"),
        };
        // Predate the `ssh` field.
        c.ssh_enabled = c.ssh.enabled;
        c.ssh_ports = [c.ssh.port1, c.ssh.port2].into_iter().flatten().collect();
    }

    if let Some(e) = pv.child("security_policy") {
        c.security = SecurityPolicy {
            password_restrictions: e.attr_bool("pw_restrictions"),
            lock_forever: e.attr_bool("forever"),
            lock_time_secs: e.attr_parsed("lock_time"),
            osd_password: e.attr_bool("osd_password"),
            max_try: e.attr_parsed("max_try"),
            try_time_secs: e.attr_parsed("try_time"),
            auth_order: AuthMethod::parse_order(e.attr_or_empty("aaa_order")),
        };
    }

    if let Some(aaa) = pv.child("aaa") {
        if let Some(e) = aaa.child("ldap") {
            c.aaa.ldap = Ldap {
                mode: opt(e.attr("mode")),
                search_dn: opt(e.attr("search_dn")),
                search_password: opt(e.attr("search_passw")),
                search_base: opt(e.attr("search_base")),
                user_attribute: opt(e.attr("user_attrib")),
                acl_attribute: opt(e.attr("acl_attrib")),
                servers: e
                    .children_named("server")
                    .map(|s| LdapServer {
                        host: s.attr_or_empty("host").to_owned(),
                        port: s.attr_parsed("port"),
                        protocol: opt(s.attr("proto")),
                    })
                    .collect(),
            };
        }
        if let Some(e) = aaa.child("radius") {
            c.aaa.radius = Radius {
                realm: opt(e.attr("realm")),
                accounting: e.attr_bool("accounting"),
                servers: e
                    .children_named("server")
                    .map(|s| RadiusServer {
                        host: s.attr_or_empty("host").to_owned(),
                        port: s.attr_parsed("port"),
                        accounting_port: s.attr_parsed("acc_port"),
                        secret: opt(s.attr("secret")),
                    })
                    .collect(),
            };
        }
    }

    if let Some(e) = pv.child("system_time") {
        if let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
            e.attr_parsed("year"),
            e.attr_parsed("month"),
            e.attr_parsed("day"),
            e.attr_parsed("hour"),
            e.attr_parsed("min"),
            e.attr_parsed("sec"),
        ) {
            c.system_time = Some(DeviceTime {
                year,
                month,
                day,
                hour,
                minute,
                second,
            });
        }
    }

    if let Some(e) = pv.child("kvm_manager") {
        c.kvm_manager = KvmManager {
            enabled: e.attr_bool("enable"),
            fixed_ip: e.attr_bool("fixed_ip"),
            ip: opt(e.attr("ip")),
        };
    }

    if let Some(list) = pv.child("serial_devices") {
        c.serial_devices = list
            .children_named("serial_device")
            .map(|e| SerialDevice {
                port: e.attr_parsed("port").unwrap_or(0),
                name: e.attr_or_empty("name").to_owned(),
                active: e.attr_bool("active"),
                baud_rate: opt(e.attr("baud_rate")),
                data_bits: opt(e.attr("data_bits")),
                stop_bits: opt(e.attr("stop_bits")),
                parity: opt(e.attr("parity")),
                assign: e.attr("PDU").and_then(SerialAssign::from_code),
                charset: opt(e.attr("charset")),
            })
            .collect();
    }

    if let Some(e) = pv
        .child("network_adapters")
        .and_then(|l| l.child("network_adapter"))
    {
        c.network = NetworkAdapter {
            name: opt(e.attr("name")),
            mac: opt(e.attr("mac")),
            ip: opt(e.attr("ip")),
            subnet: opt(e.attr("subnet")),
            gateway: opt(e.attr("gateway")),
            dns: opt(e.attr("dns")),
            use_static_ip: e.attr_bool("use_static_ip"),
            static_dns: e.attr_bool("staticdns"),
            ipv6_enabled: e.attr_bool("enableipv6"),
            ipv6: opt(e.attr("ipv6")),
            ipv6_prefix: e.attr_parsed("subnetv6"),
            ipv6_gateway: opt(e.attr("gatewayv6")),
            ipv6_mode: opt(e.attr("autoconfigv6")),
            ipv6_dns: opt(e.attr("dnsv6")),
            ipv6_static_dns: e.attr_bool("staticdnsv6"),
        };
    }

    if let Some(e) = pv.child("snmp") {
        c.snmp = Snmp {
            traps_enabled: e.attr_bool("enable_traps"),
            manager_ip: opt(e.attr("mngr_ip")),
            community: opt(e.attr("community")),
        };
    }

    if let Some(list) = pv.child("users") {
        c.users = list
            .children_named("user")
            .filter(|e| e.attr("name").is_some())
            .map(|e| User {
                name: e.attr_or_empty("name").to_owned(),
                active: e.attr_bool("active"),
                admin: e.attr_bool("admin"),
                user_level: e.attr_parsed("userlevel"),
                target_ports: opt(e.attr("target_ports")),
                vm_target_ports: opt(e.attr("vmtarget_ports")),
                serial_target_ports: opt(e.attr("starget_ports")),
            })
            .collect();
    }

    if let Some(list) = pv.child("targets") {
        c.targets = list
            .children_named("target")
            .filter_map(|e| {
                Some(Target {
                    port: e.attr_parsed("port")?,
                    name: e.attr("name")?.to_owned(),
                    num_pocs: e.attr_parsed("numpocs"),
                })
            })
            .collect();
    }

    if let Some(power) = pv.child("power") {
        if let Some(types) = power.child("pdu_types") {
            c.power.types = types
                .children_named("entry")
                .map(|e| PduType {
                    manufacturer: e.attr_or_empty("manufacturer").to_owned(),
                    model: e.attr_or_empty("model").to_owned(),
                    outlets: e.attr_parsed("outlets"),
                    mode: opt(e.attr("mode")),
                })
                .collect();
        }
        c.power.pdus = power
            .children_named("pdu")
            .map(|e| Pdu {
                name: e.attr_or_empty("name").to_owned(),
                model: opt(e.attr("model")),
                outlets: e.attr_parsed("outlets"),
                ip: opt(e.attr("ip")),
                outlet_map: e
                    .children_named("outlet")
                    .filter_map(|o| {
                        Some(Outlet {
                            outlet: o.attr_parsed("port")?,
                            target_port: o.attr_parsed("target_port")?,
                        })
                    })
                    .collect(),
            })
            .collect();
    }

    Ok(c)
}

/// `None` only if absent; empty is meaningful (`mngr_ip=""`).
fn opt(v: Option<&str>) -> Option<String> {
    v.map(str::to_owned)
}

fn split_ports(v: &str) -> Vec<u16> {
    v.split(',').filter_map(|s| s.trim().parse().ok()).collect()
}

/// State of one port from a `PV_STATUS` `<status_list>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortStatus {
    pub number: u32,
    pub name: String,
    /// `"kvm"`, `"telnet"`, …
    pub kind: String,
    /// e.g. `"Available"`, `"Off"`, `"Busy"`.
    pub status: String,
    /// SIU type: `"VMROCC"` (with virtual media) or `"none"`.
    pub rocc_type: String,
    pub user: String,
    pub power: bool,
    pub vm_active: bool,
}

impl PortStatus {
    pub fn has_siu(&self) -> bool {
        !self.rocc_type.eq_ignore_ascii_case("none")
            && !self.rocc_type.eq_ignore_ascii_case("NotScanned")
    }
}

/// Device status, from a `value="status"` request (`PV_STATUS`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub name: Option<String>,
    /// Overall health, e.g. `"Operational, no faults"`.
    pub description: Option<String>,
    pub severity: Option<String>,
    pub firmware_version: Option<String>,
    pub hardware: Option<String>,
    pub lan_active: Option<String>,
    pub ports: Vec<PortStatus>,
}

/// Parse a `PV_STATUS` document.
pub fn parse_status(xml: &str) -> Result<Status, Error> {
    let mut s = Status::default();

    on_first(xml, "PV_status", |a| {
        s.name = get(a, "name").map(str::to_owned);
        Ok(())
    })?;
    on_first(xml, "code", |a| {
        s.description = get(a, "description").map(str::to_owned);
        s.severity = get(a, "severity").map(str::to_owned);
        s.lan_active = get(a, "lan").map(str::to_owned);
        Ok(())
    })?;
    on_first(xml, "version", |a| {
        // Collapse embedded newlines.
        s.firmware_version =
            get(a, "firmware").map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "));
        s.hardware = get(a, "hardware").map(str::to_owned);
        Ok(())
    })?;

    // Both <status_list> and <telnet_status> have <port>s; `kind` tells them
    // apart.
    on_each(xml, "port", |a| {
        if let Some(number) = get(a, "number").and_then(|v| v.parse().ok()) {
            s.ports.push(PortStatus {
                number,
                name: get(a, "name").unwrap_or_default().to_owned(),
                kind: get(a, "type").unwrap_or_default().to_owned(),
                status: get(a, "status").unwrap_or_default().to_owned(),
                rocc_type: get(a, "rocctype").unwrap_or_default().to_owned(),
                user: get(a, "user").unwrap_or_default().to_owned(),
                power: get(a, "power").map(as_bool).unwrap_or(false),
                vm_active: get(a, "vmactive").map(as_bool).unwrap_or(false),
            });
        }
        Ok(())
    })?;

    Ok(s)
}

/// Whether a `CMA_CONFIG` reply says the device is rebooting.
pub fn reboot_in_progress(xml: &str) -> bool {
    matches!(
        crate::xml::find_element(xml, "PV_reply"),
        Ok(Some(ref e)) if e.attr_bool("need_boot")
    )
}

/// `<reply_data>` payload (a backup).
pub fn reply_data(xml: &str) -> Option<String> {
    let e = crate::xml::find_element(xml, "reply_data").ok()??;
    Some(e.text())
}

/// Firmware upgrade progress from a `status` reply.
pub fn parse_upgrade_status(xml: &str) -> Result<UpgradeStatus, Error> {
    let e = crate::xml::find_element(xml, "device_upgrade")?
        .ok_or_else(|| Error::Device("no upgrade in progress".into()))?;
    Ok(UpgradeStatus {
        status: e.attr_or_empty("upgrade_status").to_owned(),
        percent_complete: e.attr_parsed("percent_complete"),
    })
}

pub fn parse_siu_upgrade_status(xml: &str) -> Result<Vec<SiuUpgradeStatus>, Error> {
    let Some(data) = crate::xml::find_element(xml, "config_data")? else {
        return Ok(Vec::new());
    };
    Ok(data
        .children_named("target")
        .filter_map(|e| {
            Some(SiuUpgradeStatus {
                port: e.attr_parsed("port")?,
                status: e.attr_or_empty("status").to_owned(),
                percent_complete: e.attr_parsed("percent"),
            })
        })
        .collect())
}

/// Parse the SIU version reply (`roc_hw`/`roc_fw`).
pub fn parse_siu_versions(xml: &str) -> Result<Vec<SiuVersion>, Error> {
    let Some(status) = crate::xml::find_element(xml, "PV_status")? else {
        return Ok(Vec::new());
    };
    let list = status.child("status_list").unwrap_or(&status);
    Ok(list
        .children_named("port")
        .filter_map(|e| {
            Some(SiuVersion {
                port: e.attr_parsed("number")?,
                hardware: siu_field(e.attr("roc_hw")),
                firmware: siu_field(e.attr("roc_fw")),
            })
        })
        .collect())
}

/// `"Not responding"` means no SIU.
fn siu_field(v: Option<&str>) -> Option<String> {
    match v {
        Some(v) if !v.is_empty() && !v.eq_ignore_ascii_case("Not responding") => Some(v.to_owned()),
        _ => None,
    }
}

/// The `reply_error` description, if any (e.g. "Invalid session").
pub fn error_in(xml: &str) -> Option<String> {
    let mut found = None;
    let _ = on_first(xml, "reply_error", |a| {
        let id = get(a, "id").unwrap_or_default();
        let desc = get(a, "description").unwrap_or_default();
        if id != "1000" && !desc.is_empty() {
            found = Some(desc.to_owned());
        }
        Ok(())
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTH_OK: &str =
        include_str!("../../../research/notes/analysis/live-fixtures/auth_reply.xml");
    const CONFIG: &str =
        include_str!("../../../research/notes/analysis/live-fixtures/config_full.xml");
    const STATUS: &str = include_str!("../../../research/notes/analysis/live-fixtures/status.xml");

    #[test]
    fn parses_live_auth_reply() {
        let r = parse_auth_reply(AUTH_OK).unwrap();
        assert!(r.is_ok());
        assert_eq!(r.session_id.as_deref(), Some("SESSION_TOKEN_REDACTED="));
        assert_eq!(r.hardware.as_deref(), Some("108"));
        assert_eq!(r.port, Some(900));
        assert!(r.has_fast_vm);
        assert!(r.has_giga_ethernet);
        assert_eq!(r.aaa_type.as_deref(), Some("3"));
        assert!(r.error.is_none());
    }

    #[test]
    fn parses_auth_failure() {
        let xml = r#"<PV_Message name="PV_REPLY"><PV_reply status="error" hardware="108" port="900">
            <reply_error description="Access denied" id="32004"/></PV_reply></PV_Message>"#;
        let r = parse_auth_reply(xml).unwrap();
        assert!(!r.is_ok());
        assert_eq!(r.session_id, None);
        assert_eq!(r.error, Some(("Access denied".into(), "32004".into())));
    }

    #[test]
    fn parses_live_config() {
        let c = parse_config(CONFIG).unwrap();
        // Trailing newline must be trimmed.
        assert_eq!(c.name.as_deref(), Some("D1214403"));
        assert_eq!(c.hardware_type.as_deref(), Some("108"));
        assert_eq!(c.switch_ports, Some(8));
        assert_eq!(c.idle_time_minutes, Some(10));
        assert_eq!(c.session_ports, vec![900]);
        assert!(!c.ssh_enabled);
        assert_eq!(c.ssh_ports, vec![4001, 4002]);

        assert!(!c.network.use_static_ip, "device is on DHCP");
        assert_eq!(c.network.ip.as_deref(), Some("192.168.0.10")); // sanitized
        assert!(c.network.ipv6_enabled);

        assert_eq!(c.users.len(), 1);
        assert_eq!(c.users[0].name, "admin");
        assert!(c.users[0].admin);
        assert_eq!(c.users[0].target_ports.as_deref(), Some("1,2,3,4,5,6,7,8"));

        assert_eq!(c.security.max_try, Some(5));
        assert_eq!(c.security.try_time_secs, Some(180));
        assert_eq!(c.security.lock_time_secs, Some(1800));
        assert!(!c.security.lock_forever);
        assert_eq!(c.security.auth_order, [crate::AuthMethod::Local]);

        // This firmware omits <aaa> entirely until it is configured.
        assert_eq!(c.aaa, Aaa::default());

        assert_eq!(
            c.system_time,
            Some(DeviceTime {
                year: 2026,
                month: 9,
                day: 18,
                hour: 23,
                minute: 31,
                second: 37,
            })
        );

        assert_eq!(c.serial_devices.len(), 2);
        assert_eq!(c.serial_devices[0].name, "Telnet 01");
        assert_eq!(c.serial_devices[0].baud_rate.as_deref(), Some("9600"));
        assert_eq!(c.serial_devices[0].parity.as_deref(), Some("N"));
        // This firmware omits @PDU entirely when nothing is wired to the port.
        assert_eq!(c.serial_devices[0].assign, None);

        assert!(c.network.ipv6_enabled);
        assert_eq!(c.network.ipv6_mode.as_deref(), Some("dhcp"));
        assert_eq!(c.network.ipv6_prefix, Some(64));
        assert!(!c.network.static_dns);

        assert!(!c.snmp.traps_enabled);
        assert_eq!(c.snmp.manager_ip.as_deref(), Some(""));

        assert_eq!(
            c.users[0].vm_target_ports.as_deref(),
            Some("1,2,3,4,5,6,7,8")
        );
        assert_eq!(c.users[0].serial_target_ports.as_deref(), Some("1,2"));

        // The firmware lists every PDU it can drive; none is configured here.
        assert_eq!(c.power.types.len(), 40);
        assert_eq!(c.power.types[0].manufacturer, "Tripp Lite");
        assert!(c.power.pdus.is_empty());

        assert_eq!(c.targets.len(), 8);
        assert_eq!(
            c.targets[0],
            Target {
                port: 1,
                name: "Server 01".into(),
                num_pocs: None,
            }
        );
        assert_eq!(c.targets[7].port, 8);
    }

    #[test]
    fn parses_live_status_and_detects_siu() {
        let s = parse_status(STATUS).unwrap();
        assert_eq!(s.name.as_deref(), Some("D1214403"));
        assert_eq!(s.description.as_deref(), Some("Operational, no faults"));
        assert_eq!(s.severity.as_deref(), Some("OK"));
        assert_eq!(
            s.firmware_version.as_deref(),
            Some("Version 2.1.1184.1.0 .0 Build()")
        );

        // 8 KVM ports + 2 telnet ports.
        let kvm: Vec<_> = s.ports.iter().filter(|p| p.kind == "kvm").collect();
        assert_eq!(kvm.len(), 8);
        // Ports 1 and 2 have SIUs attached (VMROCC); 3-8 are empty.
        assert!(kvm[0].has_siu());
        assert_eq!(kvm[0].rocc_type, "VMROCC");
        assert_eq!(kvm[0].status, "Available");
        assert!(!kvm[2].has_siu());
        assert_eq!(kvm[2].status, "Off");
    }

    #[test]
    fn parses_a_live_siu_version_reply() {
        // One port per reply, whatever was asked.
        let xml = r#"<PV_Message name="PV_STATUS"><PV_status SESSIONID="x"><status_list>
            <port number="1" roc_hw="20373 0.2 20372 2.0 ID00080207"
                  roc_fw="RoC II USB En v4.0 C8 B225 12/03/2013"/>
            </status_list></PV_status></PV_Message>"#;
        let v = parse_siu_versions(xml).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].port, 1);
        assert_eq!(
            v[0].hardware.as_deref(),
            Some("20373 0.2 20372 2.0 ID00080207")
        );
        assert_eq!(
            v[0].firmware.as_deref(),
            Some("RoC II USB En v4.0 C8 B225 12/03/2013")
        );
        assert!(v[0].responding());
    }

    #[test]
    fn an_empty_port_reads_as_no_siu() {
        // The firmware answers "Not responding" rather than omitting the port.
        let xml = r#"<PV_Message name="PV_STATUS"><PV_status><status_list>
            <port number="3" roc_hw="Not responding" roc_fw="Not responding"/>
            </status_list></PV_status></PV_Message>"#;
        let v = parse_siu_versions(xml).unwrap();
        assert_eq!(v[0].port, 3);
        assert_eq!(v[0].hardware, None);
        assert_eq!(v[0].firmware, None);
        assert!(!v[0].responding());
    }

    #[test]
    fn error_in_ignores_no_error() {
        assert_eq!(error_in(AUTH_OK), None);
        let bad =
            r#"<PV_Message><reply_error description="Invalid session" id="32001"/></PV_Message>"#;
        assert_eq!(error_in(bad).as_deref(), Some("Invalid session"));
    }
}
