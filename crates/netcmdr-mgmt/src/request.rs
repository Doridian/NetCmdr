// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `CMA_Message` bodies, as values.
//!
//! These are pure string builders with no transport attached, which is what
//! lets the web client reuse them. Natively [`crate::Client`] posts them over
//! its own TLS; in a browser the same strings go out through `fetch` to
//! `netcmdr-bridge`, which forwards them verbatim. Either way there is one
//! description of what the device is asked, and it is this one.
//!
//! The bodies match the device client's own
//! (`com.minicom.app.config.controller.http.pv.*`) byte for byte, because the
//! device's parser is literal about them.

use crate::{attr, PowerCommand};

/// Log in. The reply carries the `SESSIONID` everything else needs.
pub fn authorize(username: &str, password: &str) -> String {
    format!(
        "<CMA_Message name=\"CMA_AUTHORIZE\"><user name=\"{}\" password=\"{}\"/></CMA_Message>",
        attr(username),
        attr(password),
    )
}

/// Log out. The same verb as the login, distinguished by carrying a session
/// and no user.
pub fn logout(session_id: &str) -> String {
    format!(
        "<CMA_Message name=\"CMA_AUTHORIZE\" SESSIONID=\"{}\"></CMA_Message>",
        attr(session_id),
    )
}

/// Wrap `inner` in an authenticated envelope.
pub fn envelope(verb: &str, session_id: &str, inner: &str) -> String {
    format!(
        "<CMA_Message name=\"{}\" SESSIONID=\"{}\">{}</CMA_Message>",
        verb,
        attr(session_id),
        inner,
    )
}

/// Read something. `value` is the `request_data value=` selector.
pub fn request(session_id: &str, inner: &str) -> String {
    envelope("CMA_REQUEST", session_id, inner)
}

/// Do something.
pub fn action(session_id: &str, inner: &str) -> String {
    envelope("CMA_ACTION", session_id, inner)
}

/// The full configuration document, which is also where the target list lives.
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

/// Any document by selector, e.g. `"config"`, `"status"`, `"switch"`.
pub fn fetch(session_id: &str, value: &str) -> String {
    request(
        session_id,
        &format!("<request_data value=\"{}\"/>", attr(value)),
    )
}

/// Power-control a target's IP-PDU outlet.
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

/// Reboot the device. Drops every active session.
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

    /// The stock client concatenates strings and would break on these; we do
    /// not. A password is the likeliest place for one to turn up.
    #[test]
    fn credentials_are_escaped() {
        let body = authorize("a&b", "p\"<>");
        assert!(body.contains("name=\"a&amp;b\""));
        assert!(body.contains("password=\"p&quot;&lt;&gt;\""));
    }

    /// The session token is base64 and routinely contains characters that are
    /// markup-significant if they reach the document unescaped.
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
