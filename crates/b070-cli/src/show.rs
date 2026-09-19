// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Printing the device's settings.
//!
//! One function per area, so that `b070 <area> show` and the corresponding
//! block of `b070 config` are the same code and cannot drift apart.
//!
//! Secrets — the LDAP bind password, RADIUS shared secrets, the SNMP community
//! — are stored on the device in the clear and come back in every fetch. They
//! are masked unless `--show-secrets` is given, so that a pasted `b070 config`
//! does not hand them out.

use b070_mgmt::model::{DeviceConfig, Pdu, SerialDevice, User};
use b070_mgmt::{AuthMethod, Parity};

use crate::args::{or_dash, show_port_list, yes_no};

/// Width of the label column, chosen so the longest label still has a gap.
const LABEL: usize = 22;

pub fn kv(key: &str, value: impl AsRef<str>) {
    println!("{:<LABEL$} {}", key, value.as_ref());
}

fn heading(title: &str) {
    println!("\n{title}");
    println!("{}", "-".repeat(title.len()));
}

/// Mask a secret unless the caller asked to see it.
fn secret(value: Option<&str>, reveal: bool) -> String {
    match value {
        None => "-".to_string(),
        Some("") => "-".to_string(),
        Some(v) if reveal => v.to_string(),
        Some(_) => "(set; --show-secrets to reveal)".to_string(),
    }
}

/// Everything, in the order the device's own configuration dialog has it.
pub fn all(c: &DeviceConfig, secrets: bool) {
    device(c);
    heading("Network");
    network(c);
    heading("Serial ports");
    serial(c);
    heading("SSH");
    ssh(c);
    heading("Security");
    security(c);
    heading("Authentication");
    auth(c, secrets);
    heading("SNMP");
    snmp(c, secrets);
    heading("Central management");
    kvm_manager(c);
    heading("Clock");
    time(c);
    heading("Targets");
    targets(c);
    heading("Users");
    users(c);
    heading("Power");
    power(c);
}

pub fn device(c: &DeviceConfig) {
    kv("Name", or_dash(c.name.as_deref()));
    kv("Hardware type", or_dash(c.hardware_type.as_deref()));
    kv(
        "KVM ports",
        c.switch_ports.map_or("-".into(), |v| v.to_string()),
    );
    if let Some(cascade) = c.cascade.as_deref().filter(|s| !s.is_empty()) {
        kv("Cascaded switch", cascade);
    }
    kv(
        "Session TCP port",
        c.session_ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    );
    kv(
        "Idle timeout",
        match c.idle_time_minutes {
            None => "-".to_string(),
            Some(0) => "none".to_string(),
            Some(m) => format!("{m} min"),
        },
    );
}

pub fn network(c: &DeviceConfig) {
    let n = &c.network;
    kv("MAC", or_dash(n.mac.as_deref()));
    kv("IPv4", if n.use_static_ip { "static" } else { "DHCP" });
    kv("  address", or_dash(n.ip.as_deref()));
    kv("  subnet mask", or_dash(n.subnet.as_deref()));
    kv("  gateway", or_dash(n.gateway.as_deref()));
    kv(
        "  DNS",
        format!(
            "{} ({})",
            or_dash(n.dns.as_deref()),
            if n.static_dns { "static" } else { "from DHCP" }
        ),
    );
    kv(
        "IPv6",
        if n.ipv6_enabled {
            or_dash(n.ipv6_mode.as_deref()).to_string()
        } else {
            "off".to_string()
        },
    );
    if n.ipv6_enabled {
        kv(
            "  address",
            // The device reports `::/0` for "IPv6 is on but I have not got an
            // address yet", which reads like a default route. Say so instead.
            match (n.ipv6.as_deref(), n.ipv6_prefix) {
                (Some("::") | None, _) => "-".to_string(),
                (Some(a), Some(p)) if !a.is_empty() => format!("{a}/{p}"),
                (Some(a), _) if !a.is_empty() => a.to_string(),
                _ => "-".to_string(),
            },
        );
        kv("  gateway", or_dash(n.ipv6_gateway.as_deref()));
        kv(
            "  DNS",
            format!(
                "{} ({})",
                or_dash(n.ipv6_dns.as_deref()),
                if n.ipv6_static_dns {
                    "static"
                } else {
                    "autoconfigured"
                }
            ),
        );
    }
}

pub fn ssh(c: &DeviceConfig) {
    kv("Serial over SSH", yes_no(c.ssh.enabled));
    kv(
        "  serial 1 TCP port",
        c.ssh.port1.map_or("-".into(), |p| p.to_string()),
    );
    kv(
        "  serial 2 TCP port",
        c.ssh.port2.map_or("-".into(), |p| p.to_string()),
    );
}

pub fn security(c: &DeviceConfig) {
    let s = &c.security;
    kv(
        "Block account after",
        match (s.max_try, s.try_time_secs) {
            (Some(n), Some(w)) => format!("{n} failed logins within {}", duration(w)),
            (Some(n), None) => format!("{n} failed logins"),
            _ => "-".to_string(),
        },
    );
    kv(
        "Block lasts",
        if s.lock_forever {
            "until an administrator clears it".to_string()
        } else {
            s.lock_time_secs.map_or("-".into(), duration)
        },
    );
    kv("Strict password rules", yes_no(s.password_restrictions));
    kv("OSD password", yes_no(s.osd_password));
    kv(
        "Idle timeout",
        match c.idle_time_minutes {
            None => "-".to_string(),
            Some(0) => "none".to_string(),
            Some(m) => format!("{m} min"),
        },
    );
}

/// Seconds as the device's own dialog shows them: hours and minutes.
fn duration(secs: u32) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (h, 0, 0) => format!("{h}h"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, 0) => format!("{h}h {m}m"),
        (h, m, s) => format!("{h}h {m}m {s}s"),
    }
}

pub fn auth(c: &DeviceConfig, secrets: bool) {
    let order = &c.security.auth_order;
    kv(
        "Order",
        if order.is_empty() {
            "-".to_string()
        } else {
            order
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    println!();
    ldap(c, secrets);
    println!();
    radius(c, secrets);
}

pub fn ldap(c: &DeviceConfig, secrets: bool) {
    let l = &c.aaa.ldap;
    let in_use = c.security.auth_order.contains(&AuthMethod::Ldap);
    kv("LDAP", if in_use { "enabled" } else { "not in use" });
    kv("  permission source", or_dash(l.mode.as_deref()));
    kv("  search DN", or_dash(l.search_dn.as_deref()));
    kv(
        "  search password",
        secret(l.search_password.as_deref(), secrets),
    );
    kv("  search base", or_dash(l.search_base.as_deref()));
    kv("  user attribute", or_dash(l.user_attribute.as_deref()));
    kv("  access attribute", or_dash(l.acl_attribute.as_deref()));
    if l.servers.is_empty() {
        kv("  servers", "none configured");
    } else {
        for (i, s) in l.servers.iter().enumerate() {
            kv(
                &format!("  server {}", i + 1),
                format!(
                    "{}:{} ({})",
                    s.host,
                    s.port.map_or("-".into(), |p| p.to_string()),
                    or_dash(s.protocol.as_deref()),
                ),
            );
        }
    }
}

pub fn radius(c: &DeviceConfig, secrets: bool) {
    let r = &c.aaa.radius;
    let in_use = c.security.auth_order.contains(&AuthMethod::Radius);
    kv("RADIUS", if in_use { "enabled" } else { "not in use" });
    kv("  realm", or_dash(r.realm.as_deref()));
    kv("  accounting", yes_no(r.accounting));
    if r.servers.is_empty() {
        kv("  servers", "none configured");
    } else {
        for (i, s) in r.servers.iter().enumerate() {
            kv(
                &format!("  server {}", i + 1),
                format!(
                    "{}:{} (accounting {}), secret {}",
                    s.host,
                    s.port.map_or("-".into(), |p| p.to_string()),
                    s.accounting_port.map_or("-".into(), |p| p.to_string()),
                    secret(s.secret.as_deref(), secrets),
                ),
            );
        }
    }
}

pub fn snmp(c: &DeviceConfig, secrets: bool) {
    kv("Traps", yes_no(c.snmp.traps_enabled));
    kv("Trap destination", or_dash(c.snmp.manager_ip.as_deref()));
    kv("Community", secret(c.snmp.community.as_deref(), secrets));
}

pub fn kvm_manager(c: &DeviceConfig) {
    let m = &c.kvm_manager;
    kv("Managed centrally", yes_no(m.enabled));
    kv(
        "Manager address",
        if m.fixed_ip {
            or_dash(m.ip.as_deref()).to_string()
        } else {
            "discovered".to_string()
        },
    );
}

pub fn time(c: &DeviceConfig) {
    match c.system_time {
        Some(t) => kv(
            "Device clock",
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                t.year, t.month, t.day, t.hour, t.minute, t.second
            ),
        ),
        None => kv("Device clock", "-"),
    }
}

pub fn serial(c: &DeviceConfig) {
    if c.serial_devices.is_empty() {
        println!("no serial ports");
        return;
    }
    for d in &c.serial_devices {
        println!("{:>2}  {}", d.port, serial_line(d));
    }
}

fn serial_line(d: &SerialDevice) -> String {
    let parity = d
        .parity
        .as_deref()
        .and_then(Parity::from_letter)
        .map_or("-", Parity::name);
    format!(
        "{:<12} {:>6} {}{}{}  parity={:<5} {:<8} {}",
        d.name,
        or_dash(d.baud_rate.as_deref()),
        or_dash(d.data_bits.as_deref()),
        if d.parity.is_some() { "/" } else { "" },
        or_dash(d.stop_bits.as_deref()),
        parity,
        if d.active { "visible" } else { "hidden" },
        match d.assign {
            Some(a) => format!("assigned to {}", a.name()),
            None => "assigned to none".to_string(),
        }
    )
}

pub fn targets(c: &DeviceConfig) {
    if c.targets.is_empty() {
        println!("no targets");
        return;
    }
    for t in &c.targets {
        println!("{:>2}  {}", t.port, t.name);
    }
}

pub fn users(c: &DeviceConfig) {
    if c.users.is_empty() {
        println!("no users");
        return;
    }
    println!(
        "{:<16} {:<6} {:<9} {:<18} {:<18} SERIAL",
        "NAME", "ROLE", "STATE", "TARGETS", "VIRTUAL MEDIA"
    );
    for u in &c.users {
        println!("{}", user_line(u));
    }
}

fn user_line(u: &User) -> String {
    format!(
        "{:<16} {:<6} {:<9} {:<18} {:<18} {}",
        u.name,
        if u.admin { "admin" } else { "user" },
        if u.active { "active" } else { "blocked" },
        show_port_list(u.target_ports.as_deref()),
        show_port_list(u.vm_target_ports.as_deref()),
        show_port_list(u.serial_target_ports.as_deref()),
    )
}

pub fn power(c: &DeviceConfig) {
    if c.power.pdus.is_empty() {
        println!("no power distribution units configured");
        println!(
            "({} models supported; `b070 pdu models` lists them)",
            c.power.types.len()
        );
        return;
    }
    for p in &c.power.pdus {
        println!("{}", pdu_line(p));
        for o in &p.outlet_map {
            let target = c
                .targets
                .iter()
                .find(|t| t.port == o.target_port)
                .map_or("-", |t| t.name.as_str());
            println!(
                "    outlet {:>2}  ->  port {} ({})",
                o.outlet, o.target_port, target
            );
        }
    }
}

fn pdu_line(p: &Pdu) -> String {
    format!(
        "{}  {}  {} outlets  at {}",
        p.name,
        or_dash(p.model.as_deref()),
        p.outlets.map_or("-".into(), |n| n.to_string()),
        or_dash(p.ip.as_deref()),
    )
}

/// The PDU models this firmware can drive.
pub fn pdu_models(c: &DeviceConfig) {
    for t in &c.power.types {
        println!(
            "{:<12} {:>3}  {}",
            t.manufacturer,
            t.outlets.map_or("-".into(), |n| n.to_string()),
            t.model
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_the_device_dialog() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(60), "1m");
        assert_eq!(duration(180), "3m");
        assert_eq!(duration(1800), "30m");
        assert_eq!(duration(3600), "1h");
        assert_eq!(duration(5400), "1h 30m");
        assert_eq!(duration(90), "1m 30s");
    }

    #[test]
    fn secrets_are_masked_by_default() {
        assert_eq!(
            secret(Some("hunter2"), false),
            "(set; --show-secrets to reveal)"
        );
        assert_eq!(secret(Some("hunter2"), true), "hunter2");
        // Nothing stored is nothing to hide.
        assert_eq!(secret(Some(""), false), "-");
        assert_eq!(secret(None, false), "-");
    }
}
