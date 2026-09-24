// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Talking to `netcmdr-bridge`. URLs are relative (same origin), so no CORS,
//! and `wss:` pages get `wss:` sockets.

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, Response, WebSocket};

use crate::ws::describe;

fn window() -> web_sys::Window {
    web_sys::window().expect("a browser window")
}

/// POST a `CMA_Message` and return the reply.
pub async fn post_message_pv(body: &str) -> Result<String, String> {
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(body));

    let request = Request::new_with_str_and_init("message.pv", &init)
        .map_err(|e| format!("building the request: {}", describe(&e)))?;

    let response: Response = JsFuture::from(window().fetch_with_request(&request))
        .await
        .map_err(|e| format!("reaching the bridge: {}", describe(&e)))?
        .dyn_into()
        .map_err(|_| "the bridge answered with something that is not a response".to_string())?;

    let text = JsFuture::from(
        response
            .text()
            .map_err(|e| format!("reading the reply: {}", describe(&e)))?,
    )
    .await
    .map_err(|e| format!("reading the reply: {}", describe(&e)))?
    .as_string()
    .unwrap_or_default();

    if !response.ok() {
        return Err(format!("{} — {}", response.status(), text.trim()));
    }
    Ok(text)
}

/// Open a channel and send the session id as the first message (not in the
/// URL, where it would need escaping and could be logged).
pub async fn open_channel(kind: &str, session_id: &str) -> Result<WebSocket, String> {
    let location = window().location();
    let scheme = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = location
        .host()
        .map_err(|e| format!("reading the page's address: {}", describe(&e)))?;
    let url = format!("{scheme}://{host}/kvm?type={kind}");

    let socket = WebSocket::new(&url).map_err(|e| format!("opening {kind}: {}", describe(&e)))?;
    socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
    await_open(&socket, kind).await?;

    socket
        .send_with_str(session_id)
        .map_err(|e| format!("{kind}: sending the session id: {}", describe(&e)))?;
    Ok(socket)
}

/// Resolve once the socket is open, or fail if it closes first.
async fn await_open(socket: &WebSocket, kind: &str) -> Result<(), String> {
    let socket = socket.clone();
    let kind = kind.to_string();
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let on_open = wasm_bindgen::closure::Closure::once_into_js(move || {
            let _ = resolve.call0(&JsValue::NULL);
        });
        // A refused upgrade closes with the reason.
        let why = kind.clone();
        let on_close =
            wasm_bindgen::closure::Closure::once_into_js(move |event: web_sys::CloseEvent| {
                let reason = event.reason();
                let text = if reason.is_empty() {
                    format!("{why}: the bridge closed the connection")
                } else {
                    format!("{why}: {reason}")
                };
                let _ = reject.call1(&JsValue::NULL, &JsValue::from_str(&text));
            });
        socket.set_onopen(Some(on_open.unchecked_ref()));
        socket.set_onclose(Some(on_close.unchecked_ref()));
    });

    JsFuture::from(promise)
        .await
        .map_err(|e| describe(&e))
        .map(|_| ())
}

/// Yield for `ms` milliseconds so socket callbacks can run.
pub async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = JsFuture::from(promise).await;
}

/// GET a small text document from the bridge.
pub async fn get_text(path: &str) -> Result<String, String> {
    let response: Response = JsFuture::from(window().fetch_with_str(path))
        .await
        .map_err(|e| format!("reaching the bridge: {}", describe(&e)))?
        .dyn_into()
        .map_err(|_| "the bridge answered with something that is not a response".to_string())?;
    if !response.ok() {
        return Err(format!("{}", response.status()));
    }
    JsFuture::from(
        response
            .text()
            .map_err(|e| format!("reading the reply: {}", describe(&e)))?,
    )
    .await
    .map_err(|e| format!("reading the reply: {}", describe(&e)))?
    .as_string()
    .ok_or_else(|| "the reply was not text".to_string())
}
