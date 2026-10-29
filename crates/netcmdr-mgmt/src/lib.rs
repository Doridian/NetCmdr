// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Management client for Tripp Lite NetCommander / Minicom Smart IP KVM switches.
//!
//! Verified against a B070-008-19-IP (firmware 2.2.1258.1.0). Everything goes
//! through one endpoint:
//!
//! ```text
//! POST https://<device>/message.pv
//! ```
//!
//! The body is a `CMA_Message` XML document:
//!
//! | `name`           | purpose                                              |
//! |------------------|------------------------------------------------------|
//! | `CMA_AUTHORIZE`  | log in (get a `SESSIONID`) / log out                  |
//! | `CMA_REQUEST`    | read config / status / switches                       |
//! | `CMA_ACTION`     | reboot, power, lock, disconnect, backup, factory reset |
//! | `CMA_PERMISSION` | take and release the exclusive right to write config  |
//! | `CMA_CONFIG`     | write the configuration back                          |
//! | `CMA_POST`       | upload a file: firmware, SIU firmware, TLS certificate |
//!
//! Login returns a `SESSIONID` carried by every later request. The client is
//! synchronous; see [`transport`] for why it uses OpenSSL directly.

pub mod config;
pub mod model;
pub mod request;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
pub mod xml;

pub use config::{
    AuthMethod, ConfigDoc, Ipv6Mode, LdapMode, LdapProtocol, Parity, SerialAssign, SystemTime,
    BAUD_RATES, DATA_BITS, STOP_BITS, USER_LEVEL_ADMIN, USER_LEVEL_USER,
};
pub use model::{
    parse_auth_reply, parse_config, parse_status, AuthReply, DeviceConfig, NetworkAdapter,
    PortStatus, Status, Target, User,
};
#[cfg(not(target_arch = "wasm32"))]
pub use transport::Endpoint;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

/// XML-escape for a double-quoted attribute. (The stock client doesn't.)
pub(crate) fn attr(v: &str) -> String {
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

/// IP-PDU outlet power action (`PowerTargetHttpCmd`).
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

/// Exclusive config write access (`CMA_PERMISSION`), held around a save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Config,
    /// No exclusive access.
    Normal,
}

#[cfg(not(target_arch = "wasm32"))]
impl Permission {
    fn as_str(self) -> &'static str {
        match self {
            Permission::Config => "config",
            Permission::Normal => "normal",
        }
    }
}

/// Result of a config save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveOutcome {
    /// The device is rebooting (network and port changes); the session is gone.
    pub rebooting: bool,
}

/// Progress of a firmware upgrade, from `<device_upgrade>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeStatus {
    /// `"none"`, `"upgrading"`, `"finished"` or `"failed"`.
    pub status: String,
    pub percent_complete: Option<u32>,
}

/// Progress of one SIU's firmware upgrade, from `<target>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiuUpgradeStatus {
    pub port: u32,
    /// `"upgrading"`, `"finished"` or `"failed"`.
    pub status: String,
    pub percent_complete: Option<u32>,
}

/// The firmware and hardware revision of the SIU on one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiuVersion {
    pub port: u32,
    /// e.g. `20373 0.2 20372 2.0 ID00080207`.
    pub hardware: Option<String>,
    /// e.g. `RoC II USB En v4.0 C8 B225 12/03/2013`.
    pub firmware: Option<String>,
}

impl SiuVersion {
    /// False when the device reports `"Not responding"`.
    pub fn responding(&self) -> bool {
        self.hardware.is_some() || self.firmware.is_some()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS: {0}")]
    Tls(String),

    #[cfg(not(target_arch = "wasm32"))]
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

    /// A config problem found locally, not by the device.
    #[error("{0}")]
    Config(String),
}

#[cfg(not(target_arch = "wasm32"))]
impl From<openssl::ssl::HandshakeError<std::net::TcpStream>> for Error {
    fn from(e: openssl::ssl::HandshakeError<std::net::TcpStream>) -> Self {
        Error::Tls(e.to_string())
    }
}

/// Native management client with a blocking TLS transport. The web client uses
/// [`crate::request`] and [`crate::model`] over `fetch` instead.
#[cfg(not(target_arch = "wasm32"))]
pub struct Client {
    endpoint: Endpoint,
    session_id: String,
    auth: AuthReply,
}

#[cfg(not(target_arch = "wasm32"))]
impl Client {
    /// Connect and log in.
    pub fn connect(host: &str, username: &str, password: &str) -> Result<Self, Error> {
        Client::connect_with(
            host,
            netcmdr_proto::DEFAULT_HTTPS_PORT,
            username,
            password,
            Duration::from_secs(20),
        )
    }

    pub fn connect_with(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let mut endpoint = Endpoint::new(host, port);
        endpoint.timeout = timeout;

        let xml = transport::post_message_pv(&endpoint, &request::authorize(username, password))?;
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

    /// Device info from login.
    pub fn auth_info(&self) -> &AuthReply {
        &self.auth
    }

    /// Also used by the KVM session handshake.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a `CMA_Message`; a `reply_error` becomes [`Error::Device`].
    /// `timeout` overrides the default for slow calls (the vendor allows 60 s
    /// for a save, 45 s for a fetch).
    fn message(&self, verb: &str, inner: &str, timeout: Option<Duration>) -> Result<String, Error> {
        let body = request::envelope(verb, &self.session_id, inner);
        let mut endpoint = self.endpoint.clone();
        if let Some(t) = timeout {
            endpoint.timeout = t.max(self.endpoint.timeout);
        }
        // Unredacted: TRACE logs contain every credential the device holds.
        tracing::trace!(verb, body = %inner, "sending");
        let xml = transport::post_message_pv(&endpoint, &body)?;
        tracing::trace!(verb, reply = %xml, "received");
        if let Some(err) = model::error_in(&xml) {
            return Err(Error::Device(err));
        }
        Ok(xml)
    }

    fn request(&self, inner: &str) -> Result<String, Error> {
        self.message("CMA_REQUEST", inner, Some(Duration::from_secs(45)))
    }

    fn action(&self, inner: &str) -> Result<String, Error> {
        self.message("CMA_ACTION", inner, None)
    }

    /// Raw config XML, from which both typed and editable views can be parsed.
    pub fn config_raw(&self) -> Result<String, Error> {
        self.request("<request_data value=\"config\" type=\"full\"/>")
    }

    pub fn config(&self) -> Result<DeviceConfig, Error> {
        parse_config(&self.config_raw()?)
    }

    /// Runtime status (firmware, LAN, per-port state).
    pub fn status(&self) -> Result<Status, Error> {
        let xml =
            self.request("<request_data value=\"status\" type=\"full\" changetime=\"0\"/>")?;
        parse_status(&xml)
    }

    pub fn targets(&self) -> Result<Vec<Target>, Error> {
        Ok(self.config()?.targets)
    }

    /// Raw XML by `request_data value=` selector, e.g. `"config"`, `"status"`.
    pub fn fetch_raw(&self, value: &str) -> Result<String, Error> {
        self.request(&format!("<request_data value=\"{}\"/>", attr(value)))
    }

    pub fn power(&self, port: u32, command: PowerCommand) -> Result<(), Error> {
        self.action(&format!(
            "<action type=\"power\" command=\"{}\" port=\"{}\"/>",
            command.as_str(),
            port,
        ))?;
        Ok(())
    }

    /// Drops every active session.
    pub fn reboot(&self) -> Result<(), Error> {
        self.action("<action type=\"reboot\"/>")?;
        Ok(())
    }

    /// Editable config; modify and pass to [`Client::save_config`].
    pub fn config_doc(&self) -> Result<ConfigDoc, Error> {
        ConfigDoc::parse(&self.config_raw()?)
    }

    /// Take or release exclusive config access (`CMA_PERMISSION`). Methods that
    /// need it take it themselves; call this only to clear a stale lock.
    pub fn permission(&self, level: Permission) -> Result<(), Error> {
        self.message(
            "CMA_PERMISSION",
            &format!("<permission level=\"{}\"/>", level.as_str()),
            None,
        )?;
        Ok(())
    }

    /// Run a request with exclusive config access, releasing it even on
    /// failure (a release error is only logged). `release` is false for
    /// requests that reboot the device.
    fn holding_config_permission<T>(
        &self,
        release: bool,
        f: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.permission(Permission::Config)?;
        let out = f();
        if release || out.is_err() {
            if let Err(e) = self.permission(Permission::Normal) {
                tracing::warn!(error = %e, "could not release config permission");
            }
        }
        out
    }

    /// Write the configuration back, holding exclusive access as the vendor
    /// client does.
    ///
    /// Every setting is applied, so fetch with [`Client::config_doc`] just
    /// before saving.
    pub fn save_config(&self, doc: &ConfigDoc) -> Result<SaveOutcome, Error> {
        self.permission(Permission::Config)?;

        let saved = self.message(
            "CMA_CONFIG",
            &doc.to_save_body(),
            Some(Duration::from_secs(60)),
        );

        let xml = match saved {
            Ok(xml) => xml,
            Err(e) => {
                if let Err(release) = self.permission(Permission::Normal) {
                    tracing::warn!(error = %release, "could not release config permission");
                }
                return Err(e);
            }
        };

        let rebooting = model::reboot_in_progress(&xml);
        if !rebooting {
            self.permission(Permission::Normal)?;
        }
        Ok(SaveOutcome { rebooting })
    }

    /// Lock every other user out of the device (`systemlock`).
    pub fn lock_system(&self) -> Result<(), Error> {
        self.action("<action type=\"systemlock\"/>")?;
        Ok(())
    }

    /// Release a [`Client::lock_system`] lock.
    pub fn unlock_system(&self) -> Result<(), Error> {
        self.action("<action type=\"systemunlock\"/>")?;
        Ok(())
    }

    /// Disconnect whoever is holding a KVM port.
    pub fn disconnect_port(&self, port: u32) -> Result<(), Error> {
        self.action(&format!("<action type=\"disconnect\" port=\"{port}\"/>"))?;
        Ok(())
    }

    /// Download an opaque configuration backup.
    pub fn backup(&self) -> Result<String, Error> {
        let xml = self.holding_config_permission(true, || {
            self.message(
                "CMA_ACTION",
                "<action type=\"backup\"/>",
                Some(Duration::from_secs(60)),
            )
        })?;
        model::reply_data(&xml).ok_or_else(|| Error::Device("backup reply carried no data".into()))
    }

    /// Restore a [`Client::backup`]. `preserve` keeps network settings. The
    /// device reboots.
    pub fn restore(&self, backup: &str, preserve: bool) -> Result<(), Error> {
        let body = format!(
            "<action type=\"restore_from_backup\" preserve=\"{}\"/><data><![CDATA[{}]]></data>",
            preserve,
            cdata_safe(backup)?,
        );
        self.holding_config_permission(false, || {
            self.message("CMA_ACTION", &body, Some(Duration::from_secs(120)))
        })?;
        Ok(())
    }

    /// Reset to factory defaults, optionally keeping the network settings.
    /// The device reboots.
    pub fn factory_restore(&self, preserve_network: bool) -> Result<(), Error> {
        let body = format!("<action type=\"restore\" preserve=\"{preserve_network}\"/>");
        self.holding_config_permission(false, || {
            self.message("CMA_ACTION", &body, Some(Duration::from_secs(60)))
        })?;
        Ok(())
    }

    /// Install a PEM certificate and key. `key_password` is `""` if the key is
    /// unencrypted. The device reboots.
    ///
    /// The vendor puts the certificate in `<private>` and the key in `<public>`
    /// (`InstallSslHttpCmd`); we match it. Without config permission the device
    /// answers "INVALID XML, cmd not supported".
    pub fn install_certificate(
        &self,
        certificate: &str,
        private_key: &str,
        key_password: &str,
    ) -> Result<(), Error> {
        let body = format!(
            "<post_data value=\"ssl_certificate\"><private password=\"{}\"><![CDATA[{}]]></private><public><![CDATA[{}]]></public></post_data>",
            attr(key_password),
            cdata_safe(certificate)?,
            cdata_safe(private_key)?,
        );
        self.holding_config_permission(false, || {
            self.message("CMA_POST", &body, Some(Duration::from_secs(60)))
        })?;
        Ok(())
    }

    /// Upload firmware and start the upgrade; poll [`Client::upgrade_status`].
    /// **Untested on hardware.**
    pub fn upgrade_device(&self, image: &str) -> Result<(), Error> {
        self.message(
            "CMA_POST",
            &format!(
                "<post_data value=\"upgrade\"><![CDATA[{}]]></post_data>",
                cdata_safe(image)?
            ),
            Some(Duration::from_secs(300)),
        )?;
        Ok(())
    }

    /// Progress of a device firmware upgrade.
    pub fn upgrade_status(&self) -> Result<UpgradeStatus, Error> {
        model::parse_upgrade_status(&self.request("<request_data value=\"status\"/>")?)
    }

    /// Upload SIU firmware and upgrade `ports`. **Untested on hardware.**
    pub fn upgrade_sius(&self, image: &str, ports: &[u32]) -> Result<(), Error> {
        self.message(
            "CMA_POST",
            &format!(
                "<post_data value=\"roc\" ports=\"{}\"><![CDATA[{}]]></post_data>",
                join_u32(ports),
                cdata_safe(image)?,
            ),
            Some(Duration::from_secs(300)),
        )?;
        Ok(())
    }

    /// Per-port progress of a SIU firmware upgrade.
    pub fn siu_upgrade_status(&self) -> Result<Vec<SiuUpgradeStatus>, Error> {
        model::parse_siu_upgrade_status(&self.message(
            "CMA_CONFIG",
            "<config_data value=\"update\" operation=\"status\"></config_data>",
            None,
        )?)
    }

    /// SIU firmware and hardware revision on one port. Takes a second or two;
    /// an empty port gives an empty [`SiuVersion`].
    ///
    /// **One port per request**: the device ignores all but the first `<port>`
    /// (measured on 2.2.1258.1.0), though the vendor client sends several.
    pub fn siu_version(&self, port: u32) -> Result<SiuVersion, Error> {
        let xml = self.message(
            "CMA_ACTION",
            &format!("<action type=\"roc_version\"/><port number=\"{port}\"/>"),
            Some(Duration::from_secs(60)),
        )?;
        model::parse_siu_versions(&xml)?
            .into_iter()
            .find(|v| v.port == port)
            .ok_or_else(|| Error::Device(format!("device said nothing about port {port}")))
    }

    /// [`Client::siu_version`] per port (~30 s for 16 ports).
    pub fn siu_versions(&self, ports: &[u32]) -> Result<Vec<SiuVersion>, Error> {
        ports.iter().map(|&p| self.siu_version(p)).collect()
    }

    pub fn logout(self) -> Result<(), Error> {
        transport::post_message_pv(&self.endpoint, &request::logout(&self.session_id))?;
        Ok(())
    }
}

/// Reject payloads containing `]]>`, which would end the CDATA section.
#[cfg(not(target_arch = "wasm32"))]
fn cdata_safe(payload: &str) -> Result<&str, Error> {
    if payload.contains("]]>") {
        return Err(Error::Config(
            "payload contains \"]]>\", which cannot be sent inside CDATA".into(),
        ));
    }
    Ok(payload)
}

#[cfg(not(target_arch = "wasm32"))]
fn join_u32(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_credentials_in_auth_body() {
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
