// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `CMA_Message` body builders, transport-free so the web client can reuse
//! them. Byte-identical to `com.minicom.app.config.controller.http.pv.*`, since
//! the device parses them literally.

use crate::{attr, PowerCommand};

/// Log in. The reply carries the `SESSIONID`.
pub fn authorize(username: &str, password: &str) -> String {
    format!(
        "<CMA_Message name=\"CMA_AUTHORIZE\"><user name=\"{}\" password=\"{}\"/></CMA_Message>",
        attr(username),
        attr(password),
    )
}

/// Log out: the login verb with a session and no user.
pub fn logout(session_id: &str) -> String {
    format!(
        "<CMA_Message name=\"CMA_AUTHORIZE\" SESSIONID=\"{}\"></CMA_Message>",
        attr(session_id),
    )
}

pub fn envelope(verb: &str, session_id: &str, inner: &str) -> String {
    format!(
        "<CMA_Message name=\"{}\" SESSIONID=\"{}\">{}</CMA_Message>",
        verb,
        attr(session_id),
        inner,
    )
}

/// `value` is the `request_data value=` selector.
pub fn request(session_id: &str, inner: &str) -> String {
    envelope("CMA_REQUEST", session_id, inner)
}

pub fn action(session_id: &str, inner: &str) -> String {
    envelope("CMA_ACTION", session_id, inner)
}

/// The full config, including the target list.
pub fn config(session_id: &str) -> String {
    request(session_id, "<request_data value=\"config\" type=\"full\"/>")
}

/// Runtime status: firmware, LAN, per-port state.
pub fn status(session_id: &str) -> String {
    request(
        session_id,
        "<request_data value=\"status\" type=\"full\" changetime=\"0\"/>",
    )
}

/// By selector, e.g. `"config"`, `"status"`, `"switch"`.
pub fn fetch(session_id: &str, value: &str) -> String {
    request(
        session_id,
        &format!("<request_data value=\"{}\"/>", attr(value)),
    )
}

pub fn power(session_id: &str, port: u32, command: PowerCommand) -> String {
    action(
        session_id,
        &format!(
            "<action type=\"power\" command=\"{}\" port=\"{}\"/>",
            command.as_str(),
            port,
        ),
    )
}

/// Drops every active session.
pub fn reboot(session_id: &str) -> String {
    action(session_id, "<action type=\"reboot\"/>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_carries_the_credentials_and_no_session() {
        let body = authorize("admin", "access");
        assert!(body.contains("name=\"CMA_AUTHORIZE\""));
        assert!(body.contains("<user name=\"admin\" password=\"access\"/>"));
        assert!(!body.contains("SESSIONID"));
    }

    /// The stock client doesn't escape these.
    #[test]
    fn credentials_are_escaped() {
        let body = authorize("a&b", "p\"<>");
        assert!(body.contains("name=\"a&amp;b\""));
        assert!(body.contains("password=\"p&quot;&lt;&gt;\""));
    }

    /// Base64 session tokens can contain markup characters.
    #[test]
    fn the_session_token_is_escaped_too() {
        let body = config("a\"b&c");
        assert!(body.contains("SESSIONID=\"a&quot;b&amp;c\""));
    }

    #[test]
    fn power_names_the_action_the_device_expects() {
        let body = power("tok", 3, PowerCommand::Cycle);
        assert!(body.contains("name=\"CMA_ACTION\""));
        assert!(body.contains("<action type=\"power\" command=\"cycle\" port=\"3\"/>"));
    }

    #[test]
    fn a_logout_is_an_authorize_with_a_session_and_no_user() {
        let body = logout("tok");
        assert!(body.contains("name=\"CMA_AUTHORIZE\""));
        assert!(body.contains("SESSIONID=\"tok\""));
        assert!(!body.contains("<user"));
    }
}
