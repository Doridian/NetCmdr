// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Just enough HTTP/1.1 to serve a single-page app and upgrade a WebSocket.
//!
//! This is hand-rolled for the same reason the device client's own transport is
//! (see `b070_mgmt::transport`): the surface actually needed is a request line,
//! a handful of headers, a length-delimited body and a static file. A framework
//! would bring an async runtime with it, and the relay this serves is
//! deliberately synchronous — one thread per connection, blocking where it can
//! and polling where it must.
//!
//! What is *not* implemented is as important as what is: no chunked transfer
//! encoding, no keep-alive, no compression. Every response closes the
//! connection. The bridge answers three kinds of request — a file, a
//! `/message.pv` forward and a WebSocket upgrade — and none of them are helped
//! by pipelining.

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

/// Refuse a request head larger than this. Real ones are a few hundred bytes.
const MAX_HEAD: usize = 16 * 1024;

/// Refuse a body larger than this.
///
/// Ordinary `/message.pv` calls are tiny, but a firmware upload is the whole
/// image base64-encoded inside the XML, so the ceiling has to clear that with
/// room to spare rather than sit at a "sensible" few hundred kilobytes.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// One parsed request.
pub struct Request {
    pub method: String,
    /// The raw request target, query string included.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Bytes already read that belong to whatever comes *after* this request.
    ///
    /// The head is read in chunks, so a client that sends its first WebSocket
    /// frame in the same packet as the upgrade request has that frame sitting
    /// in our buffer. Handing it to `tungstenite` rather than dropping it is
    /// the difference between a working relay and one that loses the first
    /// message roughly whenever the network is fast.
    pub rest: Vec<u8>,
}

impl Request {
    /// Read one request. `Ok(None)` means the peer closed before sending one.
    pub fn read(stream: &mut impl Read) -> io::Result<Option<Request>> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        let split = loop {
            if let Some(at) = find(&buf, b"\r\n\r\n") {
                break at;
            }
            if buf.len() > MAX_HEAD {
                return Err(bad("request head too large"));
            }
            match stream.read(&mut chunk)? {
                0 if buf.is_empty() => return Ok(None),
                0 => return Err(bad("connection closed mid-request")),
                n => buf.extend_from_slice(&chunk[..n]),
            }
        };

        let head = String::from_utf8_lossy(&buf[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let mut start = lines
            .next()
            .ok_or_else(|| bad("empty request"))?
            .split_whitespace();
        let method = start.next().ok_or_else(|| bad("no method"))?.to_string();
        let target = start.next().ok_or_else(|| bad("no target"))?.to_string();

        let mut headers = Vec::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }

        let mut rest = buf[split + 4..].to_vec();
        let len: usize = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim().parse().map_err(|_| bad("bad content-length")))
            .transpose()?
            .unwrap_or(0);
        if len > MAX_BODY {
            return Err(bad("request body too large"));
        }

        // Whatever arrived with the head counts towards the body first.
        let mut body = rest.drain(..rest.len().min(len)).collect::<Vec<u8>>();
        if body.len() < len {
            let mut remaining = vec![0u8; len - body.len()];
            stream.read_exact(&mut remaining)?;
            body.extend_from_slice(&remaining);
        }

        Ok(Some(Request {
            method,
            target,
            headers,
            body,
            rest,
        }))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether a comma-separated header lists `needle`, ignoring case.
    ///
    /// `Connection: keep-alive, Upgrade` is as valid as `Connection: Upgrade`,
    /// and browsers do send the former.
    pub fn header_lists(&self, name: &str, needle: &str) -> bool {
        self.header(name).is_some_and(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(needle))
        })
    }

    /// The request target with any query string removed.
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(p, _)| p)
    }

    /// One percent-decoded query parameter.
    pub fn query(&self, key: &str) -> Option<String> {
        let query = self.target.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (percent_decode(k) == key).then(|| percent_decode(v))
        })
    }
}

/// Decode `%XX` escapes and `+`, leaving anything malformed as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn bad(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Send a response and let the caller drop the connection.
pub fn respond(
    out: &mut impl Write,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len(),
    )?;
    out.write_all(body)?;
    out.flush()
}

/// Send a bare status, using the reason phrase as the body.
pub fn respond_status(out: &mut impl Write, status: u16, reason: &str) -> io::Result<()> {
    respond(
        out,
        status,
        reason,
        "text/plain; charset=utf-8",
        reason.as_bytes(),
    )
}

/// Serve a file from `root`, or 404.
///
/// `/` means `index.html`. Anything that is not a plain relative path — a
/// parent reference, a root anchor, a Windows prefix — is refused outright
/// rather than normalised, because normalising is where directory traversal
/// bugs come from.
pub fn serve_static(out: &mut impl Write, root: &Path, path: &str) -> io::Result<()> {
    let relative = path.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };

    let mut full = PathBuf::from(root);
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => full.push(part),
            _ => return respond_status(out, 403, "Forbidden"),
        }
    }

    match std::fs::read(&full) {
        Ok(body) => respond(out, 200, "OK", content_type(&full), &body),
        Err(e) if e.kind() == io::ErrorKind::NotFound => respond_status(out, 404, "Not Found"),
        Err(e) => {
            tracing::warn!(path = %full.display(), error = %e, "static read failed");
            respond_status(out, 500, "Internal Server Error")
        }
    }
}

/// Content type by extension.
///
/// `application/wasm` is the one that actually matters: browsers refuse to
/// stream-compile a module served as anything else, and the failure reads as a
/// generic instantiation error rather than a content-type complaint.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8]) -> Request {
        Request::read(&mut io::Cursor::new(raw.to_vec()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn reads_a_request_line_and_headers() {
        let r = parse(b"GET /kvm?type=video HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n");
        assert_eq!(r.method, "GET");
        assert_eq!(r.path(), "/kvm");
        assert_eq!(r.header("upgrade"), Some("websocket"));
        assert!(r.body.is_empty());
    }

    #[test]
    fn reads_a_body_of_the_declared_length() {
        let r = parse(b"POST /message.pv HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello");
        assert_eq!(r.body, b"hello");
        assert!(r.rest.is_empty());
    }

    /// Anything past the declared body belongs to the next thing on the wire.
    #[test]
    fn keeps_bytes_that_arrived_after_the_request() {
        let r = parse(b"GET /kvm HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi\x81\x00");
        assert_eq!(r.body, b"hi");
        assert_eq!(r.rest, b"\x81\x00");
    }

    #[test]
    fn a_connection_header_may_list_several_tokens() {
        let r = parse(b"GET / HTTP/1.1\r\nConnection: keep-alive, Upgrade\r\n\r\n");
        assert!(r.header_lists("connection", "upgrade"));
        assert!(r.header_lists("connection", "KEEP-ALIVE"));
        assert!(!r.header_lists("connection", "close"));
    }

    #[test]
    fn decodes_query_parameters() {
        let r = parse(b"GET /kvm?type=video&session=a%2Fb%20c HTTP/1.1\r\n\r\n");
        assert_eq!(r.query("type").as_deref(), Some("video"));
        assert_eq!(r.query("session").as_deref(), Some("a/b c"));
        assert_eq!(r.query("missing"), None);
    }

    #[test]
    fn a_closed_connection_is_not_an_error() {
        assert!(Request::read(&mut io::Cursor::new(Vec::new()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn traversal_is_refused_rather_than_normalised() {
        let mut out = Vec::new();
        serve_static(&mut out, Path::new("/nonexistent"), "/../etc/passwd").unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 403 "));
    }

    #[test]
    fn wasm_gets_the_type_browsers_insist_on() {
        assert_eq!(content_type(Path::new("app_bg.wasm")), "application/wasm");
    }
}
