// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A session channel over a browser WebSocket.
//!
//! `netcmdr-bridge` has already done the device's handshake and, where the socket
//! type calls for it, its TLS — so what arrives here is the plain protocol
//! stream, and this is only the adapter that lets [`netcmdr_session::Channel`]
//! read it.
//!
//! # Turning an event into a stream
//!
//! A browser WebSocket pushes: it calls `onmessage` when it feels like it. The
//! framing above wants to pull, with `read`. The join between them is a queue —
//! `onmessage` appends, [`WsTransport::read`] drains, and an empty queue is
//! reported as `WouldBlock`, which is exactly the "not yet" the framing already
//! understands. Nothing blocks, which is just as well, because on the browser's
//! main thread nothing may.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::rc::Rc;

use netcmdr_session::channel::Transport;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{CloseEvent, MessageEvent, WebSocket};

/// What the socket's callbacks write into, and `read` takes out of.
#[derive(Default)]
struct Inbox {
    bytes: VecDeque<u8>,
    /// Set once the socket has closed; an empty queue then means end of stream
    /// rather than "not yet".
    closed: bool,
}

/// One session channel, carried by a WebSocket to the bridge.
pub struct WsTransport {
    socket: WebSocket,
    inbox: Rc<RefCell<Inbox>>,
    /// Kept alive for as long as the socket is: dropping a `Closure` detaches
    /// the JavaScript callback it stands behind.
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl WsTransport {
    /// Attach to an open socket.
    ///
    /// The socket must already be open and must already have been given the
    /// session id; see [`crate::connect`], which does both.
    pub fn attach(socket: WebSocket) -> WsTransport {
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let inbox = Rc::new(RefCell::new(Inbox::default()));

        let queue = inbox.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() else {
                // The bridge only ever sends binary; anything else is not ours.
                return;
            };
            let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
            queue.borrow_mut().bytes.extend(bytes);
        });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let queue = inbox.clone();
        let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            tracing::info!(code = event.code(), reason = %event.reason(), "channel closed");
            queue.borrow_mut().closed = true;
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        WsTransport {
            socket,
            inbox,
            _on_message: on_message,
            _on_close: on_close,
        }
    }
}

impl Read for WsTransport {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut inbox = self.inbox.borrow_mut();
        if inbox.bytes.is_empty() {
            // A closed socket with nothing left really is the end; an open one
            // simply has not been given anything yet.
            return if inbox.closed {
                Ok(0)
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            };
        }
        let n = out.len().min(inbox.bytes.len());
        for slot in out.iter_mut().take(n) {
            *slot = inbox.bytes.pop_front().expect("queue is not empty");
        }
        Ok(n)
    }
}

impl Write for WsTransport {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // The browser buffers for us, so a send either takes everything or the
        // socket is gone.
        match self.socket.send_with_u8_array(data) {
            Ok(()) => Ok(data.len()),
            Err(e) => Err(io::Error::other(describe(&e))),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Transport for WsTransport {
    fn close(&self) {
        let _ = self.socket.close();
    }
}

pub fn describe(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(value, &"message".into())
                .ok()?
                .as_string()
        })
        .unwrap_or_else(|| format!("{value:?}"))
}
