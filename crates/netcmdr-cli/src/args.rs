// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Argument types shared by the `show` and `set` commands.
//!
//! The device's closed value sets (parity, LDAP mode, …) live in
//! [`netcmdr_mgmt::config`], because they are protocol vocabulary. What is here is
//! only how they reach the command line: clap needs an owned type to derive
//! `ValueEnum` on, and the wire spellings would make poor flag values anyway
//! (`--assign 3` rather than `--assign poc`).

use anyhow::{bail, Result};
use clap::ValueEnum;

/// A setting that is on or off. Spelled out rather than taken as a bare flag so
/// that "leave it alone" stays distinguishable from "turn it off": every `set`
/// command applies only the options actually given.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

impl From<OnOff> for bool {
    fn from(v: OnOff) -> bool {
        v == OnOff::On
    }
}

/// Render a device boolean for display.
pub fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

/// Render an optional string, showing `-` for absent and empty values.
pub fn or_dash(v: Option<&str>) -> &str {
    match v {
        Some(s) if !s.is_empty() => s,
        _ => "-",
    }
}

/// A set of KVM ports, as the device stores them: a comma-separated list.
///
/// `all` and `none` are conveniences; the device itself only understands the
/// list. An empty list is how the device says "no access", so `none` is written
/// as `""` and not as a missing attribute.
/// `kind` names what is being counted, so an out-of-range value says which
/// ports it meant — a device with eight KVM ports has only two serial ones.
pub fn parse_port_list(spec: &str, port_count: u32, kind: &str) -> Result<String> {
    let spec = spec.trim();
    match spec.to_ascii_lowercase().as_str() {
        "none" | "" => return Ok(String::new()),
        "all" => {
            return Ok((1..=port_count)
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(","))
        }
        _ => {}
    }

    let mut ports = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Ranges read better than eight comma-separated numbers.
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.trim().parse().map_err(|_| bad_port(part))?;
            let hi: u32 = hi.trim().parse().map_err(|_| bad_port(part))?;
            if lo == 0 || hi < lo {
                bail!("not a usable port range: {part}");
            }
            ports.extend(lo..=hi);
        } else {
            let p: u32 = part.parse().map_err(|_| bad_port(part))?;
            if p == 0 {
                bail!("port numbers start at 1");
            }
            ports.push(p);
        }
    }

    ports.sort_unstable();
    ports.dedup();
    if let Some(&over) = ports.iter().find(|&&p| p > port_count) {
        bail!("{over} is out of range: this device has {kind} ports 1 to {port_count}");
    }
    Ok(ports
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(","))
}

fn bad_port(part: &str) -> anyhow::Error {
    anyhow::anyhow!("not a port number: {part}")
}

/// Show a stored port list the way it was asked for.
pub fn show_port_list(value: Option<&str>) -> String {
    // The firmware leaves a trailing comma in some of these
    // (`vmtarget_ports="1,2,3,4,5,6,7,8,"`), so trim before deciding.
    match value.map(|v| v.trim().trim_end_matches(',')) {
        None => "-".to_string(),
        Some("") => "none".to_string(),
        Some(v) => v.to_string(),
    }
}

/// Parse a span of time as `90s`, `30m`, `2h`, `1h30m` — or a bare number of
/// seconds, which is what the device stores.
pub fn parse_duration(s: &str) -> Result<u32> {
    let s = s.trim();
    if s.is_empty() {
        bail!("expected a duration such as 30m or 1h30m");
    }
    if let Ok(secs) = s.parse::<u32>() {
        return Ok(secs);
    }

    let mut total: u32 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let n: u32 = digits
            .parse()
            .map_err(|_| anyhow::anyhow!("expected a number before {ch:?} in {s:?}"))?;
        digits.clear();
        saw_unit = true;
        total = total
            .checked_add(match ch.to_ascii_lowercase() {
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                other => bail!("unknown time unit {other:?} in {s:?}; use h, m or s"),
            })
            .ok_or_else(|| anyhow::anyhow!("{s:?} is longer than the device can store"))?;
    }
    if !digits.is_empty() || !saw_unit {
        bail!("expected a duration such as 30m or 1h30m, got {s:?}");
    }
    Ok(total)
}

/// Parse `YYYY-MM-DD HH:MM[:SS]`, with `T` accepted in place of the space.
pub fn parse_datetime(s: &str) -> Result<netcmdr_mgmt::SystemTime> {
    let s = s.trim();
    let (date, time) = s
        .split_once(['T', ' '])
        .ok_or_else(|| anyhow::anyhow!("expected YYYY-MM-DD HH:MM:SS, got {s:?}"))?;

    let d: Vec<&str> = date.split('-').collect();
    let t: Vec<&str> = time.trim().split(':').collect();
    if d.len() != 3 || !(2..=3).contains(&t.len()) {
        bail!("expected YYYY-MM-DD HH:MM:SS, got {s:?}");
    }
    let num = |v: &str, what: &str| -> Result<u32> {
        v.parse()
            .map_err(|_| anyhow::anyhow!("{what} is not a number: {v:?}"))
    };

    let when = netcmdr_mgmt::SystemTime {
        year: num(d[0], "year")?,
        month: num(d[1], "month")?,
        day: num(d[2], "day")?,
        hour: num(t[0], "hour")?,
        minute: num(t[1], "minute")?,
        second: if t.len() == 3 {
            num(t[2], "second")?
        } else {
            0
        },
    };
    if !(1..=12).contains(&when.month) || !(1..=31).contains(&when.day) {
        bail!("{s:?} is not a real date");
    }
    if when.hour > 23 || when.minute > 59 || when.second > 59 {
        bail!("{s:?} is not a real time of day");
    }
    Ok(when)
}

/// The local wall-clock time, for `time set --now`.
///
/// The device stores a bare wall clock with no zone and no NTP, so what it
/// wants is whatever the clock on the wall next to it says.
pub fn local_now() -> netcmdr_mgmt::SystemTime {
    let now = chrono::Local::now();
    use chrono::{Datelike, Timelike};
    netcmdr_mgmt::SystemTime {
        year: now.year() as u32,
        month: now.month(),
        day: now.day(),
        hour: now.hour(),
        minute: now.minute(),
        second: now.second(),
    }
}

/// Split a `1,2,5-8` port argument into numbers, for commands that take ports
/// as an argument rather than storing them.
pub fn parse_ports(spec: &str, port_count: u32) -> Result<Vec<u32>> {
    let list = parse_port_list(spec, port_count, "KVM")?;
    Ok(list
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().expect("built from parsed numbers"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_lists_accept_ranges_and_keywords() {
        assert_eq!(parse_port_list("all", 8, "KVM").unwrap(), "1,2,3,4,5,6,7,8");
        assert_eq!(parse_port_list("none", 8, "KVM").unwrap(), "");
        assert_eq!(parse_port_list("", 8, "KVM").unwrap(), "");
        assert_eq!(parse_port_list("3", 8, "KVM").unwrap(), "3");
        assert_eq!(parse_port_list("1-3,5", 8, "KVM").unwrap(), "1,2,3,5");
        // Sorted and deduplicated, so the device never sees "2,1,2".
        assert_eq!(parse_port_list("2,1,2", 8, "KVM").unwrap(), "1,2");
        // "all" means all of whatever is being counted.
        assert_eq!(parse_port_list("all", 2, "serial").unwrap(), "1,2");
    }

    #[test]
    fn port_lists_reject_what_the_device_has_not_got() {
        assert!(parse_port_list("9", 8, "KVM").is_err());
        assert!(parse_port_list("0", 8, "KVM").is_err());
        assert!(parse_port_list("5-3", 8, "KVM").is_err());
        assert!(parse_port_list("two", 8, "KVM").is_err());
        // The message names what ran out.
        let e = parse_port_list("3", 2, "serial").unwrap_err().to_string();
        assert!(e.contains("serial"), "{e}");
    }

    #[test]
    fn stored_port_lists_display_sensibly() {
        assert_eq!(show_port_list(None), "-");
        assert_eq!(show_port_list(Some("")), "none");
        // The firmware's trailing comma is not a ninth port.
        assert_eq!(show_port_list(Some("1,2,3,")), "1,2,3");
    }

    #[test]
    fn parses_a_date_and_time() {
        let t = parse_datetime("2026-09-19T14:05:30").unwrap();
        assert_eq!((t.year, t.month, t.day), (2026, 9, 19));
        assert_eq!((t.hour, t.minute, t.second), (14, 5, 30));
        // Space separator and an omitted seconds field both work.
        let t = parse_datetime("2026-01-02 03:04").unwrap();
        assert_eq!((t.month, t.day, t.second), (1, 2, 0));
    }

    #[test]
    fn rejects_impossible_dates_and_times() {
        assert!(parse_datetime("2026-13-01 00:00:00").is_err());
        assert!(parse_datetime("2026-09-19 24:00:00").is_err());
        assert!(parse_datetime("2026-09-19").is_err());
        assert!(parse_datetime("not a date").is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("1h30m15s").unwrap(), 5415);
    }

    #[test]
    fn rejects_malformed_durations() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("30x").is_err());
        // A trailing number with no unit is a typo, not "plus 30 seconds".
        assert!(parse_duration("1h30").is_err());
    }

    #[test]
    fn parses_ports_for_commands_that_take_them() {
        assert_eq!(parse_ports("1,3-4", 8).unwrap(), [1, 3, 4]);
        assert_eq!(parse_ports("none", 8).unwrap(), Vec::<u32>::new());
    }
}
