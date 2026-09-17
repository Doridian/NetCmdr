//! Parsed representations of the device's management XML.
//!
//! Schemas were taken from the firmware's own web UI, which serves reference
//! copies of each document: `research/software/webui-3.0.5.583/config/`
//! (`config.xml`, `admin.xml`, `status.xml`, `SDF.xml`, `log.xml`).
//!
//! The device is generous with attributes and this crate deliberately keeps
//! parsing lenient — unknown elements and attributes are ignored rather than
//! rejected, because firmware revisions add fields.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::Error;

/// Pull every attribute of the *first* element with the given local name.
///
/// Returns `None` if the element never appears.
fn first_element_attrs(xml: &str, want: &str) -> Result<Option<Vec<(String, String)>>, Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf)?;
        match ev {
            Event::Eof => return Ok(None),
            Event::Start(ref e) | Event::Empty(ref e)
                if local_name(e.name().as_ref()) == want.as_bytes() => {
                    let mut out = Vec::new();
                    for a in e.attributes().with_checks(false) {
                        let a = a?;
                        out.push((
                            String::from_utf8_lossy(local_name(a.key.as_ref())).into_owned(),
                            a.unescape_value()?.into_owned(),
                        ));
                    }
                    return Ok(Some(out));
                }
            _ => {}
        }
        buf.clear();
    }
}

/// Strip any namespace prefix.
fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// One KVM port and the name assigned to it.
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
    /// `use_static_ip="false"` means DHCP.
    pub use_static_ip: Option<bool>,
}

/// Session/connection settings from `<connection>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Connection {
    /// Raw `ip_port` attribute. The firmware stores this as a comma-separated
    /// list, e.g. `"900 , 901 , 902"`.
    pub ip_port_raw: Option<String>,
    pub idle_time_minutes: Option<u32>,
    pub client_https_only: Option<bool>,
    pub ldap: Option<bool>,
    pub standalone: Option<bool>,
}

impl Connection {
    /// The session ports, parsed out of [`Self::ip_port_raw`].
    pub fn ports(&self) -> Vec<u16> {
        self.ip_port_raw
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect()
    }
}

/// The device configuration, from a `client=ConfigXML` response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceConfig {
    pub device_description: Option<String>,
    pub switch_model: Option<String>,
    pub switch_ports: Option<u32>,
    pub virtual_media_enabled: Option<bool>,
    pub network: NetworkAdapter,
    pub connection: Connection,
    pub targets: Vec<Target>,
}

/// Parse a `client=ConfigXML` response.
pub fn parse_config(xml: &str) -> Result<DeviceConfig, Error> {
    let mut cfg = DeviceConfig::default();

    if let Some(a) = first_element_attrs(xml, "proxyview")? {
        cfg.device_description = get(&a, "DeviceDescription").map(str::to_owned);
        cfg.switch_model = get(&a, "switch").map(str::to_owned);
        cfg.switch_ports = get(&a, "switch_ports").and_then(|v| v.trim().parse().ok());
        cfg.virtual_media_enabled = get(&a, "USE_VM").map(parse_bool);
    }

    if let Some(a) = first_element_attrs(xml, "network_adapter")? {
        cfg.network = NetworkAdapter {
            name: get(&a, "name").map(str::to_owned),
            mac: get(&a, "mac").map(str::to_owned),
            ip: get(&a, "ip").map(str::to_owned),
            subnet: get(&a, "subnet").map(str::to_owned),
            gateway: get(&a, "gateway").map(str::to_owned),
            use_static_ip: get(&a, "use_static_ip").map(parse_bool),
        };
    }

    if let Some(a) = first_element_attrs(xml, "connection")? {
        cfg.connection = Connection {
            ip_port_raw: get(&a, "ip_port").map(str::to_owned),
            idle_time_minutes: get(&a, "idle_time").and_then(|v| v.trim().parse().ok()),
            client_https_only: get(&a, "client_https_only").map(parse_bool),
            ldap: get(&a, "ldap").map(parse_bool),
            standalone: get(&a, "standalone").map(parse_bool),
        };
    }

    cfg.targets = parse_targets(xml)?;
    Ok(cfg)
}

/// `"1"`, `"true"` and `"TRUE"` all mean true; everything else is false.
///
/// The firmware is inconsistent — `USE_VM="false"`, `client_https_only="0"`,
/// `permit_console_update="FALSE"` all appear in the same document.
fn parse_bool(v: &str) -> bool {
    let v = v.trim();
    v == "1" || v.eq_ignore_ascii_case("true")
}

fn parse_targets(xml: &str) -> Result<Vec<Target>, Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e)
                if local_name(e.name().as_ref()) == b"target" => {
                    let mut port = None;
                    let mut name = None;
                    for a in e.attributes().with_checks(false) {
                        let a = a?;
                        let key = local_name(a.key.as_ref()).to_ascii_lowercase();
                        let val = a.unescape_value()?.into_owned();
                        match key.as_slice() {
                            b"port" => port = val.trim().parse().ok(),
                            b"name" => name = Some(val),
                            _ => {}
                        }
                    }
                    if let Some(port) = port {
                        out.push(Target {
                            port,
                            name: name.unwrap_or_default(),
                        });
                    }
                }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// A local user account, from a `client=AdminXML` response.
///
/// The firmware stores passwords in cleartext in this document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Option<u32>,
    pub username: String,
    pub password: Option<String>,
    /// Vendor-defined permission level. `1` is the shipped admin account.
    pub permission: Option<u32>,
    /// `-1` means all targets.
    pub targets: Option<String>,
}

/// Parse a `client=AdminXML` response.
pub fn parse_users(xml: &str) -> Result<Vec<User>, Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e)
                if local_name(e.name().as_ref()) == b"User" => {
                    let mut attrs = Vec::new();
                    for a in e.attributes().with_checks(false) {
                        let a = a?;
                        attrs.push((
                            String::from_utf8_lossy(local_name(a.key.as_ref())).into_owned(),
                            a.unescape_value()?.into_owned(),
                        ));
                    }
                    out.push(User {
                        id: get(&attrs, "ID").and_then(|v| v.trim().parse().ok()),
                        username: get(&attrs, "UserName").unwrap_or_default().to_owned(),
                        password: get(&attrs, "Password").map(str::to_owned),
                        permission: get(&attrs, "Permission").and_then(|v| v.trim().parse().ok()),
                        targets: get(&attrs, "Targets").map(str::to_owned),
                    });
                }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Device status, from a `client=StatusXML` response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub severity: Option<String>,
    pub firmware_version: Option<String>,
    pub hardware_version: Option<String>,
    pub sdf_version: Option<String>,
    /// `"Local"` or `"Remote"`.
    pub mode: Option<String>,
    pub last_user: Option<String>,
    pub last_target: Option<String>,
    pub last_port: Option<u32>,
    pub last_connection_duration: Option<String>,
}

/// Parse a `client=StatusXML` response.
pub fn parse_status(xml: &str) -> Result<Status, Error> {
    let mut s = Status::default();
    if let Some(a) = first_element_attrs(xml, "code")? {
        s.severity = get(&a, "severity").map(str::to_owned);
    }
    if let Some(a) = first_element_attrs(xml, "version")? {
        s.firmware_version = get(&a, "firmware").map(str::to_owned);
        s.hardware_version = get(&a, "hardware").map(str::to_owned);
        s.sdf_version = get(&a, "sdf").map(str::to_owned);
    }
    if let Some(a) = first_element_attrs(xml, "operation")? {
        s.mode = get(&a, "mode").map(str::to_owned);
        s.last_user = get(&a, "last_user").map(str::to_owned);
        s.last_target = get(&a, "last_target").map(str::to_owned);
        s.last_port = get(&a, "last_port").and_then(|v| v.trim().parse().ok());
        s.last_connection_duration = get(&a, "last_connection_duration").map(str::to_owned);
    }
    Ok(s)
}

/// One entry from the switch definition file (`client=SwitchXML`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchDef {
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub description: Option<String>,
    pub num_ports: Option<u32>,
}

/// Parse a `client=SwitchXML` (SDF) response.
///
/// The document's trailing `<switchData>` CDATA block is encrypted and is
/// ignored here.
pub fn parse_switches(xml: &str) -> Result<Vec<SwitchDef>, Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out: Vec<SwitchDef> = Vec::new();
    let mut depth_in_switch = false;
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = local_name(e.name().as_ref()).to_vec();
                if name == b"switch" {
                    let mut sw_name = String::new();
                    for a in e.attributes().with_checks(false) {
                        let a = a?;
                        if local_name(a.key.as_ref()) == b"name" {
                            sw_name = a.unescape_value()?.into_owned();
                        }
                    }
                    out.push(SwitchDef {
                        name: sw_name,
                        manufacturer: None,
                        model: None,
                        description: None,
                        num_ports: None,
                    });
                    depth_in_switch = true;
                } else if depth_in_switch {
                    // Child elements carry a single value="…" attribute.
                    let mut value = None;
                    for a in e.attributes().with_checks(false) {
                        let a = a?;
                        if local_name(a.key.as_ref()) == b"value" {
                            value = Some(a.unescape_value()?.into_owned());
                        }
                    }
                    if let (Some(v), Some(sw)) = (value, out.last_mut()) {
                        match name.as_slice() {
                            b"manufacturer" => sw.manufacturer = Some(v),
                            b"model" => sw.model = Some(v),
                            b"description" => sw.description = Some(v),
                            b"numOfPorts" => sw.num_ports = v.trim().parse().ok(),
                            _ => {}
                        }
                    }
                }
            }
            Event::End(ref e)
                if local_name(e.name().as_ref()) == b"switch" => {
                    depth_in_switch = false;
                }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim from research/software/webui-3.0.5.583/config/config.xml,
    // trimmed to the elements we parse.
    const CONFIG_XML: &str = r#"
<CMA_Message id="k1" name="K1_CONFIG" number="2000">
  <config_data config_id="00/00/00 00:00:00 0" value="update">
    <proxyview DeviceDescription="Minicom Smart 116 IP" USE_VM="false"
               switch="Minicom Smart 116 IP" switch_id="SWITCH1" switch_ports="16">
      <network_adapters>
        <network_adapter AUTO_IP="192.168.0.155" AUTO_SUBNET="255.255.255.0"
            gateway="255.255.255.255" ip="255.255.255.255" mac="00:00:00:00"
            name="NoName" subnet="255.255.255.255" use_static_ip="false"/>
      </network_adapters>
      <connection client_https_only="0" enable_config_access="0" idle_time="10"
                  ip_port="900 , 901 , 902" ldap="false" secure="false" standalone="true"/>
      <targets>
        <target name="Server1" port="1"/>
        <target name="Server2" port="2"/>
      </targets>
    </proxyview>
  </config_data>
</CMA_Message>"#;

    #[test]
    fn parses_config() {
        let c = parse_config(CONFIG_XML).unwrap();
        assert_eq!(c.device_description.as_deref(), Some("Minicom Smart 116 IP"));
        assert_eq!(c.switch_ports, Some(16));
        assert_eq!(c.virtual_media_enabled, Some(false));
        assert_eq!(c.network.mac.as_deref(), Some("00:00:00:00"));
        assert_eq!(c.network.use_static_ip, Some(false));
        assert_eq!(c.connection.idle_time_minutes, Some(10));
        assert_eq!(c.connection.client_https_only, Some(false));
        assert_eq!(c.connection.ports(), vec![900, 901, 902]);
        assert_eq!(
            c.targets,
            vec![
                Target { port: 1, name: "Server1".into() },
                Target { port: 2, name: "Server2".into() },
            ]
        );
    }

    #[test]
    fn parses_users_including_the_shipped_default() {
        // Verbatim from the firmware's admin.xml.
        let xml = r#"<KOneConfig>
            <KOneUsers PWPolicies="0">
              <User DialupPermit="0" ID="0" Password="access" Permission="1"
                    RasPassword="" Targets="-1" UserName="admin">
                <account blockstatus="0"/>
              </User>
            </KOneUsers>
          </KOneConfig>"#;
        let users = parse_users(xml).unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].username, "admin");
        assert_eq!(users[0].password.as_deref(), Some("access"));
        assert_eq!(users[0].permission, Some(1));
        assert_eq!(users[0].targets.as_deref(), Some("-1"));
    }

    #[test]
    fn parses_status() {
        let xml = r#"<PV_Message id="Config Status" name="PV_STATUS" number="2001">
            <PV_status name="">
              <code config_id="" description="" severity="Ok" value="2001"/>
              <version firmware="Version 3.0.5 Build(583)" hardware="2" kme="" sdf="10.0"/>
              <operation last_connection_duration=" 0 :  0 :  0" last_port="3"
                         last_target="Server3" last_time="" last_user="SYSTEM" mode="Local"/>
            </PV_status>
          </PV_Message>"#;
        let s = parse_status(xml).unwrap();
        assert_eq!(s.severity.as_deref(), Some("Ok"));
        assert_eq!(s.firmware_version.as_deref(), Some("Version 3.0.5 Build(583)"));
        assert_eq!(s.mode.as_deref(), Some("Local"));
        assert_eq!(s.last_port, Some(3));
        assert_eq!(s.last_target.as_deref(), Some("Server3"));
    }

    #[test]
    fn parses_switch_definitions() {
        let xml = r#"<SwitchesList>
            <generalInfo><SDFVersion value="4.012000"/><numOfSwitches value="2"/></generalInfo>
            <switch name="SWITCH0">
              <numOfPorts value="1"/><manufacturer value="(None)"/>
              <description value="No KVM switch"/><model value="(None)"/>
            </switch>
            <switch name="SWITCH1">
              <numOfPorts value="16"/><manufacturer value="Minicom"/>
              <description value="Smart 116 IP"/><model value="Smart 116 IP"/>
            </switch>
          </SwitchesList>"#;
        let sw = parse_switches(xml).unwrap();
        assert_eq!(sw.len(), 2);
        assert_eq!(sw[1].name, "SWITCH1");
        assert_eq!(sw[1].manufacturer.as_deref(), Some("Minicom"));
        assert_eq!(sw[1].num_ports, Some(16));
    }

    #[test]
    fn bool_attributes_accept_every_spelling_the_firmware_uses() {
        assert!(parse_bool("1"));
        assert!(parse_bool("true"));
        assert!(parse_bool("TRUE"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool("false"));
        assert!(!parse_bool("FALSE"));
    }

    #[test]
    fn missing_elements_do_not_fail_the_parse() {
        let c = parse_config("<CMA_Message/>").unwrap();
        assert_eq!(c, DeviceConfig::default());
    }
}
