// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Session setup in the browser: the steps in `netcmdr_session::core`, awaited
//! instead of threaded. Protocol logic is in [`Login`].

use netcmdr_proto::kvmmsg::SocketType;
use netcmdr_session::channel::Channel;
use netcmdr_session::core::{Login, SessionCore, LOGIN_TIMEOUT};
use netcmdr_session::time::Instant;
use netcmdr_session::video::Framebuffer;

use crate::net;
use crate::ws::WsTransport;

pub struct Ready {
    pub core: SessionCore<WsTransport>,
    pub session_id: String,
}

pub async fn connect(
    username: &str,
    password: &str,
    target_port: u32,
    exclusive: bool,
) -> Result<Ready, String> {
    // 1. Log in over HTTP for a session id.
    let xml = net::post_message_pv(&netcmdr_mgmt::request::authorize(username, password)).await?;
    let reply =
        netcmdr_mgmt::parse_auth_reply(&xml).map_err(|e| format!("reading the login: {e}"))?;
    if !reply.is_ok() {
        return Err(reply
            .error
            .as_ref()
            .map(|(detail, id)| format!("{detail} (id {id})"))
            .unwrap_or_else(|| "the device refused the login".into()));
    }
    let session_id = reply
        .session_id
        .clone()
        .ok_or("the device returned no session id")?;

    // 2. Command channel and login.
    let cmd = Channel::new(
        WsTransport::attach(net::open_channel("cmd", &session_id).await?),
        SocketType::Cmd,
    );

    let cfg = netcmdr_session::SessionConfig {
        host: String::new(), // the bridge knows where the device is; we do not
        port: 0,
        session_id: session_id.clone(),
        username: username.to_string(),
        target_port,
        exclusive,
        connect_timeout: LOGIN_TIMEOUT,
    };
    let mut login = Login::begin(cmd, &cfg).map_err(|e| format!("sending the login: {e}"))?;

    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let config_xml = loop {
        match login.poll() {
            Ok(Some(xml)) => break xml,
            Ok(None) => {}
            Err(e) => return Err(format!("{e}")),
        }
        if Instant::now() >= deadline {
            return Err("the device did not answer the login".into());
        }
        // Yield so socket callbacks run.
        net::sleep(16).await;
    };
    let cmd = login.finish();

    // 3. Video, then keyboard/mouse.
    let video = Channel::new(
        WsTransport::attach(net::open_channel("video", &session_id).await?),
        SocketType::Video,
    );
    let km = Channel::new(
        WsTransport::attach(net::open_channel("km", &session_id).await?),
        SocketType::KeyboardMouse,
    );

    let frame = std::sync::Arc::new(std::sync::Mutex::new(Framebuffer::new()));
    let palette = std::sync::Arc::new(std::sync::Mutex::new(None));
    Ok(Ready {
        core: SessionCore::new(cmd, video, km, frame, palette, config_xml),
        session_id,
    })
}
