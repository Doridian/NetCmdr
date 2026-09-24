// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The browser side of `netcmdr_ui`'s platform traits.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use netcmdr_mgmt::PowerCommand;
use netcmdr_proto::command::{VideoFilter, VideoSetting};
use netcmdr_session::core::{SessionCore, HEARTBEAT_DELAY, HEARTBEAT_PERIOD};
use netcmdr_session::time::Instant;
use netcmdr_session::{Error, Event, Input, SharedFrame, VideoSettings};
use netcmdr_ui::platform::{MgmtLink, MgmtReply, SessionLink};
use wasm_bindgen_futures::spawn_local;

use crate::net;
use crate::ws::WsTransport;

/// The session, pumped from the frame callback.
pub struct WebSession {
    core: SessionCore<WsTransport>,
    next_beat: Instant,
}

impl WebSession {
    pub fn new(core: SessionCore<WsTransport>) -> WebSession {
        WebSession {
            core,
            next_beat: Instant::now() + HEARTBEAT_DELAY,
        }
    }

    /// Call once per frame, before draining events.
    pub fn pump(&mut self) {
        self.core.pump();
        if Instant::now() >= self.next_beat {
            self.next_beat = Instant::now() + HEARTBEAT_PERIOD;
            if let Err(e) = self.core.send_heartbeat() {
                tracing::warn!(error = %e, "heartbeat failed");
            }
        }
    }
}

impl SessionLink for WebSession {
    fn frame(&self) -> &SharedFrame {
        self.core.frame()
    }
    fn next_event(&mut self) -> Option<Event> {
        self.core.next_event()
    }
    fn is_ended(&self) -> bool {
        self.core.is_ended()
    }
    fn send_input(&mut self, ev: Input) -> Result<(), Error> {
        self.core.send_input(ev)
    }
    fn send_xml(&mut self, xml: &str) -> Result<(), Error> {
        self.core.send_xml(xml)
    }
    fn refresh_screen(&mut self) -> Result<(), Error> {
        self.core.refresh_screen()
    }
    fn auto_adjust_video(&mut self) -> Result<(), Error> {
        self.core.auto_adjust_video()
    }
    fn stop_auto_adjust(&mut self) -> Result<(), Error> {
        self.core.stop_auto_adjust()
    }
    fn set_video_number(&mut self, setting: VideoSetting, value: i32) -> Result<(), Error> {
        self.core.set_video_number(setting, value)
    }
    fn store_video_settings(
        &mut self,
        current: &VideoSettings,
        changes: &[(VideoSetting, i32)],
    ) -> Result<(), Error> {
        self.core.store_video_settings(current, changes)
    }
    fn set_video_filter(&mut self, filter: VideoFilter) -> Result<(), Error> {
        self.core.set_video_filter(filter)
    }
    fn change_target(&mut self, name: &str, port: u32) -> Result<(), Error> {
        self.core.change_target(name, port)
    }
    fn calibrate_mouse(&mut self) -> Result<(), Error> {
        self.core.calibrate_mouse()
    }
    fn align_mouse(&mut self) -> Result<(), Error> {
        self.core.align_mouse()
    }
    fn set_exclusive(&mut self, exclusive: bool) -> Result<(), Error> {
        self.core.set_exclusive(exclusive)
    }
}

/// The management API via `fetch` to the bridge; answers are queued.
pub struct WebMgmt {
    session_id: String,
    replies: Rc<RefCell<VecDeque<MgmtReply>>>,
    pending: Rc<Cell<usize>>,
}

impl WebMgmt {
    pub fn new(session_id: String) -> WebMgmt {
        WebMgmt {
            session_id,
            replies: Rc::new(RefCell::new(VecDeque::new())),
            pending: Rc::new(Cell::new(0)),
        }
    }

    fn request(&mut self, body: String, interpret: fn(Result<String, String>) -> MgmtReply) {
        self.pending.set(self.pending.get() + 1);
        let replies = self.replies.clone();
        let pending = self.pending.clone();
        spawn_local(async move {
            let reply = interpret(net::post_message_pv(&body).await);
            replies.borrow_mut().push_back(reply);
            pending.set(pending.get().saturating_sub(1));
        });
    }
}

impl MgmtLink for WebMgmt {
    fn request_targets(&mut self) {
        let body = netcmdr_mgmt::request::config(&self.session_id);
        self.request(body, |result| match result {
            Ok(xml) => match netcmdr_mgmt::parse_config(&xml) {
                Ok(config) => MgmtReply::Targets(config.targets),
                Err(e) => MgmtReply::Failed(format!("reading targets: {e}")),
            },
            Err(e) => MgmtReply::Failed(format!("reading targets: {e}")),
        });
    }

    fn request_power(&mut self, port: u32, command: PowerCommand) {
        let body = netcmdr_mgmt::request::power(&self.session_id, port, command);
        let verb = match command {
            PowerCommand::Cycle => "power cycle",
            PowerCommand::Up => "power on",
            PowerCommand::Down => "power off",
        };
        let note = format!("{verb} sent to port {port}");
        let failed = format!("{verb} on port {port}");
        self.pending.set(self.pending.get() + 1);
        let replies = self.replies.clone();
        let pending = self.pending.clone();
        spawn_local(async move {
            let reply = match net::post_message_pv(&body).await {
                Ok(xml) => match netcmdr_mgmt::model::error_in(&xml) {
                    Some(e) => MgmtReply::Failed(format!("{failed}: {e}")),
                    None => MgmtReply::Note(note),
                },
                Err(e) => MgmtReply::Failed(format!("{failed}: {e}")),
            };
            replies.borrow_mut().push_back(reply);
            pending.set(pending.get().saturating_sub(1));
        });
    }

    fn next_reply(&mut self) -> Option<MgmtReply> {
        self.replies.borrow_mut().pop_front()
    }

    fn busy(&self) -> bool {
        self.pending.get() > 0
    }
}
