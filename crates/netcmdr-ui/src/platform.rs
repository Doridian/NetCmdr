// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the viewer needs from its host: the session, the management API, and
//! the window.

use netcmdr_mgmt::{PowerCommand, Target};
use netcmdr_proto::command::{VideoFilter, VideoSetting};
use netcmdr_session::{Error, Event, Input, SharedFrame, VideoSettings};

/// The KVM session: `netcmdr_session::Session` natively, a pumped
/// `SessionCore` in the browser.
pub trait SessionLink {
    fn frame(&self) -> &SharedFrame;
    fn next_event(&mut self) -> Option<Event>;
    fn is_ended(&self) -> bool;

    fn send_input(&mut self, ev: Input) -> Result<(), Error>;
    fn send_xml(&mut self, xml: &str) -> Result<(), Error>;
    fn refresh_screen(&mut self) -> Result<(), Error>;
    fn auto_adjust_video(&mut self) -> Result<(), Error>;
    fn stop_auto_adjust(&mut self) -> Result<(), Error>;
    fn set_video_number(&mut self, setting: VideoSetting, value: i32) -> Result<(), Error>;
    /// See `netcmdr_session::Session::store_video_settings`.
    fn store_video_settings(
        &mut self,
        current: &VideoSettings,
        changes: &[(VideoSetting, i32)],
    ) -> Result<(), Error>;
    fn set_video_filter(&mut self, filter: VideoFilter) -> Result<(), Error>;
    fn change_target(&mut self, name: &str, port: u32) -> Result<(), Error>;
    fn calibrate_mouse(&mut self) -> Result<(), Error>;
    fn align_mouse(&mut self) -> Result<(), Error>;
    fn set_exclusive(&mut self, exclusive: bool) -> Result<(), Error>;
}

#[derive(Debug, Clone)]
pub enum MgmtReply {
    Targets(Vec<Target>),
    /// For the status bar.
    Note(String),
    Failed(String),
}

/// The management API (power, target list). Calls are asynchronous; answers
/// are collected later.
pub trait MgmtLink {
    fn request_targets(&mut self);
    fn request_power(&mut self, port: u32, command: PowerCommand);
    fn next_reply(&mut self) -> Option<MgmtReply>;
    /// Whether a request is outstanding.
    fn busy(&self) -> bool;
}

/// The window, or the tab.
pub trait Host {
    fn set_title(&self, title: &str);

    /// Resize to physical pixels. No-op by default (browsers can't).
    fn request_size(&self, _width: u32, _height: u32) {}

    /// Whether [`Host::request_size`] works.
    fn resizable(&self) -> bool {
        false
    }

    /// How to draw the local pointer over the video.
    fn set_cursor(&self, over_video: bool, mode: crate::menu::CursorMode);
}
