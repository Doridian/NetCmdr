// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
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
//! The body is a `CMA_Message` XML document. There are five verbs:
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
//! Authentication returns a `SESSIONID` that every subsequent request carries
//! as an attribute. The client is synchronous — each call is one short
//! request/response — see [`transport`] for why it uses OpenSSL directly rather
//! than a normal HTTP client.
//!
//! The request bodies here match the ones the device's own client sends
//! (`com.minicom.app.config.controller.http.pv.*`), byte for byte, because the
//! device's parser is literal about them.

pub mod config;
pub mod model;
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

/// The exclusive right to write configuration, taken and released around a
/// save (`CMA_PERMISSION`). Only one session may hold `Config` at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Exclusive configuration access.
    Config,
    /// The ordinary state: no exclusive access held.
    Normal,
}

impl Permission {
    fn as_str(self) -> &'static str {
        match self {
            Permission::Config => "config",
            Permission::Normal => "normal",
        }
    }
}

/// What the device did with a configuration save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveOutcome {
    /// The device is rebooting to apply the change, so the session is gone.
    /// Network and TCP-port changes do this.
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
    pub hardware: Option<String>,
    pub firmware: Option<String>,
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

    /// Something is wrong with the configuration or with a value for it, found
    /// here rather than reported by the device.
    #[error("{0}")]
    Config(String),
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

    /// Send one `CMA_Message` and return the reply, turning a `reply_error`
    /// into an [`Error::Device`].
    ///
    /// `timeout` overrides the client's default for the calls the device is
    /// slow at: the vendor's client allows a save 60 s and a fetch 45 s, and
    /// an upload takes as long as the flash write does.
    fn message(&self, verb: &str, inner: &str, timeout: Option<Duration>) -> Result<String, Error> {
        let body = format!(
            "<CMA_Message name=\"{}\" SESSIONID=\"{}\">{}</CMA_Message>",
            verb,
            attr(&self.session_id),
            inner,
        );
        let mut endpoint = self.endpoint.clone();
        if let Some(t) = timeout {
            endpoint.timeout = t.max(self.endpoint.timeout);
        }
        let xml = transport::post_message_pv(&endpoint, &body)?;
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

    /// Fetch the full device configuration as raw XML.
    ///
    /// Both views of the configuration come from this one request, so a caller
    /// that needs the typed view *and* the editable document can take them from
    /// a single round trip instead of two.
    pub fn config_raw(&self) -> Result<String, Error> {
        self.request("<request_data value=\"config\" type=\"full\"/>")
    }

    /// Fetch the full device configuration.
    pub fn config(&self) -> Result<DeviceConfig, Error> {
        parse_config(&self.config_raw()?)
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

    /// Fetch the configuration as an editable document.
    ///
    /// This is the read half of every change: mutate the returned
    /// [`ConfigDoc`] and hand it to [`Client::save_config`].
    pub fn config_doc(&self) -> Result<ConfigDoc, Error> {
        ConfigDoc::parse(&self.config_raw()?)
    }

    /// Take or release exclusive configuration access (`CMA_PERMISSION`).
    ///
    /// [`Client::save_config`] does this around a save; call it directly only
    /// to release a permission a crashed run left behind.
    pub fn permission(&self, level: Permission) -> Result<(), Error> {
        self.message(
            "CMA_PERMISSION",
            &format!("<permission level=\"{}\"/>", level.as_str()),
            None,
        )?;
        Ok(())
    }

    /// Write the configuration back.
    ///
    /// The device takes exclusive configuration access first and gives it back
    /// afterwards, exactly as the vendor's client does — without the release,
    /// the device stays locked against the next writer until its own timeout.
    /// The release is skipped when the save triggers a reboot, because the
    /// session is already gone by then.
    ///
    /// Every setting in the document is applied, not just the ones that
    /// changed, so build the document with [`Client::config_doc`] immediately
    /// before saving rather than reusing an old one.
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
                // Do not leave the device locked because the save failed.
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

    /// Download a configuration backup.
    ///
    /// The payload is opaque: the device's own client writes it to a file
    /// verbatim and hands it back on a restore without looking inside.
    pub fn backup(&self) -> Result<String, Error> {
        let xml = self.message(
            "CMA_ACTION",
            "<action type=\"backup\"/>",
            Some(Duration::from_secs(60)),
        )?;
        model::reply_data(&xml).ok_or_else(|| Error::Device("backup reply carried no data".into()))
    }

    /// Restore a backup taken with [`Client::backup`].
    ///
    /// `preserve` keeps the device's network settings, so a backup from one
    /// unit can be restored onto another without taking it off the network.
    /// The device reboots when this succeeds.
    pub fn restore(&self, backup: &str, preserve: bool) -> Result<(), Error> {
        self.message(
            "CMA_ACTION",
            &format!(
                "<action type=\"restore_from_backup\" preserve=\"{}\"/><data><![CDATA[{}]]></data>",
                preserve,
                cdata_safe(backup)?,
            ),
            Some(Duration::from_secs(120)),
        )?;
        Ok(())
    }

    /// Reset to factory defaults, optionally keeping the network settings.
    /// The device reboots.
    pub fn factory_restore(&self, preserve_network: bool) -> Result<(), Error> {
        self.message(
            "CMA_ACTION",
            &format!("<action type=\"restore\" preserve=\"{preserve_network}\"/>"),
            Some(Duration::from_secs(60)),
        )?;
        Ok(())
    }

    /// Install a TLS certificate and its private key on the management server.
    ///
    /// `key_password` is the passphrase on the key, or `""` for an unencrypted
    /// one. Both arguments are PEM text.
    ///
    /// Note the vendor's client puts the *certificate* inside `<private>` and
    /// the *key* inside `<public>` (`InstallSslHttpCmd`); the element names are
    /// simply the wrong way round, and this matches it so the device sees what
    /// it expects.
    pub fn install_certificate(
        &self,
        certificate: &str,
        private_key: &str,
        key_password: &str,
    ) -> Result<(), Error> {
        self.message(
            "CMA_POST",
            &format!(
                "<post_data value=\"ssl_certificate\"><private password=\"{}\"><![CDATA[{}]]></private><public><![CDATA[{}]]></public></post_data>",
                attr(key_password),
                cdata_safe(certificate)?,
                cdata_safe(private_key)?,
            ),
            Some(Duration::from_secs(60)),
        )?;
        Ok(())
    }

    /// Upload a firmware image and start the upgrade.
    ///
    /// Poll [`Client::upgrade_status`] for progress. **Untested against
    /// hardware.**
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

    /// Upload SIU (Server Interface Unit) firmware and upgrade the units on
    /// `ports`. **Untested against hardware.**
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

    /// Read the firmware and hardware revision of the SIUs on `ports`.
    ///
    /// The device has to talk to each unit over the Cat5 link, so this is slow
    /// and only answers for ports that actually have a SIU.
    pub fn siu_versions(&self, ports: &[u32]) -> Result<Vec<SiuVersion>, Error> {
        let mut inner = String::from("<action type=\"roc_version\"/>");
        for p in ports {
            inner.push_str(&format!("<port number=\"{p}\"/>"));
        }
        model::parse_siu_versions(&self.message(
            "CMA_ACTION",
            &inner,
            Some(Duration::from_secs(60)),
        )?)
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

/// Check that a payload can go inside a CDATA section.
///
/// `]]>` would end the section early and let the rest of the payload be parsed
/// as markup. Nothing the device produces contains it, so refusing is better
/// than silently splitting the section.
fn cdata_safe(payload: &str) -> Result<&str, Error> {
    if payload.contains("]]>") {
        return Err(Error::Config(
            "payload contains \"]]>\", which cannot be sent inside CDATA".into(),
        ));
    }
    Ok(payload)
}

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
