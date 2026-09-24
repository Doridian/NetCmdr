// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native `netcmdr_ui` platform traits: threaded session and management worker.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use netcmdr_mgmt::{Client, PowerCommand};
use netcmdr_proto::command::{VideoFilter, VideoSetting};
use netcmdr_session::{Error, Event, Input, Session, SharedFrame, VideoSettings};
use netcmdr_ui::platform::{MgmtLink, MgmtReply, SessionLink};

pub struct NativeSession {
    session: Session,
    ended: bool,
}

impl NativeSession {
    pub fn new(session: Session) -> NativeSession {
        NativeSession {
            session,
            ended: false,
        }
    }
}

impl SessionLink for NativeSession {
    fn frame(&self) -> &SharedFrame {
        self.session.frame()
    }

    fn next_event(&mut self) -> Option<Event> {
        match self.session.events.try_recv() {
            Ok(event) => {
                if matches!(event, Event::Disconnected(_)) {
                    self.ended = true;
                }
                Some(event)
            }
            Err(TryRecvError::Empty) => None,
            // The session thread is gone without a reason.
            Err(TryRecvError::Disconnected) => {
                self.ended = true;
                None
            }
        }
    }

    fn is_ended(&self) -> bool {
        self.ended
    }

    fn send_input(&mut self, ev: Input) -> Result<(), Error> {
        self.session.send_input(ev)
    }
    fn send_xml(&mut self, xml: &str) -> Result<(), Error> {
        self.session.send_xml(xml)
    }
    fn refresh_screen(&mut self) -> Result<(), Error> {
        self.session.refresh_screen()
    }
    fn auto_adjust_video(&mut self) -> Result<(), Error> {
        self.session.auto_adjust_video()
    }
    fn stop_auto_adjust(&mut self) -> Result<(), Error> {
        self.session.stop_auto_adjust()
    }
    fn set_video_number(&mut self, setting: VideoSetting, value: i32) -> Result<(), Error> {
        self.session.set_video_number(setting, value)
    }
    fn store_video_settings(
        &mut self,
        current: &VideoSettings,
        changes: &[(VideoSetting, i32)],
    ) -> Result<(), Error> {
        self.session.store_video_settings(current, changes)
    }
    fn set_video_filter(&mut self, filter: VideoFilter) -> Result<(), Error> {
        self.session.set_video_filter(filter)
    }
    fn change_target(&mut self, name: &str, port: u32) -> Result<(), Error> {
        self.session.change_target(name, port)
    }
    fn calibrate_mouse(&mut self) -> Result<(), Error> {
        self.session.calibrate_mouse()
    }
    fn align_mouse(&mut self) -> Result<(), Error> {
        self.session.align_mouse()
    }
    fn set_exclusive(&mut self, exclusive: bool) -> Result<(), Error> {
        self.session.set_exclusive(exclusive)
    }
}

enum Request {
    Targets,
    Power { port: u32, command: PowerCommand },
}

/// Management API worker thread, so slow requests don't freeze the window.
pub struct NativeMgmt {
    requests: Option<Sender<Request>>,
    replies: Receiver<MgmtReply>,
    /// Unanswered requests.
    pending: usize,
}

impl NativeMgmt {
    pub fn spawn(client: Arc<Client>) -> NativeMgmt {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (tx, replies) = mpsc::channel();

        std::thread::Builder::new()
            .name("netcmdr-mgmt".into())
            .spawn(move || {
                // Sender dropped: stop.
                while let Ok(request) = inbox.recv() {
                    if tx.send(run(&client, request)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning the management thread");

        NativeMgmt {
            requests: Some(requests),
            replies,
            pending: 0,
        }
    }

    fn send(&mut self, request: Request) {
        if let Some(tx) = &self.requests {
            if tx.send(request).is_ok() {
                self.pending += 1;
            }
        }
    }
}

impl MgmtLink for NativeMgmt {
    fn request_targets(&mut self) {
        self.send(Request::Targets);
    }

    fn request_power(&mut self, port: u32, command: PowerCommand) {
        self.send(Request::Power { port, command });
    }

    fn next_reply(&mut self) -> Option<MgmtReply> {
        let reply = self.replies.try_recv().ok()?;
        self.pending = self.pending.saturating_sub(1);
        Some(reply)
    }

    fn busy(&self) -> bool {
        self.pending > 0
    }
}

fn run(client: &Client, request: Request) -> MgmtReply {
    match request {
        Request::Targets => match client.targets() {
            Ok(targets) => MgmtReply::Targets(targets),
            Err(e) => MgmtReply::Failed(format!("reading targets: {e}")),
        },
        Request::Power { port, command } => match client.power(port, command) {
            Ok(()) => MgmtReply::Note(format!("{} sent to port {port}", verb(command))),
            Err(e) => MgmtReply::Failed(format!("{} on port {port}: {e}", verb(command))),
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
