// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The viewer: the menu bar, the remote screen, and the input routing between
//! them.
//!
//! Two things share one window, and the interesting part is deciding which gets
//! an event. Everything is handed to egui first; whatever egui does not want
//! goes to the target. That single rule keeps the menu usable without the
//! target seeing stray clicks, and keeps typing going to the target without the
//! menu swallowing it — the device wants **absolute coordinates in remote
//! screen pixels**, so pointer positions are mapped back through the rectangle
//! the image was actually drawn in.
//!
//! Nothing here knows whether it is in a window or a browser tab. What differs
//! between the two is behind [`crate::platform`].

use b070_session::time::{Duration, Instant};
use b070_session::{Event, Input, Modifiers, MouseButton};

use crate::key::PhysicalKey;
use crate::macros::{self, KeyMacro};
use crate::menu::{Action, Menu, Scaling, Work, NOTE_LINGER, WORKING_LINGER};
use crate::platform::{Host, MgmtLink, MgmtReply, SessionLink};
use crate::screen::Screen;

/// How long to keep the screen blank waiting for a target switch to land.
///
/// The device takes its time: measured against the hardware, a port change
/// answered after 4.5 s when the new target was driving video and 9.6 s when it
/// was not. This is only a backstop so a switch that never completes cannot
/// leave the window blank forever.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(25);

/// Modifiers that must be released when the window loses focus, or the target
/// is left believing one is still held.
const STICKY_MODIFIERS: [PhysicalKey; 8] = [
    PhysicalKey::ControlLeft,
    PhysicalKey::ControlRight,
    PhysicalKey::AltLeft,
    PhysicalKey::AltRight,
    PhysicalKey::ShiftLeft,
    PhysicalKey::ShiftRight,
    PhysicalKey::MetaLeft,
    PhysicalKey::MetaRight,
];

/// A target change that has been asked for but not yet confirmed.
///
/// After `change_port` the device goes on sending the **old** target's video
/// for several seconds before it re-syncs — so the picture on screen is a
/// different machine from the one the title bar now names. Rather than show
/// that, the screen is blanked until one of the device's own signals says the
/// switch has landed.
struct Switch {
    port: u32,
    name: String,
    since: Instant,
}

/// Why there is no picture to draw.
enum Waiting {
    /// A fresh session that has not decoded a frame yet.
    FirstFrame,
    /// A target switch is in flight.
    Switching { name: String, port: u32 },
    /// The device says the target is not driving its video output.
    NoSignal,
}

impl Waiting {
    fn message(&self) -> String {
        match self {
            Waiting::FirstFrame => "Waiting for video…".into(),
            Waiting::Switching { name, .. } => format!("Switching to {name}…"),
            Waiting::NoSignal => "No video signal from the target".into(),
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Waiting::FirstFrame => None,
            Waiting::Switching { port, .. } => Some(format!("port {port}")),
            Waiting::NoSignal => {
                Some("The target is off, asleep, or its video cable is out.".into())
            }
        }
    }

    /// Whether to show a spinner: only while something is actually expected.
    fn busy(&self) -> bool {
        !matches!(self, Waiting::NoSignal)
    }
}

/// What the viewer needs besides the session itself.
pub struct Config {
    pub host: String,
    pub user: String,
    pub target: u32,
    pub exclusive: bool,
}

pub struct Viewer<S: SessionLink, M: MgmtLink> {
    session: S,
    mgmt: M,
    menu: Menu,
    screen: Screen,

    /// Remote resolution, once the first frame has arrived.
    remote: Option<(u16, u16)>,
    /// Where the image was drawn last frame, in egui points.
    video_rect: egui::Rect,
    pointer_over_video: bool,

    /// A target change asked for but not yet confirmed by the device.
    switching: Option<Switch>,
    /// The device last told us the target is not driving video.
    no_signal: bool,

    modifiers: Modifiers,
    /// Repaint wanted: either new video or the UI asked for one.
    dirty: bool,
    /// Set once the remote resolution has sized the window, so a later
    /// resolution change does not fight a window the user has resized.
    sized_once: bool,
    /// The menu asked to leave; acted on once the current frame is finished.
    quitting: bool,
    /// What the last frame's menu asked for, acted on after the pass.
    actions: Vec<Action>,
    /// Why the session ended, once it has.
    end_reason: Option<String>,
    /// Cached from the last frame so input routing needs no egui context.
    ui_wants_keyboard: bool,
    pixels_per_point: f32,
}

impl<S: SessionLink, M: MgmtLink> Viewer<S, M> {
    pub fn new(session: S, mgmt: M, cfg: Config) -> Viewer<S, M> {
        let mut menu = Menu::new(cfg.host.clone(), cfg.user.clone(), cfg.target);
        menu.exclusive = cfg.exclusive;

        let mut mgmt = mgmt;
        mgmt.request_targets();

        Viewer {
            session,
            mgmt,
            menu,
            screen: Screen::new(),
            remote: None,
            video_rect: egui::Rect::NOTHING,
            pointer_over_video: false,
            switching: None,
            no_signal: false,
            modifiers: Modifiers::default(),
            dirty: true,
            sized_once: false,
            quitting: false,
            actions: Vec::new(),
            end_reason: None,
            ui_wants_keyboard: false,
            pixels_per_point: 1.0,
        }
    }

    fn send(&mut self, input: Input) {
        if let Err(e) = self.session.send_input(input) {
            tracing::warn!(error = %e, "input dropped");
        }
    }

    /// Say what just happened, in the bar. The line goes away on its own.
    fn note(&mut self, text: impl Into<String>) {
        self.menu.note(text, NOTE_LINGER);
        self.dirty = true;
    }

    /// Say what the device is doing, for work we start and cannot see finish.
    ///
    /// Progress from the device renews the line, so an adjustment that keeps
    /// reporting keeps its note; one that goes quiet loses it a few seconds
    /// later rather than leaving the bar stuck on it.
    fn working(&mut self, text: impl Into<String>) {
        self.menu.note(text, WORKING_LINGER);
        self.dirty = true;
    }

    /// The same, for work whose end we *can* recognise: the line comes down
    /// when [`Menu::work_done`] is told that answer arrived, and the linger is
    /// only the backstop for an answer that never comes.
    fn working_on(&mut self, text: impl Into<String>, work: Work) {
        self.menu.note_work(text, WORKING_LINGER, work);
        self.dirty = true;
    }

    /// Progress in the device's own words, about work already announced.
    ///
    /// Unlike [`Viewer::working`] this keeps whatever the line was waiting on,
    /// so a report mid-adjustment does not cost the adjustment its ending.
    fn progress(&mut self, text: impl Into<String>) {
        self.menu.renew_note(text, WORKING_LINGER);
        self.dirty = true;
    }

    /// Report a command that failed, without taking the session down with it.
    fn failed(&mut self, what: &str, e: impl std::fmt::Display) {
        tracing::warn!(error = %e, "{what} failed");
        self.note(format!("{what} failed: {e}"));
    }

    /// Why the session ended, if it has. The device gives a reason for most of
    /// the ways a session can stop, and it is the only explanation a user gets.
    pub fn end_reason(&self) -> Option<&str> {
        self.end_reason.as_deref()
    }

    /// The session underneath.
    ///
    /// The web client drives its own session state machine and needs to pump
    /// it once a frame; natively a thread does that and this goes unused.
    pub fn session_mut(&mut self) -> &mut S {
        &mut self.session
    }

    /// What the window should be called.
    pub fn title(&self) -> String {
        format!(
            "{} — {}@{} — port {} ({})",
            env!("CARGO_PKG_NAME"),
            self.menu.user,
            self.menu.host,
            self.menu.current_port,
            self.menu.current_name,
        )
    }

    // ---------------------------------------------------------------- actions

    fn act(&mut self, action: Action, host: &dyn Host) {
        match action {
            Action::CtrlAltDel => self.send(Input::CtrlAltDel),
            Action::Macro(index) => match macros::get(index) {
                Some(m) => self.send_macro(m),
                None => tracing::warn!(index, "no such macro"),
            },
            Action::ReleaseHeldKeys => {
                self.release_held_keys();
                self.note("released held keys");
            }

            Action::SyncPointer => self.send(Input::MouseQuickSync),
            Action::CalibrateMouse => match self.session.calibrate_mouse() {
                Ok(()) => self.working("calibrating the mouse…"),
                Err(e) => self.failed("calibrate", e),
            },
            Action::AlignMouse => match self.session.align_mouse() {
                Ok(()) => self.working("aligning the mouse…"),
                Err(e) => self.failed("align", e),
            },
            Action::SetCursorMode(mode) => {
                self.menu.cursor = mode;
                self.dirty = true;
            }

            Action::RefreshVideo => match self.session.refresh_screen() {
                Ok(()) => self.working_on("refreshing…", Work::Repaint),
                Err(e) => self.failed("refresh", e),
            },
            Action::AutoAdjustVideo => match self.session.auto_adjust_video() {
                Ok(()) => self.working_on("adjusting video…", Work::VideoAdjust),
                Err(e) => self.failed("auto adjust", e),
            },
            Action::StopAutoAdjust => match self.session.stop_auto_adjust() {
                Ok(()) => self.note("stopped adjusting"),
                Err(e) => self.failed("stop adjusting", e),
            },
            // The control that sent this is on screen showing the value, and
            // the picture answers for itself, so a note would only read the
            // knob back. Say something only when the device refuses.
            Action::SetVideoNumber(setting, value) => {
                if let Err(e) = self.session.set_video_number(setting, value) {
                    self.failed(setting.as_str(), e);
                }
            }
            Action::SetVideoFilter(filter) => match self.session.set_video_filter(filter) {
                // Picking `auto` runs the adjustment rather than setting
                // anything: that is work with no visible start, so it is worth
                // a line where an ordinary filter change is not.
                Ok(()) if filter == b070_proto::command::VideoFilter::Auto => {
                    self.working_on("adjusting video…", Work::VideoAdjust)
                }
                Ok(()) => {}
                Err(e) => self.failed("filter", e),
            },
            Action::SetScaling(mode) => {
                self.menu.scaling = mode;
                self.dirty = true;
            }
            Action::ResizeToRemote => {
                if let Some((w, h)) = self.remote {
                    host.request_size(w as u32, h as u32 + self.bar_height());
                }
            }

            Action::Power(command) => {
                let port = self.menu.current_port;
                self.mgmt.request_power(port, command);
                self.working("sending power command…");
            }

            Action::SwitchTarget { port, name } => match self.session.change_target(&name, port) {
                Ok(()) => {
                    self.menu.current_port = port;
                    self.menu.current_name = name.clone();
                    host.set_title(&self.title());
                    // The old target's tiles keep arriving for seconds after
                    // this, and the title bar already names the new one. Throw
                    // the picture away and stop uploading until the device
                    // confirms, so what is on screen is never the wrong
                    // machine.
                    self.screen.blank();
                    self.no_signal = false;
                    self.menu.switching = true;
                    self.switching = Some(Switch {
                        port,
                        name: name.clone(),
                        since: Instant::now(),
                    });
                    self.working_on(format!("switching to {name}…"), Work::Switch);
                }
                Err(e) => self.failed("switch target", e),
            },
            Action::RefreshTargets => {
                self.mgmt.request_targets();
                self.dirty = true;
            }
            Action::SetExclusive(exclusive) => match self.session.set_exclusive(exclusive) {
                Ok(()) => {
                    self.menu.exclusive = exclusive;
                    self.note(if exclusive {
                        "asked for exclusive access"
                    } else {
                        "released exclusive access"
                    });
                }
                Err(e) => self.failed("exclusivity", e),
            },
            Action::Disconnect => {
                // Tell the device rather than just dropping the sockets, so it
                // frees the target now instead of on its own timeout. The
                // session tears itself down either way.
                if let Err(e) = self.session.send_xml(&b070_proto::command::disconnect()) {
                    tracing::debug!(error = %e, "disconnect command not sent");
                }
                self.quitting = true;
            }
        }
    }

    /// Press a macro's keys in order, then release them in reverse.
    ///
    /// The modifier word sent with each event describes the state *including*
    /// the key in it, which is what the vendor's client does and what the
    /// target's keyboard controller expects.
    fn send_macro(&mut self, m: &KeyMacro) {
        // Start from a clean slate: whatever the user is physically holding
        // while they click a menu item is not part of the macro.
        self.release_held_keys();

        let mut mods = Modifiers {
            num_lock: self.modifiers.num_lock,
            caps_lock: self.modifiers.caps_lock,
            scroll_lock: self.modifiers.scroll_lock,
            ..Modifiers::default()
        };

        for key in m.keys {
            apply_modifier(&mut mods, *key, true);
            {
                let (scancode, extended) = key.scancode();
                self.send(Input::Key {
                    scancode,
                    extended,
                    down: true,
                    modifiers: mods,
                });
            }
        }
        for key in m.keys.iter().rev() {
            let (scancode, extended) = key.scancode();
            self.send(Input::Key {
                scancode,
                extended,
                down: false,
                modifiers: mods,
            });
            apply_modifier(&mut mods, *key, false);
        }
        self.note(format!("sent {}", m.label));
    }

    /// Send a key-up for every modifier, held or not.
    fn release_held_keys(&mut self) {
        for key in STICKY_MODIFIERS {
            let (scancode, extended) = key.scancode();
            self.send(Input::Key {
                scancode,
                extended,
                down: false,
                modifiers: Modifiers::default(),
            });
        }
        self.modifiers.shift = false;
        self.modifiers.ctrl = false;
        self.modifiers.alt = false;
    }

    /// The switch has landed (or given up): start drawing video again.
    fn finish_switch(&mut self) {
        if self.switching.take().is_some() {
            self.menu.switching = false;
            // "switching to X…" has stopped being true, whether the device
            // confirmed it or the wait ran out. Anything the user has done
            // since owns the bar instead, so only that line is taken down.
            self.menu.work_done(Work::Switch);
            self.screen.blank();
            self.dirty = true;
        }
    }

    /// Why there is no picture, if there is none.
    fn waiting(&self) -> Option<Waiting> {
        if let Some(switch) = &self.switching {
            return Some(Waiting::Switching {
                name: switch.name.clone(),
                port: switch.port,
            });
        }
        if self.no_signal {
            return Some(Waiting::NoSignal);
        }
        if self.screen.texture_id().is_none() {
            return Some(Waiting::FirstFrame);
        }
        None
    }

    // ----------------------------------------------------------------- events

    /// Drain session events, returning false when the session has ended.
    fn pump_session(&mut self, host: &dyn Host) -> bool {
        loop {
            let Some(event) = self.session.next_event() else {
                return !self.session.is_ended();
            };
            match event {
                Event::Frame(rect) => {
                    // Frames arriving mid-switch are still the old target's;
                    // they are decoded into the framebuffer either way, but
                    // nothing is drawn from it until the switch lands.
                    if self.switching.is_none() {
                        self.no_signal = false;
                        self.screen.mark(rect);
                        // A picture arriving is all the answer a refresh gets.
                        self.menu.work_done(Work::Repaint);
                        self.dirty = true;
                    }
                }
                Event::Resolution { width, height } => {
                    tracing::info!(width, height, "remote resolution");
                    self.remote = Some((width, height));
                    self.menu.remote = Some((width, height));
                    self.no_signal = false;
                    // New geometry means the device has re-pointed its capture
                    // at the new target: this is the switch landing.
                    self.finish_switch();
                    self.screen.mark_all();
                    self.dirty = true;

                    // Size the window to the target once, on the first mode we
                    // see. After that the user owns the window size.
                    if !self.sized_once {
                        self.sized_once = true;
                        host.request_size(width as u32, height as u32 + self.bar_height());
                    }
                }
                Event::Noise(noise) => self.menu.noise = Some(noise),
                Event::NoSignal => {
                    // The switch landed on a target that is not driving video,
                    // which is an answer too.
                    self.finish_switch();
                    self.no_signal = true;
                    self.screen.blank();
                    // Nothing is being decoded, so no further noise reports
                    // will arrive to replace the last one.
                    self.menu.noise = None;
                    self.note("no video signal from the target");
                }
                Event::Gui(msg) if !msg.is_empty() => {
                    tracing::info!(%msg, "device message");
                    self.progress(msg);
                }
                Event::Gui(_) => {}
                Event::ConfigXml(xml) => self.on_config(&xml, host),
                Event::Disconnected(reason) => {
                    tracing::info!(%reason, "session ended");
                    self.end_reason = Some(reason);
                    return false;
                }
            }
        }
    }

    /// The device's own view of which target is on screen.
    ///
    /// It re-sends this after a port change, so it is what confirms the switch
    /// actually happened rather than what we asked for.
    fn on_config(&mut self, xml: &str, host: &dyn Host) {
        // The device re-derives these whenever it re-reads the video mode, and
        // an auto-adjust answers with a fresh document — so whatever it says
        // here supersedes anything the adjustment window has been told.
        if let Some(video) = b070_session::parse_video_settings(xml) {
            self.menu.video = Some(video);
            // Which makes this document the automatic adjustment's answer —
            // the protocol has no "done" message, and this is the only thing
            // that arrives when one finishes.
            self.menu.work_done(Work::VideoAdjust);
            self.dirty = true;
        }

        let servers = b070_session::parse_servers(xml);
        if let Some(active) = servers.iter().find(|s| s.active) {
            // Two targets can share a resolution, in which case no `Resolution`
            // event marks the switch — this document is what confirms it.
            if self
                .switching
                .as_ref()
                .is_some_and(|s| s.port == active.port)
            {
                self.finish_switch();
            }
            let changed = active.port != self.menu.current_port;
            self.menu.current_port = active.port;
            if !active.name.is_empty() {
                self.menu.current_name = active.name.clone();
            }
            if changed {
                host.set_title(&self.title());
                self.screen.mark_all();
                self.dirty = true;
            }
        }
    }

    fn pump_mgmt(&mut self, host: &dyn Host) {
        while let Some(reply) = self.mgmt.next_reply() {
            match reply {
                MgmtReply::Targets(targets) => {
                    if let Some(t) = targets.iter().find(|t| t.port == self.menu.current_port) {
                        self.menu.current_name = t.name.clone();
                        host.set_title(&self.title());
                    }
                    self.menu.targets = targets;
                }
                MgmtReply::Note(text) => self.note(text),
                MgmtReply::Failed(text) => {
                    tracing::warn!(%text, "management request failed");
                    self.note(text);
                }
            }
            self.dirty = true;
        }
        self.menu.busy = self.mgmt.busy();
    }

    // ------------------------------------------------------------------- draw

    /// Drain everything that has happened since the last call.
    ///
    /// Returns false once the session has ended and the viewer should close.
    pub fn poll(&mut self, host: &dyn Host) -> bool {
        if self.quitting {
            return false;
        }
        self.pump_mgmt(host);
        if !self.pump_session(host) {
            return false;
        }
        // A switch the device never confirms must not blank the screen forever.
        if self
            .switching
            .as_ref()
            .is_some_and(|s| s.since.elapsed() > SWITCH_TIMEOUT)
        {
            tracing::warn!("target switch was never confirmed; showing video again");
            self.finish_switch();
        }
        // Nothing else drives a note off the bar: a session that goes quiet
        // draws no frames and answers nothing, and the line would otherwise
        // sit there until something unrelated replaced it.
        if self.menu.expire_note() {
            self.dirty = true;
        }
        true
    }

    /// Build one frame of UI. Call this inside `egui::Context::run_ui`.
    pub fn ui(&mut self, ui: &mut egui::Ui, host: &dyn Host) {
        let ctx = ui.ctx().clone();
        self.pixels_per_point = ctx.pixels_per_point();
        let waiting = self.waiting();

        // Uploading inside the egui pass keeps the texture and the frame that
        // draws it in step; the lock is held only for the copy. Mid-switch
        // there is deliberately nothing to upload.
        if waiting.is_none() || matches!(waiting, Some(Waiting::FirstFrame)) {
            if let Ok(frame) = self.session.frame().lock() {
                self.screen.upload(&ctx, &frame);
            }
        }

        self.menu.resizable = host.resizable();
        let actions = self.menu.show(ui);
        self.actions.extend(actions);
        self.video_rect = draw_screen(ui, &self.screen, self.menu.scaling, waiting.as_ref());

        self.pointer_over_video = ctx
            .pointer_latest_pos()
            .is_some_and(|p| self.video_rect.contains(p))
            && !ctx.is_pointer_over_egui();
        self.ui_wants_keyboard = ctx.egui_wants_keyboard_input();
        host.set_cursor(self.pointer_over_video, self.menu.cursor);
    }

    /// Act on what the frame just built asked for.
    ///
    /// Separate from [`Viewer::ui`] because an action may end the session or
    /// resize the window, and neither is safe to do while egui is mid-pass.
    pub fn finish_frame(&mut self, host: &dyn Host) {
        self.dirty = false;
        for action in std::mem::take(&mut self.actions) {
            self.act(action, host);
        }
    }

    /// Height of the menu bar in physical pixels, for sizing to the target.
    fn bar_height(&self) -> u32 {
        (self.video_rect.top() * self.pixels_per_point)
            .round()
            .max(0.0) as u32
    }

    /// Whether a repaint is wanted.
    pub fn needs_repaint(&self) -> bool {
        self.dirty
    }

    /// Note that something outside the viewer wants a repaint.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// A point in the window to a pixel on the target's screen.
    fn to_remote(&self, pos: egui::Pos2) -> Option<(i32, i32)> {
        let (w, h) = self.remote?;
        let rect = self.video_rect;
        if !rect.is_positive() || !rect.contains(pos) {
            return None;
        }
        let x = (pos.x - rect.left()) / rect.width() * w as f32;
        let y = (pos.y - rect.top()) / rect.height() * h as f32;
        Some((
            (x as i32).clamp(0, w as i32 - 1),
            (y as i32).clamp(0, h as i32 - 1),
        ))
    }

    /// Whether keystrokes belong to the target rather than to the UI.
    pub fn keyboard_goes_to_target(&self) -> bool {
        !self.ui_wants_keyboard && !self.menu.captures_keyboard()
    }

    /// Whether the pointer is over the video rather than over the menu.
    pub fn pointer_over_video(&self) -> bool {
        self.pointer_over_video
    }

    /// The local pointer moved, in egui points.
    pub fn on_pointer_moved(&mut self, point: egui::Pos2) {
        if let Some((x, y)) = self.to_remote(point) {
            self.send(Input::MouseMove { x, y });
        }
    }

    /// A mouse button, only forwarded while the pointer is over the video.
    pub fn on_mouse_button(&mut self, button: MouseButton, down: bool) {
        if self.pointer_over_video {
            self.send(Input::MouseButton { button, down });
        }
    }

    /// Wheel notches, only forwarded while the pointer is over the video.
    pub fn on_scroll(&mut self, notches: i32) {
        if notches != 0 && self.pointer_over_video {
            self.send(Input::MouseWheel(notches));
        }
    }

    /// The live modifier state, which the platform tracks for us.
    pub fn set_modifiers(&mut self, shift: bool, ctrl: bool, alt: bool) {
        self.modifiers.shift = shift;
        self.modifiers.ctrl = ctrl;
        self.modifiers.alt = alt;
    }

    /// Focus went elsewhere: let go of everything, or the target is left
    /// believing a modifier is still held.
    pub fn on_focus_lost(&mut self) {
        self.release_held_keys();
        self.modifiers = Modifiers::default();
    }

    /// A physical key went down or up.
    pub fn on_key(&mut self, code: PhysicalKey, down: bool, host: &dyn Host) {
        // Local shortcuts, not forwarded: the target can never see
        // Ctrl+Alt+Delete otherwise, and the pointer needs a resync whenever
        // the target's own acceleration drifts from ours. Each has a menu entry
        // too; these are the fast paths.
        if self.modifiers.ctrl && self.modifiers.alt && down {
            match code {
                PhysicalKey::End => return self.act(Action::CtrlAltDel, host),
                PhysicalKey::Home => return self.act(Action::SyncPointer, host),
                PhysicalKey::KeyA => return self.act(Action::AutoAdjustVideo, host),
                _ => {}
            }
        }

        // Lock keys are tracked locally: the platform does not tell us the LED
        // state, and the device only uses these as a hint.
        if down {
            match code {
                PhysicalKey::CapsLock => self.modifiers.caps_lock = !self.modifiers.caps_lock,
                PhysicalKey::NumLock => self.modifiers.num_lock = !self.modifiers.num_lock,
                PhysicalKey::ScrollLock => self.modifiers.scroll_lock = !self.modifiers.scroll_lock,
                _ => {}
            }
        }

        let (scancode, extended) = code.scancode();
        let modifiers = self.modifiers;
        self.send(Input::Key {
            scancode,
            extended,
            down,
            modifiers,
        });
    }
}

/// The "there is nothing to show yet" panel, used on a fresh session, during a
/// target switch, and when the target is not driving video.
fn draw_placeholder(ui: &mut egui::Ui, waiting: &Waiting) {
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            // `centered_and_justified` hands the child the whole panel, so the
            // column has to be nudged down to sit on the middle of it.
            ui.add_space(ui.available_height() / 2.0 - 32.0);
            if waiting.busy() {
                ui.add(egui::Spinner::new().size(24.0));
                ui.add_space(10.0);
            }
            ui.label(egui::RichText::new(waiting.message()).size(16.0).weak());
            if let Some(detail) = waiting.detail() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(detail).small().weak());
            }
        });
    });
}

fn apply_modifier(mods: &mut Modifiers, key: PhysicalKey, down: bool) {
    match key {
        PhysicalKey::ControlLeft | PhysicalKey::ControlRight => mods.ctrl = down,
        PhysicalKey::AltLeft | PhysicalKey::AltRight => mods.alt = down,
        PhysicalKey::ShiftLeft | PhysicalKey::ShiftRight => mods.shift = down,
        _ => {}
    }
}

/// Draw the remote screen, returning the rectangle it landed in.
///
/// Returns [`egui::Rect::NOTHING`] when a placeholder is showing instead, which
/// is also what stops the pointer being mapped onto a picture that is not there.
fn draw_screen(
    ui: &mut egui::Ui,
    screen: &Screen,
    scaling: Scaling,
    waiting: Option<&Waiting>,
) -> egui::Rect {
    let ppp = ui.ctx().pixels_per_point();
    let mut rect = egui::Rect::NOTHING;
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
        .show(ui, |ui| {
            let (w, h) = screen.size();
            let texture = screen.texture_id().filter(|_| w > 0 && h > 0);
            let Some(id) = texture.filter(|_| waiting.is_none()) else {
                draw_placeholder(ui, waiting.unwrap_or(&Waiting::FirstFrame));
                return;
            };
            let image = egui::vec2(w as f32, h as f32) / ppp;
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

            match scaling {
                Scaling::Fit => {
                    let avail = ui.available_size();
                    let scale = (avail.x / image.x).min(avail.y / image.y);
                    let size = image * scale;
                    let at = ui.available_rect_before_wrap();
                    rect = egui::Rect::from_center_size(at.center(), size);
                    ui.painter().image(id, rect, uv, egui::Color32::WHITE);
                }
                Scaling::Actual => {
                    // One remote pixel per physical pixel. The target's screen
                    // is usually bigger than the window at this scale, so it
                    // pans — but only by dragging the bars. The wheel and
                    // drags belong to the target: scrolling a document on the
                    // far end matters more than panning this view.
                    egui::ScrollArea::both()
                        .scroll_source(egui::scroll_area::ScrollSource {
                            scroll_bar: true,
                            drag: egui::scroll_area::DragScroll::Never,
                            mouse_wheel: false,
                        })
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                        )
                        .show(ui, |ui| {
                            let (r, _) = ui.allocate_exact_size(image, egui::Sense::hover());
                            rect = r;
                            ui.painter().image(id, r, uv, egui::Color32::WHITE);
                        });
                }
            }
        });
    rect
}
