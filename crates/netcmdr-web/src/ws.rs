// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A session channel over a browser WebSocket. The bridge has done the
//! handshake and TLS, so this carries the plain stream.
//!
//! `onmessage` fills a queue that [`WsTransport::read`] drains; empty means
//! `WouldBlock`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::rc::Rc;

use netcmdr_session::channel::Transport;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{CloseEvent, MessageEvent, WebSocket};

#[derive(Default)]
struct Inbox {
    bytes: VecDeque<u8>,
    /// Once closed, an empty queue means EOF.
    closed: bool,
}

pub struct WsTransport {
    socket: WebSocket,
    inbox: Rc<RefCell<Inbox>>,
    /// Dropping a `Closure` detaches its callback.
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl WsTransport {
    /// Attach to a socket that is open and has the session id
    /// ([`crate::connect`]).
    pub fn attach(socket: WebSocket) -> WsTransport {
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
        let inbox = Rc::new(RefCell::new(Inbox::default()));

        let queue = inbox.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() else {
                // The bridge only sends binary.
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
        // The browser buffers; a send takes everything or fails.
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
