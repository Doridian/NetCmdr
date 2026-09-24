// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `KVMMsg` framing over any transport: TCP natively ([`crate::tcp`]), a
//! WebSocket via `netcmdr-bridge` in the browser.
//!
//! [`Channel::poll`] never blocks: `WouldBlock` means "not yet", and partial
//! messages resume on the next call.

use std::io::{self, Read, Write};

use netcmdr_proto::kvmmsg::{Header, Message, SocketType, HEADER_LEN};

use crate::Error;

/// Payload size limit, so a corrupt header can't cause a huge allocation.
pub(crate) const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// A byte stream for one channel. Should return [`io::ErrorKind::WouldBlock`]
/// rather than block, unless deliberately in blocking mode.
pub trait Transport: Read + Write {
    fn close(&self);
}

/// A partially received message.
#[derive(Default)]
struct Incoming {
    header_buf: [u8; HEADER_LEN],
    header_got: usize,
    header: Option<Header>,
    payload: Vec<u8>,
    payload_got: usize,
}

pub struct Channel<T> {
    stream: T,
    socket_type: SocketType,
    incoming: Incoming,
    /// Bytes not yet accepted by the transport. Queued so a short write delays
    /// rather than corrupts the stream.
    outgoing: Vec<u8>,
    sent: usize,
}

impl<T: Transport> Channel<T> {
    /// Wrap an already-handshaken transport.
    pub fn new(stream: T, socket_type: SocketType) -> Self {
        Channel {
            stream,
            socket_type,
            incoming: Incoming::default(),
            outgoing: Vec::new(),
            sent: 0,
        }
    }

    pub fn socket_type(&self) -> SocketType {
        self.socket_type
    }

    pub fn transport(&self) -> &T {
        &self.stream
    }

    /// Queue a message and push what the transport will take. Header and
    /// payload go in one write, as the device expects.
    pub fn send(&mut self, msg: &Message) -> Result<(), Error> {
        self.outgoing.extend_from_slice(&msg.to_bytes());
        self.flush_outgoing()
    }

    /// Push queued bytes; any remainder waits for the next call.
    pub fn flush_outgoing(&mut self) -> Result<(), Error> {
        while self.sent < self.outgoing.len() {
            match self.stream.write(&self.outgoing[self.sent..]) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.sent += n,
                Err(e) if would_block(&e) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        self.outgoing.clear();
        self.sent = 0;
        match self.stream.flush() {
            Ok(()) => Ok(()),
            Err(e) if would_block(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn has_pending_writes(&self) -> bool {
        self.sent < self.outgoing.len()
    }

    /// Read what's available. `Ok(None)` if the message is incomplete; call
    /// again to resume.
    pub fn poll(&mut self) -> Result<Option<Message>, Error> {
        if self.incoming.header.is_none() {
            let got = self.incoming.header_got;
            match self.stream.read(&mut self.incoming.header_buf[got..]) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.incoming.header_got += n,
                Err(e) if would_block(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            if self.incoming.header_got < HEADER_LEN {
                return Ok(None);
            }
            let header = Header::decode(&self.incoming.header_buf)?;
            let size = header.size as usize;
            if size > MAX_PAYLOAD {
                return Err(Error::OversizedMessage(size));
            }
            self.incoming.payload = vec![0; size];
            self.incoming.payload_got = 0;
            self.incoming.header = Some(header);
        }

        while self.incoming.payload_got < self.incoming.payload.len() {
            let got = self.incoming.payload_got;
            match self.stream.read(&mut self.incoming.payload[got..]) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.incoming.payload_got += n,
                Err(e) if would_block(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }

        let header = self.incoming.header.take().expect("header present");
        let payload = std::mem::take(&mut self.incoming.payload);
        self.incoming.header_got = 0;
        self.incoming.payload_got = 0;
        Ok(Some(Message { header, payload }))
    }

    /// Read one full message. Spins on a non-blocking transport.
    pub fn recv(&mut self) -> Result<Message, Error> {
        loop {
            if let Some(msg) = self.poll()? {
                return Ok(msg);
            }
        }
    }

    pub fn close(&self) {
        self.stream.close();
    }
}

pub(crate) fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}
