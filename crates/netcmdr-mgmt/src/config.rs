// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The editable device configuration: `<config_data>` and its vocabulary.
//!
//! The device has no way to change one setting. Its only write verb,
//! `CMA_CONFIG`, takes the whole `<config_data>` document and applies it, so
//! every change is a read-modify-write of the entire configuration — which is
//! exactly what the vendor's client does (`FetchCfgHttpCmd` → dialogs →
//! `SaveCfgHttpCmd`).
//!
//! [`ConfigDoc`] is that document, held as a lossless [`crate::xml`] tree so
//! that everything this crate has no model for survives the round trip
//! untouched. It offers handles onto the sections a caller wants to change;
//! [`crate::model::DeviceConfig`] is the typed, read-only view of the same
//! document for display.
//!
//! Three things the device's own client does, which a writer has to do too:
//!
//! - **`<system_time>` is a command, not a setting.** The client blanks all six
//!   fields unless the user actually set the clock (`DatePanel.saveUIToPvXml`),
//!   because sending back the time that was read would set the clock to a value
//!   that is by then stale. [`ConfigDoc`] blanks it on save unless
//!   [`ConfigDoc::set_system_time`] was called.
//! - **`<aaa>` may be missing** from what the device sends. `FetchCfgHttpCmd`
//!   inserts a stub when it is, so LDAP and RADIUS always have somewhere to
//!   live; [`ConfigDoc::ldap`] and [`ConfigDoc::radius`] do the same.
//! - **A user's password is write-only.** It never comes back in a fetch, and
//!   setting it means adding a `password` attribute for that one save.
//!
//! Saving is gated by a permission handshake (`CMA_PERMISSION`); see
//! [`crate::Client::save_config`].

use std::fmt;
use std::str::FromStr;

use crate::xml::{self, Element};
use crate::Error;

/// The `<config_data>` document, ready to be edited and saved back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDoc {
    data: Element,
    /// Whether the caller asked to set the device clock in this save.
    set_time: bool,
}

/// Sections of `<proxyview>`, in the order the firmware emits them. Used to
/// place a section the device did not send where the vendor's client puts it.
const SECTION_ORDER: &[&str] = &[
    "connection",
    "SSH",
    "security_policy",
    "aaa",
    "system_time",
    "kvm_manager",
    "serial_devices",
    "network_adapters",
    "snmp",
    "users",
    "targets",
    "power",
];

impl ConfigDoc {
    /// Parse a `PV_CONFIG` reply into its editable `<config_data>`.
    pub fn parse(xml: &str) -> Result<Self, Error> {
        let data = xml::find_element(xml, "config_data")?
            .ok_or_else(|| Error::Device("reply has no <config_data>".into()))?;
        if data.child("proxyview").is_none() {
            return Err(Error::Device("configuration has no <proxyview>".into()));
        }
        Ok(ConfigDoc {
            data,
            set_time: false,
        })
    }

    /// The `config_id` the device stamped this document with.
    pub fn config_id(&self) -> Option<&str> {
        self.data.attr("config_id")
    }

    /// The `<proxyview>` element: the device itself.
    pub fn device(&self) -> &Element {
        self.data.child("proxyview").expect("checked in parse")
    }

    /// The `<proxyview>` element, mutably.
    pub fn device_mut(&mut self) -> &mut Element {
        self.data.child_mut("proxyview").expect("checked in parse")
    }

    /// A section of `<proxyview>`, or `None` if this device does not have one.
    pub fn section(&self, name: &str) -> Option<&Element> {
        self.device().child(name)
    }

    /// A section of `<proxyview>`, created in its usual place if absent.
    pub fn section_mut(&mut self, name: &str) -> &mut Element {
        if self.device().child(name).is_none() {
            let after = SECTION_ORDER
                .iter()
                .position(|s| *s == name)
                .map(|i| &SECTION_ORDER[..i])
                .unwrap_or(&[]);
            // Land after the latest earlier section this device actually has.
            let anchor = after
                .iter()
                .rev()
                .find(|s| self.device().child(s).is_some())
                .copied();
            let new = Element::new(name);
            match anchor {
                Some(a) => self.device_mut().insert_after(a, new),
                None => {
                    self.device_mut().push(new);
                }
            }
        }
        self.device_mut().child_mut(name).expect("just inserted")
    }

    /// `<connection>`: session TCP port and idle timeout.
    pub fn connection(&mut self) -> &mut Element {
        self.section_mut("connection")
    }

    /// `<SSH>`: serial-over-SSH passthrough.
    pub fn ssh(&mut self) -> &mut Element {
        self.section_mut("SSH")
    }

    /// `<security_policy>`: lockout, password rules, authentication order.
    pub fn security(&mut self) -> &mut Element {
        self.section_mut("security_policy")
    }

    /// `<snmp>`: trap destination and community.
    pub fn snmp(&mut self) -> &mut Element {
        self.section_mut("snmp")
    }

    /// `<kvm_manager>`: central-management appliance.
    pub fn kvm_manager(&mut self) -> &mut Element {
        self.section_mut("kvm_manager")
    }

    /// The first `<network_adapter>`. The hardware has one.
    pub fn network(&mut self) -> Result<&mut Element, Error> {
        self.section_mut("network_adapters")
            .child_mut("network_adapter")
            .ok_or_else(|| Error::Config("device reported no network adapter".into()))
    }

    /// One `<serial_device>` by its port number (1 or 2).
    pub fn serial(&mut self, port: u32) -> Result<&mut Element, Error> {
        let key = port.to_string();
        self.section_mut("serial_devices")
            .child_where_mut("serial_device", "port", &key)
            .ok_or_else(|| Error::Config(format!("device has no serial port {port}")))
    }

    /// One `<target>` (a KVM port) by its port number.
    pub fn target(&mut self, port: u32) -> Result<&mut Element, Error> {
        let key = port.to_string();
        self.section_mut("targets")
            .child_where_mut("target", "port", &key)
            .ok_or_else(|| Error::Config(format!("device has no target port {port}")))
    }

    /// The `<users>` container, for adding and removing accounts.
    pub fn users(&mut self) -> &mut Element {
        self.section_mut("users")
    }

    /// One `<user>` by name, matched case-insensitively as the device does.
    pub fn user(&mut self, name: &str) -> Result<&mut Element, Error> {
        self.users()
            .children_named_mut("user")
            .find(|u| u.attr_or_empty("name").eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::Config(format!("no such user: {name}")))
    }

    /// The `<power>` container, holding `<pdu_types>` and the configured `<pdu>`s.
    pub fn power(&mut self) -> &mut Element {
        self.section_mut("power")
    }

    /// `<aaa>/<ldap>`, with the stub the vendor's client injects when the
    /// device sends no `<aaa>` at all.
    pub fn ldap(&mut self) -> &mut Element {
        self.aaa_child("ldap")
    }

    /// `<aaa>/<radius>`, created if the device sent no `<aaa>`.
    pub fn radius(&mut self) -> &mut Element {
        self.aaa_child("radius")
    }

    fn aaa_child(&mut self, which: &str) -> &mut Element {
        let aaa = self.section_mut("aaa");
        if aaa.child(which).is_none() {
            // The same defaults `FetchCfgHttpCmd.addAaaStubToDeviceResponseLinesIfNeeded`
            // fills in, so a device that has never had AAA configured starts
            // from what its own client would have shown.
            let mut stub = Element::new(which);
            if which == "ldap" {
                stub.set("mode", LdapMode::Basic.as_str())
                    .set("search_dn", "")
                    .set("search_passw", "")
                    .set("search_base", "")
                    .set("user_attrib", "cn")
                    .set("acl_attrib", "");
            } else {
                stub.set("realm", "").set("accounting", "false");
            }
            // `ldap` before `radius`, as the stub has them.
            if which == "radius" {
                aaa.push(stub);
            } else {
                aaa.children.insert(0, xml::Node::Element(stub));
            }
        }
        aaa.child_mut(which).expect("just inserted")
    }

    /// Set the device clock. Without this, saving leaves the clock alone.
    pub fn set_system_time(&mut self, t: SystemTime) {
        self.set_time = true;
        self.section_mut("system_time")
            .set("day", t.day.to_string())
            .set("hour", t.hour.to_string())
            .set("min", t.minute.to_string())
            .set("sec", t.second.to_string())
            .set("month", t.month.to_string())
            .set("year", t.year.to_string());
    }

    /// The document to post as `CMA_CONFIG`.
    ///
    /// Applies the two rules the device's own client applies on the way out:
    /// `value=""`, and a blank `<system_time>` unless the clock was set.
    pub fn to_save_body(&self) -> String {
        let mut out = self.data.clone();
        out.set("value", "");
        if !self.set_time {
            if let Some(t) = out
                .child_mut("proxyview")
                .and_then(|pv| pv.child_mut("system_time"))
            {
                for field in ["day", "hour", "min", "sec", "month", "year"] {
                    t.set(field, "");
                }
            }
        }
        out.to_xml()
    }

    /// The document as the device sent it, for export.
    pub fn to_xml(&self) -> String {
        self.data.to_xml()
    }

    /// Send the document's `<system_time>` as it stands, rather than blanking
    /// it. Only an importer of a document that was written to carry a time
    /// wants this: an exported configuration carries the clock as it read at
    /// export, and applying that would wind the device back.
    pub fn send_system_time(&mut self) {
        self.set_time = true;
    }

    /// Wrap an externally supplied `<config_data>` document (an import) so it
    /// can be saved. Its `<system_time>` is blanked like any other save's,
    /// unless [`ConfigDoc::send_system_time`] says otherwise.
    pub fn from_element(data: Element) -> Result<Self, Error> {
        if data.name != "config_data" {
            return Err(Error::Config(format!(
                "expected a <config_data> document, got <{}>",
                data.name
            )));
        }
        if data.child("proxyview").is_none() {
            return Err(Error::Device("configuration has no <proxyview>".into()));
        }
        Ok(ConfigDoc {
            data,
            set_time: false,
        })
    }
}

/// Wall-clock time to set on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// An authentication source, as it appears in `security_policy/@aaa_order`.
///
/// The codes are the client's `MethodDescriptor` constants. Local is always
/// present and always first; the device's own UI does not allow otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Local,
    Ldap,
    Radius,
}

impl AuthMethod {
    pub const VARIANTS: &'static [&'static str] = &["local", "ldap", "radius"];

    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Local => "local",
            AuthMethod::Ldap => "ldap",
            AuthMethod::Radius => "radius",
        }
    }

    /// The numeric code the wire uses.
    pub fn code(self) -> u8 {
        match self {
            AuthMethod::Ldap => 0,
            AuthMethod::Radius => 1,
            AuthMethod::Local => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(AuthMethod::Ldap),
            1 => Some(AuthMethod::Radius),
            3 => Some(AuthMethod::Local),
            _ => None,
        }
    }

    /// Parse an `aaa_order` value, e.g. `"3,1"` → local, then RADIUS.
    ///
    /// Unknown codes (the firmware reserves 2 for TACACS+ and 4 for none) are
    /// skipped rather than failing: a device configured through some other
    /// path should still be readable.
    pub fn parse_order(value: &str) -> Vec<AuthMethod> {
        value
            .split(',')
            .filter_map(|c| c.trim().parse().ok())
            .filter_map(AuthMethod::from_code)
            .collect()
    }

    /// Render an `aaa_order` value.
    pub fn format_order(order: &[AuthMethod]) -> String {
        order
            .iter()
            .map(|m| m.code().to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AuthMethod {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "local" => Ok(AuthMethod::Local),
            "ldap" => Ok(AuthMethod::Ldap),
            "radius" => Ok(AuthMethod::Radius),
            other => Err(Error::Config(format!(
                "unknown authentication source: {other}"
            ))),
        }
    }
}

/// How the device maps an LDAP directory onto its own permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LdapMode {
    /// Authenticate only; permissions come from the local account.
    Basic,
    /// Read permissions from an attribute on the user object.
    User,
    /// Read permissions from the user's group.
    Group,
}

impl LdapMode {
    pub const VARIANTS: &'static [&'static str] = &["basic", "user", "group"];

    pub fn as_str(self) -> &'static str {
        match self {
            LdapMode::Basic => "basic",
            LdapMode::User => "user",
            LdapMode::Group => "group",
        }
    }
}

impl FromStr for LdapMode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "basic" => Ok(LdapMode::Basic),
            "user" => Ok(LdapMode::User),
            "group" => Ok(LdapMode::Group),
            other => Err(Error::Config(format!("unknown LDAP mode: {other}"))),
        }
    }
}

/// Transport for one LDAP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LdapProtocol {
    /// Plain LDAP, no encryption.
    Ldap,
    /// LDAPS (implicit TLS, usually port 636).
    Ldaps,
    /// StartTLS on the plain port.
    Tls,
}

impl LdapProtocol {
    pub const VARIANTS: &'static [&'static str] = &["ldap", "ldaps", "tls"];

    pub fn as_str(self) -> &'static str {
        match self {
            LdapProtocol::Ldap => "ldap",
            LdapProtocol::Ldaps => "ldaps",
            LdapProtocol::Tls => "tls",
        }
    }
}

impl FromStr for LdapProtocol {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "ldap" => Ok(LdapProtocol::Ldap),
            "ldaps" | "ssl" => Ok(LdapProtocol::Ldaps),
            "tls" | "starttls" => Ok(LdapProtocol::Tls),
            other => Err(Error::Config(format!("unknown LDAP protocol: {other}"))),
        }
    }
}

/// How the adapter gets its IPv6 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ipv6Mode {
    Dhcp,
    /// SLAAC.
    Stateless,
    Static,
    /// IPv6 off.
    None,
}

impl Ipv6Mode {
    pub const VARIANTS: &'static [&'static str] = &["dhcp", "stateless", "static", "none"];

    pub fn as_str(self) -> &'static str {
        match self {
            Ipv6Mode::Dhcp => "dhcp",
            Ipv6Mode::Stateless => "stateless",
            Ipv6Mode::Static => "static",
            Ipv6Mode::None => "none",
        }
    }
}

impl FromStr for Ipv6Mode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "dhcp" => Ok(Ipv6Mode::Dhcp),
            "stateless" | "slaac" | "auto" => Ok(Ipv6Mode::Stateless),
            "static" => Ok(Ipv6Mode::Static),
            "none" | "off" => Ok(Ipv6Mode::None),
            other => Err(Error::Config(format!("unknown IPv6 mode: {other}"))),
        }
    }
}

/// Serial line parity, written as a single letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Odd,
    Even,
    Mark,
    Space,
}

impl Parity {
    pub const VARIANTS: &'static [&'static str] = &["none", "odd", "even", "mark", "space"];

    pub fn as_str(self) -> &'static str {
        match self {
            Parity::None => "N",
            Parity::Odd => "O",
            Parity::Even => "E",
            Parity::Mark => "M",
            Parity::Space => "S",
        }
    }

    /// The spelled-out name, for display.
    pub fn name(self) -> &'static str {
        match self {
            Parity::None => "none",
            Parity::Odd => "odd",
            Parity::Even => "even",
            Parity::Mark => "mark",
            Parity::Space => "space",
        }
    }

    /// Read the single-letter wire form back.
    pub fn from_letter(letter: &str) -> Option<Self> {
        match letter.trim().to_ascii_uppercase().as_str() {
            "N" => Some(Parity::None),
            "O" => Some(Parity::Odd),
            "E" => Some(Parity::Even),
            "M" => Some(Parity::Mark),
            "S" => Some(Parity::Space),
            _ => None,
        }
    }
}

impl FromStr for Parity {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "n" => Ok(Parity::None),
            "odd" | "o" => Ok(Parity::Odd),
            "even" | "e" => Ok(Parity::Even),
            "mark" | "m" => Ok(Parity::Mark),
            "space" | "s" => Ok(Parity::Space),
            other => Err(Error::Config(format!("unknown parity: {other}"))),
        }
    }
}

/// What a serial port is wired to, from `serial_device/@PDU`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialAssign {
    /// A plain serial console.
    None,
    /// A keyboard-controlled power strip.
    KeyboardPower,
    /// A port-operated controller (POC) power strip.
    Poc,
}

impl SerialAssign {
    pub const VARIANTS: &'static [&'static str] = &["none", "keyboard-power", "poc"];

    pub fn as_str(self) -> &'static str {
        match self {
            SerialAssign::None => "0",
            SerialAssign::KeyboardPower => "1",
            SerialAssign::Poc => "3",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SerialAssign::None => "none",
            SerialAssign::KeyboardPower => "keyboard-power",
            SerialAssign::Poc => "poc",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code.trim() {
            "0" | "" => Some(SerialAssign::None),
            "1" => Some(SerialAssign::KeyboardPower),
            "3" => Some(SerialAssign::Poc),
            _ => None,
        }
    }
}

impl FromStr for SerialAssign {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Ok(SerialAssign::None),
            "keyboard-power" | "kb-power" => Ok(SerialAssign::KeyboardPower),
            "poc" => Ok(SerialAssign::Poc),
            other => Err(Error::Config(format!("unknown serial assignment: {other}"))),
        }
    }
}

/// Baud rates the device's own serial dialog offers.
pub const BAUD_RATES: &[&str] = &[
    "110", "300", "600", "1200", "2400", "4800", "9600", "19200", "38400", "56000", "57600",
    "115200", "128000", "256000",
];

/// Data-bit counts the device offers.
pub const DATA_BITS: &[&str] = &["4", "5", "6", "7", "8"];

/// Stop-bit counts the device offers. `1.5` is a real choice, so these are
/// strings rather than numbers.
pub const STOP_BITS: &[&str] = &["1", "1.5", "2", "3"];

/// Administrator, from `user/@userlevel`.
pub const USER_LEVEL_ADMIN: &str = "1";
/// Standard user, from `user/@userlevel`.
pub const USER_LEVEL_USER: &str = "2";

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str =
        include_str!("../../../research/notes/analysis/live-fixtures/config_full.xml");

    fn doc() -> ConfigDoc {
        ConfigDoc::parse(CONFIG).unwrap()
    }

    #[test]
    fn parses_the_live_document() {
        let d = doc();
        assert_eq!(d.config_id(), Some("00/00/00 00:00:00 0"));
        assert_eq!(d.device().attr("name"), Some("D1214403"));
        assert_eq!(
            d.section("connection").unwrap().attr("ip_port"),
            Some("900")
        );
    }

    #[test]
    fn editing_one_attribute_leaves_the_rest_alone() {
        let mut d = doc();
        d.connection().set("idle_time", "30");
        let body = d.to_save_body();
        assert!(body.contains(r#"idle_time="30""#));
        // Everything the editor never touched is still there, verbatim.
        assert!(body.contains(r#"<target name="Server 08" port="8"/>"#));
        assert!(body.contains(r#"mac="00:15:9D:00:00:00""#));
        assert!(body.contains("PDUMH15ATNET"));
    }

    #[test]
    fn save_blanks_the_clock_unless_it_was_set() {
        let d = doc();
        assert!(d
            .to_save_body()
            .contains(r#"<system_time day="" hour="" min="" sec="" month="" year=""/>"#));
    }

    #[test]
    fn save_carries_the_clock_when_it_was_set() {
        let mut d = doc();
        d.set_system_time(SystemTime {
            year: 2026,
            month: 9,
            day: 19,
            hour: 14,
            minute: 5,
            second: 0,
        });
        let body = d.to_save_body();
        assert!(body.contains(r#"day="19""#));
        assert!(body.contains(r#"year="2026""#));
        assert!(body.contains(r#"min="5""#));
    }

    #[test]
    fn save_marks_the_document_as_a_save() {
        assert!(doc().to_save_body().contains(r#"value=""#));
    }

    #[test]
    fn aaa_stub_appears_when_the_device_sent_none() {
        let mut d = doc();
        assert!(d.section("aaa").is_none(), "fixture has no <aaa>");
        d.ldap().set("search_base", "dc=example,dc=com");
        let body = d.to_save_body();
        assert!(body.contains(r#"<ldap mode="basic""#));
        assert!(body.contains(r#"search_base="dc=example,dc=com""#));
        // The stub carries a radius element too, as the vendor's client's does.
        d.radius();
        assert!(d
            .to_save_body()
            .contains(r#"<radius realm="" accounting="false"/>"#));
        // And it lands where the client puts it: right after the security policy.
        let after_policy = d
            .to_save_body()
            .split("security_policy")
            .nth(1)
            .unwrap()
            .to_string();
        assert!(after_policy
            .trim_start_matches(|c| c != '<')
            .starts_with("<aaa>"));
    }

    #[test]
    fn an_imported_document_does_not_wind_the_clock_back() {
        let data = crate::xml::find_element(CONFIG, "config_data")
            .unwrap()
            .unwrap();
        let mut imported = ConfigDoc::from_element(data).unwrap();
        assert!(imported
            .to_save_body()
            .contains(r#"<system_time day="" hour="""#));
        // Unless the caller means it.
        imported.send_system_time();
        assert!(imported.to_save_body().contains(r#"day="18""#));
    }

    #[test]
    fn finds_users_targets_and_serial_ports() {
        let mut d = doc();
        assert_eq!(d.user("ADMIN").unwrap().attr("userlevel"), Some("1"));
        assert_eq!(d.target(3).unwrap().attr("name"), Some("Server 03"));
        assert_eq!(d.serial(2).unwrap().attr("baud_rate"), Some("9600"));
        assert!(d.target(99).is_err());
        assert!(d.user("nobody").is_err());
    }

    #[test]
    fn aaa_order_round_trips() {
        assert_eq!(AuthMethod::parse_order("3"), [AuthMethod::Local]);
        assert_eq!(
            AuthMethod::parse_order("3,1"),
            [AuthMethod::Local, AuthMethod::Radius]
        );
        assert_eq!(
            AuthMethod::parse_order("3,0,1"),
            [AuthMethod::Local, AuthMethod::Ldap, AuthMethod::Radius]
        );
        // TACACS+ (2) is in the firmware's enum but not offered; skip it.
        assert_eq!(AuthMethod::parse_order("3,2"), [AuthMethod::Local]);
        assert_eq!(
            AuthMethod::format_order(&[AuthMethod::Local, AuthMethod::Ldap]),
            "3,0"
        );
    }

    #[test]
    fn parity_round_trips_through_its_letter() {
        for p in [
            Parity::None,
            Parity::Odd,
            Parity::Even,
            Parity::Mark,
            Parity::Space,
        ] {
            assert_eq!(Parity::from_letter(p.as_str()), Some(p));
            assert_eq!(p.name().parse::<Parity>().unwrap(), p);
        }
    }

    #[test]
    fn serial_assignment_round_trips_through_its_code() {
        for a in [
            SerialAssign::None,
            SerialAssign::KeyboardPower,
            SerialAssign::Poc,
        ] {
            assert_eq!(SerialAssign::from_code(a.as_str()), Some(a));
            assert_eq!(a.name().parse::<SerialAssign>().unwrap(), a);
        }
    }
}
