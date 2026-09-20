// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `b070-bridge` — puts a Tripp Lite NetCommander / Minicom Smart IP KVM behind
//! something a browser can actually talk to.
//!
//! # Why this exists
//!
//! The device's TLS is the problem, and it is not a fixable one. It offers no
//! ECDHE suite at all — only DHE and static RSA — no TLS 1.3, a 1024-bit DHE
//! group and a self-signed certificate. `b070-mgmt` reaches it by asking
//! OpenSSL for security level 1; a browser has no such switch and never will.
//! No amount of clicking through a certificate warning helps, because the
//! handshake fails before a certificate is ever considered.
//!
//! So the bridge terminates the device's TLS itself, using the same OpenSSL
//! settings the native client already proved against hardware, and offers the
//! browser plain HTTP and WebSockets. The web client then needs no TLS stack of
//! its own, which is most of what makes a WebAssembly build of it possible.
//!
//! # What it serves
//!
//! | Route | Purpose |
//! |---|---|
//! | `POST /message.pv` | forwarded verbatim to the device's management API |
//! | `GET /kvm?type=…` | a WebSocket carrying one session socket |
//! | `GET /api/info` | what device this bridge is pointed at |
//! | anything else | a file from `--static-dir`, for the web client itself |
//!
//! The files of that last row are served compressed where the browser takes it.
//! The bridge compresses them itself, in the background, as it starts — see
//! [`assets`].
//!
//! # Security
//!
//! **The bridge does not authenticate anyone.** It cannot: the only credential
//! in this protocol is the device's own `SESSIONID`, and minting one is exactly
//! what `/message.pv` is for. Anything that can reach the listening port can
//! therefore reach the device's management API and session ports, including to
//! attempt logins. That is why the default listen address is loopback, and why
//! anything else should be put behind a reverse proxy that does authenticate,
//! and that terminates a TLS the browser will accept.
//!
//! What it does *not* do is act as an open relay: the device address and both
//! ports are fixed at startup and cannot be steered by a request.

pub mod assets;
pub mod http;
pub mod ws;

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use b070_proto::kvmmsg::SocketType;
use tungstenite::protocol::frame::coding::CloseCode;

use crate::assets::Assets;
use crate::http::Request;

/// Everything a connection thread needs, cloned per connection.
#[derive(Clone)]
pub struct Bridge {
    pub host: String,
    pub https_port: u16,
    pub session_port: u16,
    /// The web client to serve, and the compressed copies of it.
    pub assets: Option<Arc<Assets>>,
    pub timeout: Duration,
}

pub fn serve(bridge: &Bridge, mut stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "?".into(), |a| a.to_string());
    if let Err(e) = route(bridge, &mut stream) {
        tracing::debug!(%peer, error = %e, "connection ended");
    }
}

pub fn route(bridge: &Bridge, stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(bridge.timeout))?;
    stream.set_write_timeout(Some(bridge.timeout))?;

    let Some(request) = Request::read(stream)? else {
        return Ok(()); // the peer hung up without asking for anything
    };

    match (request.method.as_str(), request.path()) {
        ("POST", "/message.pv") => forward_message_pv(bridge, &request, stream),
        ("GET", "/kvm") => open_channel(bridge, &request, stream),
        ("GET", "/api/info") => http::respond(
            stream,
            200,
            "OK",
            "application/json",
            format!(
                r#"{{"device":"{}","sessionPort":{}}}"#,
                bridge.host.replace('"', ""),
                bridge.session_port
            )
            .as_bytes(),
        ),
        ("GET", path) => match &bridge.assets {
            Some(assets) => {
                let accepted = request
                    .header("accept-encoding")
                    .unwrap_or_default()
                    .to_string();
                http::serve_static(stream, assets, path, &accepted)
            }
            None => http::respond_status(stream, 404, "Not Found"),
        },
        _ => http::respond_status(stream, 405, "Method Not Allowed"),
    }
}

/// Forward a management request to the device and hand the answer back.
///
/// The body is opaque here — it is the device's own XML, and the bridge has no
/// reason to look inside it. That keeps every `b070-mgmt` call the web client
/// makes working without the bridge knowing any of them exist.
fn forward_message_pv(
    bridge: &Bridge,
    request: &Request,
    stream: &mut TcpStream,
) -> std::io::Result<()> {
    let endpoint = b070_mgmt::Endpoint {
        host: bridge.host.clone(),
        port: bridge.https_port,
        timeout: bridge.timeout,
    };
    let body = String::from_utf8_lossy(&request.body).into_owned();

    match b070_mgmt::transport::post_message_pv(&endpoint, &body) {
        Ok(reply) => http::respond(
            stream,
            200,
            "OK",
            "text/xml; charset=utf-8",
            reply.as_bytes(),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "management request failed");
            // The device is upstream of us, so its failures are 502s.
            http::respond(
                stream,
                502,
                "Bad Gateway",
                "text/plain; charset=utf-8",
                format!("device: {e}").as_bytes(),
            )
        }
    }
}

/// Open one session socket and relay it over a WebSocket.
///
/// The upgrade happens *before* the device is contacted, because the session id
/// arrives on the WebSocket rather than in the URL — see [`ws::read_session_id`]
/// for why. That also means a device refusal comes back as a close frame with a
/// reason, which is the only form a browser will show the user.
fn open_channel(bridge: &Bridge, request: &Request, stream: &mut TcpStream) -> std::io::Result<()> {
    let Some(socket_type) = request.query("type").as_deref().and_then(socket_type) else {
        return http::respond_status(stream, 400, "Bad Request");
    };

    let mut ws = match ws::accept(request, stream) {
        Ok(ws) => ws,
        Err(e) => {
            tracing::debug!(error = %e, "upgrade refused");
            return http::respond_status(stream, 400, "Bad Request");
        }
    };

    let session_id = match ws::read_session_id(&mut ws) {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!(error = %e, "no session id");
            ws::refuse(&mut ws, CloseCode::Policy, "expected the session id first");
            return Ok(());
        }
    };

    let device = match b070_session::connect_stream(
        &bridge.host,
        bridge.session_port,
        &session_id,
        socket_type,
        bridge.timeout,
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(?socket_type, error = %e, "device refused the channel");
            ws::refuse(&mut ws, CloseCode::Error, &format!("device: {e}"));
            return Ok(());
        }
    };

    tracing::info!(?socket_type, "channel relaying");
    let outcome = ws::relay(ws, device);
    match &outcome {
        Ok(()) => tracing::info!(?socket_type, "channel closed"),
        Err(e) => tracing::warn!(?socket_type, error = %e, "channel failed"),
    }
    outcome
}

/// The `type` query parameter, named as the protocol write-up names the
/// channels rather than by the wire codes.
fn socket_type(name: &str) -> Option<SocketType> {
    match name {
        "cmd" => Some(SocketType::Cmd),
        "video" => Some(SocketType::Video),
        "km" => Some(SocketType::KeyboardMouse),
        "vm" => Some(SocketType::VirtualMedia),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names_map_to_the_protocols_socket_types() {
        assert_eq!(socket_type("cmd"), Some(SocketType::Cmd));
        assert_eq!(socket_type("video"), Some(SocketType::Video));
        assert_eq!(socket_type("km"), Some(SocketType::KeyboardMouse));
        assert_eq!(socket_type("vm"), Some(SocketType::VirtualMedia));
    }

    /// The relay must not be steerable by name; an unknown one is a refusal,
    /// never a fallback.
    #[test]
    fn an_unknown_channel_name_is_refused() {
        assert_eq!(socket_type("general"), None);
        assert_eq!(socket_type(""), None);
        assert_eq!(socket_type("0"), None);
    }
}
