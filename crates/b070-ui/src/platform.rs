// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the viewer needs from whatever is hosting it.
//!
//! Three things differ between a window and a browser tab, and only three: how
//! the session is driven, how the management API is reached, and what a
//! "window" can be asked to do. Each is a trait here, and everything else in
//! this crate is written against them, which is what lets one viewer serve both
//! clients.

use b070_mgmt::{PowerCommand, Target};
use b070_proto::command::{VideoFilter, VideoSetting};
use b070_session::{Error, Event, Input, SharedFrame, VideoSettings};

/// The KVM session.
///
/// Natively this is the threaded `b070_session::Session`; in a browser it is a
/// `SessionCore` pumped from the frame callback. The viewer does not care
/// which, so long as events can be drained and commands sent.
pub trait SessionLink {
    /// The framebuffer the decoder writes into.
    fn frame(&self) -> &SharedFrame;
    /// The next event, or `None` when there is nothing waiting.
    fn next_event(&mut self) -> Option<Event>;
    /// Whether the session has ended.
    fn is_ended(&self) -> bool;

    fn send_input(&mut self, ev: Input) -> Result<(), Error>;
    fn send_xml(&mut self, xml: &str) -> Result<(), Error>;
    fn refresh_screen(&mut self) -> Result<(), Error>;
    fn auto_adjust_video(&mut self) -> Result<(), Error>;
    fn stop_auto_adjust(&mut self) -> Result<(), Error>;
    fn set_video_number(&mut self, setting: VideoSetting, value: i32) -> Result<(), Error>;
    /// Write knobs into the device's stored config, gains and all, from its own
    /// last report. See `b070_session::Session::store_video_settings`.
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

/// An answer from the management API.
#[derive(Debug, Clone)]
pub enum MgmtReply {
    Targets(Vec<Target>),
    /// A short line for the status bar.
    Note(String),
    Failed(String),
}

/// The management API.
///
/// Power control and the target list live on HTTPS rather than on the session,
/// and every call is slow enough that it cannot be made from a click handler —
/// a worker thread natively, a `fetch` in a browser. Either way the viewer asks
/// and collects the answer later.
pub trait MgmtLink {
    fn request_targets(&mut self);
    fn request_power(&mut self, port: u32, command: PowerCommand);
    fn next_reply(&mut self) -> Option<MgmtReply>;
    /// Whether anything asked for has not been answered, so the UI can say so.
    fn busy(&self) -> bool;
}

/// The window, or the tab.
pub trait Host {
    fn set_title(&self, title: &str);

    /// Ask to be this many physical pixels. A browser tab cannot oblige, and
    /// is not expected to — the default does nothing.
    fn request_size(&self, _width: u32, _height: u32) {}

    /// Whether [`Host::request_size`] is worth offering in the menu.
    fn resizable(&self) -> bool {
        false
    }

    /// How to draw the local pointer over the video.
    fn set_cursor(&self, over_video: bool, mode: crate::menu::CursorMode);
}
