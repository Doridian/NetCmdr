// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `KVMMsg` framing over one session channel, whatever is carrying it.
//!
//! The device's four channels all speak the same framing, so this is the layer
//! both clients share: natively it sits on a TCP socket (see [`crate::tcp`]),
//! and in a browser on a WebSocket relayed by `netcmdr-bridge`. Nothing here knows
//! which.
//!
//! The key property that makes one implementation serve both is that reading
//! never blocks by contract. [`Channel::poll`] treats `WouldBlock` as "not yet",
//! keeps whatever part of the message it already has, and resumes on the next
//! call — so a transport that can only ever answer from what has already
//! arrived, as a browser's is, needs no special handling.

use std::io::{self, Read, Write};

use netcmdr_proto::kvmmsg::{Header, Message, SocketType, HEADER_LEN};

use crate::Error;

/// Refuse to buffer a payload larger than this; a plausible frame is well under
/// it, and a corrupt header must not turn into a huge allocation.
pub(crate) const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// A byte stream carrying one session channel.
///
/// Implementors report "nothing available yet" as [`io::ErrorKind::WouldBlock`]
/// rather than blocking, unless they are a socket the caller has deliberately
/// put in blocking mode.
pub trait Transport: Read + Write {
    /// Close the stream, so a reader stops rather than waiting forever.
    fn close(&self);
}

/// Partially received message, so a read that stops mid-frame can be resumed
/// rather than losing its place in the stream.
#[derive(Default)]
struct Incoming {
    header_buf: [u8; HEADER_LEN],
    header_got: usize,
    header: Option<Header>,
    payload: Vec<u8>,
    payload_got: usize,
}

/// One connected session channel.
pub struct Channel<T> {
    stream: T,
    socket_type: SocketType,
    incoming: Incoming,
    /// Bytes written but not yet accepted by the transport.
    ///
    /// A non-blocking transport can take a write only part way, and the
    /// messages here are a protocol stream where a half-written one desyncs
    /// everything after it. Queueing rather than calling `write_all` is what
    /// makes a short write a delay instead of a corruption.
    outgoing: Vec<u8>,
    sent: usize,
}

impl<T: Transport> Channel<T> {
    /// Put `KVMMsg` framing on an already-handshaken transport.
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

    /// The transport underneath, for the platform-specific knobs on it.
    pub fn transport(&self) -> &T {
        &self.stream
    }

    /// Queue one message and push as much of it as the transport will take.
    ///
    /// Header and payload go out together, as the client does — the device's
    /// reader expects them in one write.
    pub fn send(&mut self, msg: &Message) -> Result<(), Error> {
        self.outgoing.extend_from_slice(&msg.to_bytes());
        self.flush_outgoing()
    }

    /// Push whatever is still queued. A transport that will take no more right
    /// now is not an error; the remainder waits for the next call.
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

    /// Whether anything is still waiting to go out.
    pub fn has_pending_writes(&self) -> bool {
        self.sent < self.outgoing.len()
    }

    /// Read whatever is available. Returns `Ok(None)` if the current message is
    /// still incomplete; the partial state is kept, so calling again resumes it.
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

    /// Read one message, retrying until it is complete.
    ///
    /// Only sensible on a transport that actually blocks; on a non-blocking one
    /// this spins. The session's state machine polls instead.
    pub fn recv(&mut self) -> Result<Message, Error> {
        loop {
            if let Some(msg) = self.poll()? {
                return Ok(msg);
            }
        }
    }

    /// Close the underlying transport, unblocking a reader.
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
