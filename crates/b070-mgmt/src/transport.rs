//! TLS 1.0 HTTP transport for the device's `/message.pv` endpoint.
//!
//! The device (an Embedthis Appweb 2.4.x server on Windows CE) speaks **only
//! TLS 1.0** with `DHE-RSA-AES256-SHA` and presents a self-signed, SHA-1-signed
//! certificate regenerated on every boot. Modern TLS stacks refuse all of that:
//! rustls has no TLS 1.0 at all, and a default OpenSSL build rejects the
//! handshake at security level 2. So this module drives OpenSSL directly with
//! the security level dropped to 0 and the protocol pinned to TLS 1.0 — the
//! exact settings `curl --tlsv1.0 --ciphers DEFAULT@SECLEVEL=0` needs against
//! this device.
//!
//! Verified against a live B070-008-19-IP (firmware 2.1.1184) on 2026-09-18.
//!
//! HTTP is hand-rolled: the requests are a single POST with a small XML body,
//! and the server honours `Connection: close`, so we read to EOF and split on
//! the header/body boundary rather than pulling in an HTTP client that would
//! fight us over the legacy TLS anyway.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};

use crate::Error;

/// A connection target.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub timeout: Duration,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Endpoint {
            host: host.into(),
            port,
            timeout: Duration::from_secs(20),
        }
    }
}

/// POST `body` to `/message.pv` and return the response body as a string.
///
/// A fresh TLS connection is made per call. The device's session state lives in
/// the `SESSIONID` carried inside the XML, not in the transport, so there is
/// nothing to keep open between requests.
pub fn post_message_pv(ep: &Endpoint, body: &str) -> Result<String, Error> {
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    // The device only offers TLS 1.0.
    builder.set_min_proto_version(Some(SslVersion::TLS1))?;
    builder.set_max_proto_version(Some(SslVersion::TLS1))?;
    // SECLEVEL=0 re-enables the SHA-1 cert signature and the DHE-RSA-AES256-SHA
    // cipher that a level-2 default would reject.
    builder.set_cipher_list("DEFAULT@SECLEVEL=0")?;
    // The certificate is self-signed and its CN is the device serial, so there
    // is nothing to verify against and no name to match. The management channel
    // is unauthenticated by design; treat the network as trusted or tunnel it.
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    let tcp = TcpStream::connect((ep.host.as_str(), ep.port))?;
    tcp.set_read_timeout(Some(ep.timeout))?;
    tcp.set_write_timeout(Some(ep.timeout))?;
    tcp.set_nodelay(true).ok();

    // Do not verify the hostname (see set_verify above).
    let mut cfg = connector.configure()?;
    cfg.set_verify_hostname(false);
    cfg.set_use_server_name_indication(false);
    let mut tls = cfg
        .connect(&ep.host, tcp)
        .map_err(|e| Error::Tls(e.to_string()))?;

    let request = format!(
        "POST /message.pv HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-cache,max-age=0\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        host = ep.host,
        len = body.len(),
        body = body,
    );
    tls.write_all(request.as_bytes())?;
    tls.flush()?;

    let mut raw = Vec::new();
    // Read to EOF. The server closes the connection because we asked it to.
    // A TLS close_notify from a CE device can surface as an unclean shutdown;
    // treat that as end-of-response rather than an error, since we already have
    // the bytes.
    match tls.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if !raw.is_empty() => {
            tracing::debug!(error = %e, "read_to_end ended uncleanly; using bytes received");
        }
        Err(e) => return Err(e.into()),
    }

    parse_http_response(&raw)
}

/// Split an HTTP/1.x response into status and body, returning the body.
fn parse_http_response(raw: &[u8]) -> Result<String, Error> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(Error::MalformedHttp("no header/body separator"))?;
    let head = &raw[..split];
    let body = &raw[split + 4..];

    let status_line = head
        .split(|&b| b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .unwrap_or_default();

    // "HTTP/1.1 200 OK"
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or(Error::MalformedHttp("no status code"))?;

    if code != 200 {
        return Err(Error::HttpStatus(code));
    }

    Ok(String::from_utf8_lossy(body).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_body_from_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-length: 5\r\n\r\nhello";
        assert_eq!(parse_http_response(raw).unwrap(), "hello");
    }

    #[test]
    fn surfaces_non_200() {
        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-length: 0\r\n\r\n";
        assert!(matches!(
            parse_http_response(raw),
            Err(Error::HttpStatus(400))
        ));
    }

    #[test]
    fn rejects_response_without_separator() {
        assert!(matches!(
            parse_http_response(b"HTTP/1.1 200 OK"),
            Err(Error::MalformedHttp(_))
        ));
    }
}
