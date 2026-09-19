//! Parsed representations of the device's `PV_*` management XML.
//!
//! The schema here is from **live captures** of a B070-008-19-IP on firmware
//! 2.1.1184, saved under `research/notes/analysis/live-fixtures/`, and re-checked
//! against firmware 2.2.1258.1.0, which parses identically. The earlier
//! `/config/` `CMA_Message` schema this crate first targeted is gone from
//! shipping firmware.
//!
//! Parsing is deliberately lenient: unknown elements and attributes are ignored
//! so that other hardware variants and firmware revisions still parse. Note the
//! firmware embeds stray newlines inside some attribute values
//! (`name="D1214403\n"`), so every extracted string is trimmed.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::Error;

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

/// `"true"`/`"1"` → true; anything else → false. The firmware uses `"true"`
/// and `"false"` in the PV schema (unlike the older mixed `0`/`FALSE` forms).
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

/// The reply to a `CMA_AUTHORIZE`. `PV_REPLY` / `PV_reply`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthReply {
    /// `"ok"` or `"error"`.
    pub status: String,
    /// Present only on success. This is the token the KVM session's socket
    /// preamble also needs.
    pub session_id: Option<String>,
    /// AAA scheme in force (`SESSION_AAA_TYPE`); `3` is local auth.
    pub aaa_type: Option<String>,
    /// Hardware family code, e.g. `108`.
    pub hardware: Option<String>,
    /// Session port the device wants for the KVM channel.
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

/// Parse a `PV_REPLY` (authentication response).
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
        // id 1000 is "no error"; don't record it as a failure.
        if id != "1000" && !desc.is_empty() {
            r.error = Some((desc.to_owned(), id.to_owned()));
        }
        Ok(())
    })?;
    Ok(r)
}

/// One KVM port target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub port: u32,
    pub name: String,
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
    pub ipv6_enabled: bool,
    pub ipv6: Option<String>,
}

/// The device configuration, from a `value="config"` request (`PV_CONFIG`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceConfig {
    /// Device name, e.g. the serial `D1214403`.
    pub name: Option<String>,
    /// Hardware family code, e.g. `108`.
    pub hardware_type: Option<String>,
    pub switch_ports: Option<u32>,
    pub idle_time_minutes: Option<u32>,
    /// Raw `ip_port` from `<connection>` (session port list).
    pub session_ports: Vec<u16>,
    pub ssh_enabled: bool,
    pub ssh_ports: Vec<u16>,
    pub network: NetworkAdapter,
    pub users: Vec<User>,
    pub targets: Vec<Target>,
}

/// A local user account from `<users>/<user>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub active: bool,
    pub admin: bool,
    pub user_level: Option<u32>,
    /// Ports this user may reach, e.g. `"1,2,3,4,5,6,7,8"`.
    pub target_ports: Option<String>,
}

/// Parse a `PV_CONFIG` document.
pub fn parse_config(xml: &str) -> Result<DeviceConfig, Error> {
    let mut c = DeviceConfig::default();

    on_first(xml, "proxyview", |a| {
        c.name = get(a, "name").map(str::to_owned);
        c.hardware_type = get(a, "type").map(str::to_owned);
        c.switch_ports = get(a, "switch_ports").and_then(|v| v.parse().ok());
        Ok(())
    })?;

    on_first(xml, "connection", |a| {
        c.idle_time_minutes = get(a, "idle_time").and_then(|v| v.parse().ok());
        c.session_ports = get(a, "ip_port")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        Ok(())
    })?;

    on_first(xml, "SSH", |a| {
        c.ssh_enabled = get(a, "enable").map(as_bool).unwrap_or(false);
        c.ssh_ports = ["port1", "port2"]
            .iter()
            .filter_map(|k| get(a, k).and_then(|v| v.parse().ok()))
            .collect();
        Ok(())
    })?;

    on_first(xml, "network_adapter", |a| {
        c.network = NetworkAdapter {
            name: get(a, "name").map(str::to_owned),
            mac: get(a, "mac").map(str::to_owned),
            ip: get(a, "ip").map(str::to_owned),
            subnet: get(a, "subnet").map(str::to_owned),
            gateway: get(a, "gateway").map(str::to_owned),
            dns: get(a, "dns").map(str::to_owned),
            use_static_ip: get(a, "use_static_ip").map(as_bool).unwrap_or(false),
            ipv6_enabled: get(a, "enableipv6").map(as_bool).unwrap_or(false),
            ipv6: get(a, "ipv6").map(str::to_owned),
        };
        Ok(())
    })?;

    on_each(xml, "user", |a| {
        if let Some(name) = get(a, "name") {
            c.users.push(User {
                name: name.to_owned(),
                active: get(a, "active").map(as_bool).unwrap_or(false),
                admin: get(a, "admin").map(as_bool).unwrap_or(false),
                user_level: get(a, "userlevel").and_then(|v| v.parse().ok()),
                target_ports: get(a, "target_ports").map(str::to_owned),
            });
        }
        Ok(())
    })?;

    on_each(xml, "target", |a| {
        if let (Some(port), Some(name)) =
            (get(a, "port").and_then(|v| v.parse().ok()), get(a, "name"))
        {
            c.targets.push(Target {
                port,
                name: name.to_owned(),
            });
        }
        Ok(())
    })?;

    Ok(c)
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
    /// Server Interface Unit type: `"VMROCC"` when an SIU with virtual media is
    /// attached, `"none"` when the port is empty.
    pub rocc_type: String,
    pub user: String,
    pub power: bool,
    pub vm_active: bool,
}

impl PortStatus {
    /// Whether a Server Interface Unit is attached to this port.
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
        // The firmware string has an embedded newline; collapse whitespace.
        s.firmware_version =
            get(a, "firmware").map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "));
        s.hardware = get(a, "hardware").map(str::to_owned);
        Ok(())
    })?;

    // <status_list> and <telnet_status> both contain <port> elements; take them
    // all and let the `kind` attribute distinguish.
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

/// Pull the `reply_error` description out of any PV document, if it signals an
/// error (used to surface "Invalid session" and friends).
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
        // The firmware embeds a trailing newline in the name; must be trimmed.
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

        assert_eq!(c.targets.len(), 8);
        assert_eq!(
            c.targets[0],
            Target {
                port: 1,
                name: "Server 01".into()
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
        // Embedded newline in the firmware version must be collapsed.
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
    fn error_in_ignores_no_error() {
        assert_eq!(error_in(AUTH_OK), None);
        let bad =
            r#"<PV_Message><reply_error description="Invalid session" id="32001"/></PV_Message>"#;
        assert_eq!(error_in(bad).as_deref(), Some("Invalid session"));
    }
}
