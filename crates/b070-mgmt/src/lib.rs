//! Management client for Tripp Lite NetCommander / Minicom Smart IP KVM switches.
//!
//! Current-firmware protocol, verified against a live B070-008-19-IP
//! (firmware 2.2.1258.1.0) on 2026-09-19. All management goes through a
//! single endpoint over TLS:
//!
//! ```text
//! POST https://<device>/message.pv
//! ```
//!
//! The body is a `CMA_Message` XML document. There are three verbs:
//!
//! | `name`          | purpose                          |
//! |-----------------|----------------------------------|
//! | `CMA_AUTHORIZE` | log in (get a `SESSIONID`) / log out |
//! | `CMA_REQUEST`   | read config / status / switches  |
//! | `CMA_ACTION`    | reboot, power control            |
//!
//! Authentication returns a `SESSIONID` that every subsequent request carries
//! as an attribute. The client is synchronous — each call is one short
//! request/response — see [`transport`] for why it uses OpenSSL directly rather
//! than a normal HTTP client.
//!
//! The command bodies here are lifted verbatim from the decompiled device
//! client (`com.minicom.app.config.controller.http.pv.*`), preserved under
//! `research/decompiled/app-live/`.

pub mod model;
pub mod transport;

pub use model::{
    parse_auth_reply, parse_config, parse_status, AuthReply, DeviceConfig, NetworkAdapter,
    PortStatus, Status, Target, User,
};
pub use transport::Endpoint;

use std::time::Duration;

/// XML-escape a value for a double-quoted attribute.
///
/// The stock client does no escaping — it concatenates strings — so a password
/// containing `"` or `&` would break its request. We escape properly.
fn attr(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Power actions for a target mapped to an IP-PDU outlet.
///
/// Strings match the device client's `PowerTargetHttpCmd` (`cycle`/`up`/`down`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerCommand {
    Cycle,
    Up,
    Down,
}

impl PowerCommand {
    fn as_str(self) -> &'static str {
        match self {
            PowerCommand::Cycle => "cycle",
            PowerCommand::Up => "up",
            PowerCommand::Down => "down",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS: {0}")]
    Tls(String),

    #[error("OpenSSL: {0}")]
    OpenSsl(#[from] openssl::error::ErrorStack),

    #[error("device returned HTTP {0}")]
    HttpStatus(u16),

    #[error("malformed HTTP response: {0}")]
    MalformedHttp(&'static str),

    #[error("XML parse: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("XML attribute: {0}")]
    XmlAttr(#[from] quick_xml::events::attributes::AttrError),

    #[error("XML encoding: {0}")]
    XmlEncoding(#[from] quick_xml::encoding::EncodingError),

    #[error("login failed: {0}")]
    LoginFailed(String),

    #[error("not authenticated; call authenticate() first")]
    NotAuthenticated,

    #[error("device reported: {0}")]
    Device(String),
}

impl From<openssl::ssl::HandshakeError<std::net::TcpStream>> for Error {
    fn from(e: openssl::ssl::HandshakeError<std::net::TcpStream>) -> Self {
        Error::Tls(e.to_string())
    }
}

/// A management client bound to one device.
///
/// Construct with [`Client::connect`], which also authenticates.
pub struct Client {
    endpoint: Endpoint,
    session_id: String,
    /// Facts the device volunteered at login.
    auth: AuthReply,
}

impl Client {
    /// Connect and authenticate in one step.
    pub fn connect(host: &str, username: &str, password: &str) -> Result<Self, Error> {
        Client::connect_with(
            host,
            b070_proto::DEFAULT_HTTPS_PORT,
            username,
            password,
            Duration::from_secs(20),
        )
    }

    /// Connect and authenticate with an explicit port and timeout.
    pub fn connect_with(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let mut endpoint = Endpoint::new(host, port);
        endpoint.timeout = timeout;

        let body = format!(
            "<CMA_Message name=\"CMA_AUTHORIZE\"><user name=\"{}\" password=\"{}\"/></CMA_Message>",
            attr(username),
            attr(password),
        );
        let xml = transport::post_message_pv(&endpoint, &body)?;
        let reply = parse_auth_reply(&xml)?;

        if !reply.is_ok() {
            let msg = reply
                .error
                .as_ref()
                .map(|(d, id)| format!("{d} (id {id})"))
                .unwrap_or_else(|| "device rejected the login".to_string());
            return Err(Error::LoginFailed(msg));
        }
        let session_id = reply
            .session_id
            .clone()
            .ok_or_else(|| Error::LoginFailed("device returned no session id".into()))?;

        Ok(Client {
            endpoint,
            session_id,
            auth: reply,
        })
    }

    /// Facts the device reported at login (hardware family, capabilities, …).
    pub fn auth_info(&self) -> &AuthReply {
        &self.auth
    }

    /// The active session token. This is also what the KVM session socket's
    /// preamble needs.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn request(&self, inner: &str) -> Result<String, Error> {
        let body = format!(
            "<CMA_Message name=\"CMA_REQUEST\" SESSIONID=\"{}\">{}</CMA_Message>",
            attr(&self.session_id),
            inner,
        );
        let xml = transport::post_message_pv(&self.endpoint, &body)?;
        if let Some(err) = model::error_in(&xml) {
            return Err(Error::Device(err));
        }
        Ok(xml)
    }

    fn action(&self, inner: &str) -> Result<String, Error> {
        let body = format!(
            "<CMA_Message name=\"CMA_ACTION\" SESSIONID=\"{}\">{}</CMA_Message>",
            attr(&self.session_id),
            inner,
        );
        let xml = transport::post_message_pv(&self.endpoint, &body)?;
        if let Some(err) = model::error_in(&xml) {
            return Err(Error::Device(err));
        }
        Ok(xml)
    }

    /// Fetch the full device configuration.
    pub fn config(&self) -> Result<DeviceConfig, Error> {
        let xml = self.request("<request_data value=\"config\" type=\"full\"/>")?;
        parse_config(&xml)
    }

    /// Fetch runtime status (firmware, LAN, per-port state).
    pub fn status(&self) -> Result<Status, Error> {
        let xml =
            self.request("<request_data value=\"status\" type=\"full\" changetime=\"0\"/>")?;
        parse_status(&xml)
    }

    /// The configured KVM targets (ports and names).
    pub fn targets(&self) -> Result<Vec<Target>, Error> {
        Ok(self.config()?.targets)
    }

    /// Fetch a document as raw XML, for debugging or fields not yet modelled.
    ///
    /// `value` is the `request_data value=` selector, e.g. `"config"`,
    /// `"status"`, `"switch"`.
    pub fn fetch_raw(&self, value: &str) -> Result<String, Error> {
        self.request(&format!("<request_data value=\"{}\"/>", attr(value)))
    }

    /// Power-control a target's IP-PDU outlet.
    pub fn power(&self, port: u32, command: PowerCommand) -> Result<(), Error> {
        self.action(&format!(
            "<action type=\"power\" command=\"{}\" port=\"{}\"/>",
            command.as_str(),
            port,
        ))?;
        Ok(())
    }

    /// Reboot the device. Drops every active session.
    pub fn reboot(&self) -> Result<(), Error> {
        self.action("<action type=\"reboot\"/>")?;
        Ok(())
    }

    /// Log out, ending the session on the device.
    pub fn logout(self) -> Result<(), Error> {
        let body = format!(
            "<CMA_Message name=\"CMA_AUTHORIZE\" SESSIONID=\"{}\"></CMA_Message>",
            attr(&self.session_id),
        );
        transport::post_message_pv(&self.endpoint, &body)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_credentials_in_auth_body() {
        // Building the body must survive a password with XML metacharacters.
        let body = format!(
            "<CMA_Message name=\"CMA_AUTHORIZE\"><user name=\"{}\" password=\"{}\"/></CMA_Message>",
            attr("ad\"min"),
            attr("a&b<c"),
        );
        assert!(body.contains("name=\"ad&quot;min\""));
        assert!(body.contains("password=\"a&amp;b&lt;c\""));
    }

    #[test]
    fn power_command_strings() {
        assert_eq!(PowerCommand::Cycle.as_str(), "cycle");
        assert_eq!(PowerCommand::Up.as_str(), "up");
        assert_eq!(PowerCommand::Down.as_str(), "down");
    }
}
