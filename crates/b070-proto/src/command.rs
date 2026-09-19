// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The app-level `<commands>` control vocabulary.
//!
//! These are the documents the GUI hands to its plugin facade
//! (`com.minicom.app.video.controller.cmds.*`, still present in the current
//! firmware's client). The plugin re-encodes them as [`crate::kvmmsg`] messages
//! on the wire — so these are the *facade* language, not the wire format. See
//! `research/notes/FINDINGS.md` §5.1.
//!
//! Attribute spellings are the vendor's, typos included (`session_exlusive`),
//! because the device parses them literally.

use std::fmt::Write as _;

/// Escape a value for use inside a single-quoted XML attribute.
///
/// The stock client does no escaping at all — it builds these with
/// `Formatter.format`, so a password containing `'` would corrupt the document.
/// We escape properly; the device's parser accepts entities.
fn attr(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn wrap(inner: &str) -> String {
    format!("<commands>{inner}</commands>")
}

/// How a session claims the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusivity {
    Shared,
    Exclusive,
}

impl Exclusivity {
    fn as_str(self) -> &'static str {
        match self {
            Exclusivity::Shared => "false",
            Exclusivity::Exclusive => "true",
        }
    }
}

/// Mouse pointer rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MousePointer {
    Default,
    Dot,
    None,
}

impl MousePointer {
    fn as_str(self) -> &'static str {
        match self {
            MousePointer::Default => "default",
            MousePointer::Dot => "dot",
            MousePointer::None => "none",
        }
    }
}

/// One knob of the device's analog video capture.
///
/// These are the seven settings behind the stock client's Advanced Video
/// dialog (`AdvancedVideoCmd`, whose integer constants these mirror). Each is
/// set on its own, as a single attribute of a `<video_settings>` element —
/// see [`set_video_setting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSetting {
    Brightness,
    Contrast,
    /// Where the digitiser starts sampling each scan line, in pixels.
    HOffset,
    /// Sampling phase within the pixel clock, 0–31.
    Phase,
    VOffset,
    /// Reported by the device but given no control in the vendor's own dialog,
    /// and answered with a `-10000000` sentinel on every mode seen so far.
    Scale,
    Filter,
}

impl VideoSetting {
    pub fn as_str(self) -> &'static str {
        match self {
            VideoSetting::Brightness => "brightness",
            VideoSetting::Contrast => "contrast",
            VideoSetting::HOffset => "hoffset",
            VideoSetting::Phase => "phase",
            VideoSetting::VOffset => "voffset",
            VideoSetting::Scale => "scale",
            VideoSetting::Filter => "filter",
        }
    }
}

/// The noise filter applied to the digitised signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFilter {
    /// Not a filter setting at all: the vendor's dialog answers this choice by
    /// running [`auto_video_adjust`] instead of sending anything, because the
    /// device picks the filter itself as part of that. Callers must do the
    /// same; sending `filter='auto'` is not what the stock client does.
    Auto,
    None,
    Low,
    Medium,
    High,
}

impl VideoFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            VideoFilter::Auto => "auto",
            VideoFilter::None => "nofilter",
            VideoFilter::Low => "low",
            VideoFilter::Medium => "medium",
            VideoFilter::High => "high",
        }
    }

    /// Parse the value the device reports in `<video_settings filter='…'/>`.
    pub fn parse(s: &str) -> Option<VideoFilter> {
        Some(match s.trim() {
            "auto" => VideoFilter::Auto,
            "nofilter" => VideoFilter::None,
            "low" => VideoFilter::Low,
            "medium" => VideoFilter::Medium,
            "high" => VideoFilter::High,
            _ => return None,
        })
    }
}

/// Power action for a target mapped to an IP PDU outlet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerCommand {
    Cycle,
    Up,
    Down,
}

impl PowerCommand {
    /// The client's `PowerCmd` substitutes these straight into
    /// `power_command='%s'`; names match the toolbar menu items in
    /// `environ.xml` (`power_cycle`, `power_up`, `power_down`).
    fn as_str(self) -> &'static str {
        match self {
            PowerCommand::Cycle => "power_cycle",
            PowerCommand::Up => "power_up",
            PowerCommand::Down => "power_down",
        }
    }
}

/// Credentials and target selection for [`connect`].
#[derive(Debug, Clone)]
pub struct Connect<'a> {
    pub ip: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    /// KVM switch port number (1-based), as a string in the wire format.
    pub switch_port: &'a str,
    /// Session TCP port; the client defaults to `900`.
    pub tcp_port: &'a str,
    pub exclusivity: Exclusivity,
}

/// `<command name='connect' .../>`
pub fn connect(c: &Connect<'_>) -> String {
    wrap(&format!(
        "<command name='connect' ip='{}' username='{}' password='{}' \
         switch_port='{}' tcp_port='{}' session_exlusive='{}'/>",
        attr(c.ip),
        attr(c.username),
        attr(c.password),
        attr(c.switch_port),
        attr(c.tcp_port),
        c.exclusivity.as_str(),
    ))
}

/// `<command name='disconnect'/>`
///
/// The Java client compares against this exact string when deciding whether to
/// allow a command through after the proxy is torn down, so the formatting is
/// load-bearing.
pub fn disconnect() -> String {
    wrap("<command name='disconnect'/>")
}

/// `<command name='change_port' .../>` — switch to another target.
pub fn change_port(server_name: &str, port_num: &str) -> String {
    wrap(&format!(
        "<command name='change_port' servername='{}' port_num='{}'/>",
        attr(server_name),
        attr(port_num),
    ))
}

/// `<command name='video_refresh'/>` — force a full frame.
pub fn video_refresh() -> String {
    wrap("<command name='video_refresh'/>")
}

/// `<command name='auto_video_adjust' state='start'/>`
pub fn auto_video_adjust() -> String {
    wrap("<command name='auto_video_adjust' state='start'/>")
}

/// `<command name='calibrate' .../>` — mouse calibration.
pub fn calibrate_mouse() -> String {
    wrap("<command name='calibrate' state='start' mode='calibrate'/>")
}

/// `<command name='align' .../>` — mouse alignment.
pub fn align_mouse() -> String {
    wrap("<command name='align' state='start' mode='align'/>")
}

/// `<command name='mouse_pointer' .../>`
pub fn mouse_pointer(p: MousePointer) -> String {
    wrap(&format!(
        "<command name='mouse_pointer' type='{}'/>",
        p.as_str()
    ))
}

/// `<command name='power' .../>`
pub fn power(cmd: PowerCommand) -> String {
    wrap(&format!(
        "<command name='power' power_command='{}'/>",
        cmd.as_str()
    ))
}

/// `<command name='mount_drive' .../>` — expose a local drive as virtual media.
pub fn mount_drive(id: &str) -> String {
    wrap(&format!("<command name='mount_drive' id='{}'/>", attr(id)))
}

/// `<command name='mount_iso_file' .../>`
pub fn mount_iso(path: &str) -> String {
    wrap(&format!(
        "<command name='mount_iso_file' iso_file_path='{}'/>",
        attr(path)
    ))
}

/// `<command name='unmount' .../>`
pub fn unmount(id: &str) -> String {
    wrap(&format!("<command name='unmount' id='{}'/>", attr(id)))
}

/// `<command name='serial' type='open' .../>`
pub fn serial_open(port_num: &str) -> String {
    wrap(&format!(
        "<command name='serial' type='open' external='true' port_num='{}'/>",
        attr(port_num)
    ))
}

/// `<command name='serial' type='close' .../>`
pub fn serial_close() -> String {
    wrap("<command name='serial' type='close' external='true'/>")
}

/// `<command name='run_special_key' .../>` — fire a stored macro.
pub fn run_special_key(id: &str) -> String {
    wrap(&format!(
        "<command name='run_special_key' special_key_id='{}'/>",
        attr(id)
    ))
}

/// `<command name='update_special_key' .../>`
pub fn update_special_key(id: &str, macro_def: &str, description: &str) -> String {
    wrap(&format!(
        "<command name='update_special_key' special_key_id='{}' macro='{}' description='{}'/>",
        attr(id),
        attr(macro_def),
        attr(description),
    ))
}

/// `<command name='delete_special_key' .../>`
pub fn delete_special_key(id: &str) -> String {
    wrap(&format!(
        "<command name='delete_special_key' special_key_id='{}'/>",
        attr(id)
    ))
}

/// `<command name='gui' active='false'/>` — suppress the device-drawn GUI.
pub fn gui_inactive() -> String {
    wrap("<command name='gui' active='false'/>")
}

/// `<command name='config'><config>…</config></command>` with caller-supplied
/// inner XML.
pub fn config(inner: &str) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "<command name='config'><config>{inner}</config></command>"
    );
    wrap(&s)
}

/// Set one analog video knob, as a `config` command.
///
/// One attribute per command is the vendor's own granularity: its Advanced
/// Video dialog sends a fresh document for every slider release and every
/// spinner click, rather than the whole `<video_settings>` element.
pub fn set_video_setting(setting: VideoSetting, value: &str) -> String {
    config(&format!(
        "<video_settings {}='{}'/>",
        setting.as_str(),
        attr(value)
    ))
}

/// Session exclusivity change, expressed as a `config` command.
pub fn set_exclusivity(e: Exclusivity) -> String {
    config(&format!("<session_exlusive  value='{}'/>", e.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_matches_client_literal() {
        // The Java client string-compares against exactly this.
        assert_eq!(
            disconnect(),
            "<commands><command name='disconnect'/></commands>"
        );
    }

    #[test]
    fn connect_matches_client_template() {
        let s = connect(&Connect {
            ip: "10.0.0.5",
            username: "admin",
            password: "access",
            switch_port: "1",
            tcp_port: "900",
            exclusivity: Exclusivity::Shared,
        });
        assert_eq!(
            s,
            "<commands><command name='connect' ip='10.0.0.5' username='admin' \
             password='access' switch_port='1' tcp_port='900' \
             session_exlusive='false'/></commands>"
                .replace("             ", "")
        );
    }

    #[test]
    fn attributes_are_escaped() {
        let s = connect(&Connect {
            ip: "10.0.0.5",
            username: "ad'min",
            password: "a&b",
            switch_port: "1",
            tcp_port: "900",
            exclusivity: Exclusivity::Exclusive,
        });
        assert!(s.contains("username='ad&apos;min'"));
        assert!(s.contains("password='a&amp;b'"));
        assert!(s.contains("session_exlusive='true'"));
    }

    #[test]
    fn vendor_typo_is_preserved() {
        // 'exlusive', not 'exclusive' — the device parses the misspelling.
        assert!(set_exclusivity(Exclusivity::Exclusive).contains("session_exlusive"));
        assert!(!set_exclusivity(Exclusivity::Exclusive).contains("session_exclusive"));
    }

    /// The exact document `AdvancedVideoCmd` builds, one attribute at a time.
    #[test]
    fn a_video_setting_matches_the_client_literal() {
        assert_eq!(
            set_video_setting(VideoSetting::HOffset, "364"),
            "<commands><command name='config'><config>\
             <video_settings hoffset='364'/></config></command></commands>"
        );
    }

    #[test]
    fn every_filter_round_trips_through_its_wire_name() {
        for f in [
            VideoFilter::Auto,
            VideoFilter::None,
            VideoFilter::Low,
            VideoFilter::Medium,
            VideoFilter::High,
        ] {
            assert_eq!(VideoFilter::parse(f.as_str()), Some(f));
        }
        assert_eq!(VideoFilter::parse("sharpen"), None);
    }

    /// The names are the device's, and it parses them literally.
    #[test]
    fn video_setting_names_are_the_vendors() {
        assert_eq!(VideoSetting::HOffset.as_str(), "hoffset");
        assert_eq!(VideoSetting::VOffset.as_str(), "voffset");
        assert_eq!(VideoFilter::None.as_str(), "nofilter");
    }

    #[test]
    fn simple_commands_match_client_literals() {
        assert_eq!(
            video_refresh(),
            "<commands><command name='video_refresh'/></commands>"
        );
        assert_eq!(
            mouse_pointer(MousePointer::Dot),
            "<commands><command name='mouse_pointer' type='dot'/></commands>"
        );
        assert_eq!(
            serial_close(),
            "<commands><command name='serial' type='close' external='true'/></commands>"
        );
    }
}
