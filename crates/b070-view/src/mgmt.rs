//! A worker thread for the management API.
//!
//! Power control and the target list live on the HTTPS side, not on the KVM
//! session, and every one of those calls opens a fresh TLS connection. Doing
//! that from the menu's click handler would freeze the window for as long as
//! the device takes to answer, so requests go to this thread and answers come
//! back as events the viewer drains alongside the session's own.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use b070_mgmt::{Client, PowerCommand, Target};

/// Something to ask the device over HTTPS.
#[derive(Debug, Clone)]
pub enum Request {
    /// Re-read the configured targets.
    Targets,
    /// Power-control a target's IP-PDU outlet.
    Power { port: u32, command: PowerCommand },
}

/// An answer, or a note to put in the status line.
#[derive(Debug, Clone)]
pub enum Reply {
    Targets(Vec<Target>),
    /// A short line for the status bar.
    Note(String),
    Failed(String),
}

/// Handle on the worker. Dropping it stops the thread.
pub struct Mgmt {
    requests: Option<Sender<Request>>,
    pub replies: Receiver<Reply>,
    /// Requests sent but not yet answered, so the UI can show it is waiting.
    pending: usize,
}

impl Mgmt {
    pub fn spawn(client: Arc<Client>) -> Mgmt {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (tx, replies) = mpsc::channel();

        std::thread::Builder::new()
            .name("b070-mgmt".into())
            .spawn(move || {
                // `recv` fails once the viewer drops its sender: that is the
                // stop signal, and there is nothing to clean up.
                while let Ok(request) = inbox.recv() {
                    let reply = run(&client, request);
                    if tx.send(reply).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning the management thread");

        Mgmt {
            requests: Some(requests),
            replies,
            pending: 0,
        }
    }

    pub fn send(&mut self, request: Request) {
        if let Some(tx) = &self.requests {
            if tx.send(request).is_ok() {
                self.pending += 1;
            }
        }
    }

    /// Call for every reply drained, so [`Mgmt::busy`] stays honest.
    pub fn answered(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }

    pub fn busy(&self) -> bool {
        self.pending > 0
    }
}

fn run(client: &Client, request: Request) -> Reply {
    match request {
        Request::Targets => match client.targets() {
            Ok(targets) => Reply::Targets(targets),
            Err(e) => Reply::Failed(format!("reading targets: {e}")),
        },
        Request::Power { port, command } => match client.power(port, command) {
            Ok(()) => Reply::Note(format!("{} sent to port {port}", verb(command))),
            Err(e) => Reply::Failed(format!("{} on port {port}: {e}", verb(command))),
        },
    }
}

fn verb(command: PowerCommand) -> &'static str {
    match command {
        PowerCommand::Cycle => "power cycle",
        PowerCommand::Up => "power on",
        PowerCommand::Down => "power off",
    }
}
