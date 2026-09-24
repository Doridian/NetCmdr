// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! HTTPS transport for the device's `/message.pv` endpoint.
//!
//! The device (Appweb 2.4.x on Windows CE) speaks TLS 1.2 with a self-signed
//! RSA-2048 cert, but:
//!
//! - **No ECDHE and no TLS 1.3**, so rustls can't connect. Hence OpenSSL.
//! - A 1024-bit DHE group, rejected at OpenSSL's default level 2. Level 1
//!   keeps forward secrecy (unlike falling back to static RSA).
//!
//! HTTP is hand-rolled: one POST with `Connection: close`, read to EOF.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};

use crate::Error;

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

/// POST `body` to `/message.pv` over a fresh connection (session state lives
/// in the XML) and return the response body.
pub fn post_message_pv(ep: &Endpoint, body: &str) -> Result<String, Error> {
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    // For the 1024-bit DHE group.
    builder.set_cipher_list("DEFAULT@SECLEVEL=1")?;
    // Self-signed; treat the network as trusted or tunnel it.
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    let tcp = TcpStream::connect((ep.host.as_str(), ep.port))?;
    tcp.set_read_timeout(Some(ep.timeout))?;
    tcp.set_write_timeout(Some(ep.timeout))?;
    tcp.set_nodelay(true).ok();

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
    // Read to EOF. The device's unclean TLS shutdown counts as EOF.
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
