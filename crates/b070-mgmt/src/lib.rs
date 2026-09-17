//! HTTPS management client for Tripp Lite NetCommander / Minicom Smart IP KVM switches.
//!
//! The device exposes a single ISAPI endpoint. Every request is a form POST to
//! `/config/` whose body selects the document:
//!
//! ```text
//! POST /config/
//! Content-Type: application/x-www-form-urlencoded
//!
//! client=ConfigXML
//! ```
//!
//! Recovered from the firmware's own web UI (`common.js`,
//! `RPMCONFIG.utils.XMLReq`) — see `research/notes/FINDINGS.md` §5.2.
//!
//! Plain HTTP is rejected by the device; only HTTPS works. The shipped
//! certificate is self-signed for `*.kvm.net`, so [`ClientBuilder::danger_accept_invalid_certs`]
//! is on by default — see its documentation for what that costs you.

pub mod model;

use std::time::Duration;

pub use model::{
    parse_config, parse_status, parse_switches, parse_users, Connection, DeviceConfig,
    NetworkAdapter, Status, SwitchDef, Target, User,
};

/// The documents the `/config/` endpoint serves, keyed by the `client=` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// Device configuration: network, connection, targets.
    Config,
    /// Local user accounts.
    Admin,
    /// Switch definition file.
    Switch,
    /// Runtime status: firmware version, last session.
    Status,
}

impl Resource {
    /// The `client=` value. Note `Status` carries the vendor's spaces —
    /// the web UI sends `client = StatusXML`, and we reproduce it verbatim
    /// rather than guess that the handler trims.
    pub fn client_param(self) -> &'static str {
        match self {
            Resource::Config => "client=ConfigXML",
            Resource::Admin => "client=AdminXML",
            Resource::Switch => "client=SwitchXML",
            Resource::Status => "client = StatusXML",
        }
    }
}

/// Actions the endpoint performs rather than documents it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// End the current session.
    LogOff,
    /// Reboot the device.
    Reboot,
}

impl Action {
    pub fn client_param(self) -> &'static str {
        match self {
            Action::LogOff => "client=LogOFF",
            Action::Reboot => "client=RPMDoReboot",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP transport: {0}")]
    Http(#[from] reqwest::Error),

    #[error("device returned HTTP {status} for {what}")]
    Status {
        status: reqwest::StatusCode,
        what: &'static str,
    },

    #[error("XML parse: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("XML attribute: {0}")]
    XmlAttr(#[from] quick_xml::events::attributes::AttrError),

    #[error("XML encoding: {0}")]
    XmlEncoding(#[from] quick_xml::encoding::EncodingError),

    #[error("invalid URL: {0}")]
    Url(String),
}

/// Builder for [`Client`].
pub struct ClientBuilder {
    host: String,
    port: u16,
    accept_invalid_certs: bool,
    timeout: Duration,
    credentials: Option<(String, String)>,
}

impl ClientBuilder {
    pub fn new(host: impl Into<String>) -> Self {
        ClientBuilder {
            host: host.into(),
            port: b070_proto::DEFAULT_HTTPS_PORT,
            // These devices ship a self-signed `*.kvm.net` certificate and have
            // no way to be issued a trusted one for an IP address. Refusing it
            // by default would make the client unusable out of the box; the
            // session protocol itself is likewise unauthenticated. Set this to
            // false once you have installed your own certificate on the device
            // and are addressing it by the matching name.
            accept_invalid_certs: true,
            timeout: Duration::from_secs(15),
            credentials: None,
        }
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Whether to accept the device's self-signed certificate. Defaults to
    /// `true`; see [`ClientBuilder::new`].
    pub fn danger_accept_invalid_certs(mut self, yes: bool) -> Self {
        self.accept_invalid_certs = yes;
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// HTTP Basic credentials.
    ///
    /// **[ASSUMED]** The web UI's own JavaScript posts no credentials, so the
    /// device most likely authenticates with a session cookie set by a login
    /// form we have not yet mapped. This hook exists so that once the real
    /// scheme is confirmed on hardware there is a place to put it.
    pub fn credentials(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.credentials = Some((user.into(), pass.into()));
        self
    }

    pub fn build(self) -> Result<Client, Error> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(self.accept_invalid_certs)
            .danger_accept_invalid_hostnames(self.accept_invalid_certs)
            .timeout(self.timeout)
            .build()?;
        Ok(Client {
            base: format!("https://{}:{}", self.host, self.port),
            http,
            credentials: self.credentials,
        })
    }
}

/// A management client bound to one device.
pub struct Client {
    base: String,
    http: reqwest::Client,
    credentials: Option<(String, String)>,
}

impl Client {
    pub fn builder(host: impl Into<String>) -> ClientBuilder {
        ClientBuilder::new(host)
    }

    /// The `https://host:port` this client talks to.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// POST a raw `client=…` body to `/config/` and return the response body.
    ///
    /// The trailing slash on `/config/` is mandatory — the web UI's own comment
    /// says so.
    pub async fn post_raw(&self, body: &'static str, what: &'static str) -> Result<String, Error> {
        let url = format!("{}/config/", self.base);
        tracing::debug!(%url, body, "POST /config/");

        let mut req = self
            .http
            .post(&url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::CACHE_CONTROL, "no-cache,max-age=0")
            .body(body);

        if let Some((u, p)) = &self.credentials {
            req = req.basic_auth(u, Some(p));
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Status { status, what });
        }
        Ok(resp.text().await?)
    }

    /// Fetch one document as raw XML.
    pub async fn fetch_raw(&self, r: Resource) -> Result<String, Error> {
        let what = match r {
            Resource::Config => "config",
            Resource::Admin => "admin",
            Resource::Switch => "switch",
            Resource::Status => "status",
        };
        self.post_raw(r.client_param(), what).await
    }

    /// Fetch and parse the device configuration.
    pub async fn config(&self) -> Result<DeviceConfig, Error> {
        parse_config(&self.fetch_raw(Resource::Config).await?)
    }

    /// Fetch and parse runtime status.
    pub async fn status(&self) -> Result<Status, Error> {
        parse_status(&self.fetch_raw(Resource::Status).await?)
    }

    /// Fetch and parse local user accounts.
    pub async fn users(&self) -> Result<Vec<User>, Error> {
        parse_users(&self.fetch_raw(Resource::Admin).await?)
    }

    /// Fetch and parse the switch definition file.
    pub async fn switches(&self) -> Result<Vec<SwitchDef>, Error> {
        parse_switches(&self.fetch_raw(Resource::Switch).await?)
    }

    /// The configured targets (KVM ports and their names).
    pub async fn targets(&self) -> Result<Vec<Target>, Error> {
        Ok(self.config().await?.targets)
    }

    /// Perform an action. Returns the device's raw response.
    pub async fn perform(&self, a: Action) -> Result<String, Error> {
        let what = match a {
            Action::LogOff => "logoff",
            Action::Reboot => "reboot",
        };
        self.post_raw(a.client_param(), what).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_params_match_the_web_ui_verbatim() {
        // Straight out of common.js — including the spaces around '=' that the
        // vendor's StatusXML call has and the others do not.
        assert_eq!(Resource::Config.client_param(), "client=ConfigXML");
        assert_eq!(Resource::Admin.client_param(), "client=AdminXML");
        assert_eq!(Resource::Switch.client_param(), "client=SwitchXML");
        assert_eq!(Resource::Status.client_param(), "client = StatusXML");
        assert_eq!(Action::LogOff.client_param(), "client=LogOFF");
        assert_eq!(Action::Reboot.client_param(), "client=RPMDoReboot");
    }

    #[test]
    fn builder_defaults_to_https_443() {
        let c = Client::builder("10.0.0.5").build().unwrap();
        assert_eq!(c.base_url(), "https://10.0.0.5:443");
    }

    #[test]
    fn builder_honours_custom_port() {
        let c = Client::builder("kvm.local").port(8443).build().unwrap();
        assert_eq!(c.base_url(), "https://kvm.local:8443");
    }
}
