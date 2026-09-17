//! The XML control vocabulary.
//!
//! These are the exact documents the stock client hands to
//! `KVMIPGate.setCommand()`. Every template here was lifted verbatim from the
//! decompiled `com.minicom.app.video.controller.cmds.*` classes — see
//! `research/decompiled/app/` and `research/notes/FINDINGS.md` §5.1.
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
    wrap(&format!(
        "<command name='mount_drive' id='{}'/>",
        attr(id)
    ))
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
    let _ = write!(s, "<command name='config'><config>{inner}</config></command>");
    wrap(&s)
}

/// Session exclusivity change, expressed as a `config` command.
pub fn set_exclusivity(e: Exclusivity) -> String {
    config(&format!(
        "<session_exlusive  value='{}'/>",
        e.as_str()
    ))
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
