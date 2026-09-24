// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The viewer: menu bar, remote screen and input routing.
//!
//! Events go to egui first; the rest go to the target. Pointer positions are
//! mapped to **absolute remote pixels** through the drawn image rect.

use netcmdr_session::align::{self, Aligner, Step as AlignStep};
use netcmdr_session::time::{Duration, Instant};
use netcmdr_session::{Event, Input, Modifiers, MouseButton};

use crate::key::PhysicalKey;
use crate::macros::{self, KeyMacro};
use crate::menu::{Action, Menu, Scaling, Work, NOTE_LINGER, WORKING_LINGER};
use crate::platform::{Host, MgmtLink, MgmtReply, SessionLink};
use crate::screen::Screen;

/// Backstop for a target switch that never confirms. Measured switches took
/// 4.5 s (with video) and 9.6 s (without).
const SWITCH_TIMEOUT: Duration = Duration::from_secs(25);

/// Released on focus loss so none stays stuck.
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

/// An unconfirmed target change. The device keeps sending the old target's
/// video for seconds, so the screen stays blank until the switch lands.
struct Switch {
    port: u32,
    name: String,
    since: Instant,
}

enum Waiting {
    /// No frame decoded yet.
    FirstFrame,
    Switching {
        name: String,
        port: u32,
    },
    /// No video signal.
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

    /// Whether something is expected.
    fn busy(&self) -> bool {
        !matches!(self, Waiting::NoSignal)
    }
}

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

    remote: Option<(u16, u16)>,
    video_rect: egui::Rect,
    pointer_over_video: bool,

    /// See [`netcmdr_session::align`].
    align: Option<Aligner>,
    /// The last alignment's live correction, so we can report when the
    /// device silently discards it.
    aligned: Option<String>,
    /// Config documents expected in reply to our own stored writes, so they
    /// aren't mistaken for the device re-deriving its capture.
    own_config: u32,

    switching: Option<Switch>,
    no_signal: bool,

    modifiers: Modifiers,
    dirty: bool,
    /// The window is only auto-sized once.
    sized_once: bool,
    /// Quit after this frame.
    quitting: bool,
    actions: Vec<Action>,
    end_reason: Option<String>,
    /// Cached from the last frame.
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
            align: None,
            aligned: None,
            own_config: 0,
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

    /// A transient status note.
    fn note(&mut self, text: impl Into<String>) {
        self.menu.note(text, NOTE_LINGER);
        self.dirty = true;
    }

    /// A note for work with no completion signal; device progress renews it.
    fn working(&mut self, text: impl Into<String>) {
        self.menu.note(text, WORKING_LINGER);
        self.dirty = true;
    }

    /// A note cleared by [`Menu::work_done`].
    fn working_on(&mut self, text: impl Into<String>, work: Work) {
        self.menu.note_work(text, WORKING_LINGER, work);
        self.dirty = true;
    }

    /// Device progress text, keeping the note's [`Work`].
    fn progress(&mut self, text: impl Into<String>) {
        self.menu.renew_note(text, WORKING_LINGER);
        self.dirty = true;
    }

    /// Report a failed command.
    fn failed(&mut self, what: &str, e: impl std::fmt::Display) {
        tracing::warn!(error = %e, "{what} failed");
        self.note(format!("{what} failed: {e}"));
    }

    /// Why the session ended, if it has.
    pub fn end_reason(&self) -> Option<&str> {
        self.end_reason.as_deref()
    }

    /// For the web client to pump each frame.
    pub fn session_mut(&mut self) -> &mut S {
        &mut self.session
    }

    /// Window title.
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
            Action::AlignCapture => self.start_align(),
            // Only report failures; the control shows the value.
            Action::SetVideoNumber(setting, value) => {
                if let Err(e) = self.session.set_video_number(setting, value) {
                    self.failed(setting.as_str(), e);
                }
            }
            Action::StoreVideoNumbers(changes) => {
                let Some(video) = self.menu.video.clone() else {
                    self.note("the device has not reported its capture settings yet");
                    return;
                };
                self.expect_own_config();
                match self.session.store_video_settings(&video, &changes) {
                    // Confirmed (or not) in `on_config`.
                    Ok(()) => self.working_on("saving to the device…", Work::VideoAdjust),
                    Err(e) => self.failed("save", e),
                }
            }
            Action::SetVideoFilter(filter) => match self.session.set_video_filter(filter) {
                // `auto` runs auto-adjust, which deserves a note.
                Ok(()) if filter == netcmdr_proto::command::VideoFilter::Auto => {
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
                    // Blank until confirmed; the old target's tiles keep
                    // arriving for seconds.
                    self.screen.blank();
                    self.no_signal = false;
                    self.menu.switching = true;
                    self.switching = Some(Switch {
                        port,
                        name: name.clone(),
                        since: Instant::now(),
                    });
                    // Abort alignment: it would measure the wrong target.
                    if let Some(aligner) = &mut self.align {
                        aligner.abort("the target changed");
                    }
                    self.aligned = None;
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
                // Tell the device so it frees the target immediately.
                if let Err(e) = self.session.send_xml(&netcmdr_proto::command::disconnect()) {
                    tracing::debug!(error = %e, "disconnect command not sent");
                }
                self.quitting = true;
            }
        }
    }

    /// Press a macro's keys in order, release in reverse. Each event's
    /// modifier state includes its own key, as the vendor client does.
    fn send_macro(&mut self, m: &KeyMacro) {
        // Ignore whatever the user is physically holding.
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

    /// The switch landed or timed out; resume drawing.
    fn finish_switch(&mut self) {
        if self.switching.take().is_some() {
            self.menu.switching = false;
            // Clear only the "switching" note.
            self.menu.work_done(Work::Switch);
            self.screen.blank();
            self.dirty = true;
        }
    }

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

    // ----------------------------------------------------------------- align

    /// Start measuring the capture offsets. Unlike auto-adjust, which restores
    /// the firmware table's (possibly wrong) values, this measures the picture.
    /// See [`netcmdr_session::align`].
    fn start_align(&mut self) {
        if self.align.is_some() {
            self.note("already aligning the capture");
            return;
        }
        if self.menu.video.is_none() {
            self.note("the device has not reported its capture settings yet");
            return;
        }
        self.align = Some(Aligner::new(align::Options::default()));
        self.working_on("aligning the capture…", Work::Align);
    }

    /// Poll the aligner and carry out its step.
    fn pump_align(&mut self) {
        let Some(mut aligner) = self.align.take() else {
            return;
        };
        // Stored values (session sets aren't reflected).
        let Some(video) = self.menu.video.clone() else {
            self.note("the device stopped reporting its capture settings");
            return;
        };

        let step = match self.session.frame().lock() {
            Ok(frame) => aligner.poll(Instant::now(), &frame, &video),
            Err(_) => {
                self.align = Some(aligner);
                return;
            }
        };

        match step {
            AlignStep::Set(setting, value) => {
                self.progress("aligning the capture…");
                if let Err(e) = self.session.set_video_number(setting, value) {
                    // Nothing to measure without the set.
                    aligner.abort(format!("could not set {}: {e}", setting.as_str()));
                }
            }
            AlignStep::Store(changes) => {
                self.progress("saving the capture position…");
                self.expect_own_config();
                if let Err(e) = self.session.store_video_settings(&video, &changes) {
                    aligner.abort(format!("could not save the capture position: {e}"));
                }
            }
            AlignStep::Wait => {}
            AlignStep::Done(report) => {
                self.menu.work_done(Work::Align);
                self.note(report.summary());
                self.aligned = report.changed().then(|| report.summary());
                self.dirty = true;
                return;
            }
            AlignStep::Failed(why) => {
                tracing::warn!(%why, "capture alignment gave up");
                self.menu.work_done(Work::Align);
                self.note(format!("could not align the capture: {why}"));
                self.dirty = true;
                return;
            }
        }

        // Keep repainting so the run is polled.
        self.dirty = true;
        self.align = Some(aligner);
    }

    /// Expect a config document in reply to our stored write.
    fn expect_own_config(&mut self) {
        self.own_config += 1;
    }

    /// The device re-derived its capture settings, discarding our correction.
    /// Just report it; re-running automatically could loop.
    fn align_lost(&mut self, why: &str) {
        if self.align.is_some() {
            return;
        }
        if let Some(what) = self.aligned.take() {
            tracing::info!(%what, %why, "capture alignment lost");
            self.note(format!("{why}; the capture alignment ({what}) is gone"));
        }
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
                    // Mid-switch frames are the old target's; decoded, not drawn.
                    if self.switching.is_none() {
                        self.no_signal = false;
                        self.screen.mark(rect);
                        // The only acknowledgement a refresh gets.
                        self.menu.work_done(Work::Repaint);
                        self.dirty = true;
                    }
                }
                Event::Resolution { width, height } => {
                    tracing::info!(width, height, "remote resolution");
                    // Only a changed mode invalidates measurements; the device
                    // also re-announces the current one.
                    if self.remote.is_some_and(|had| had != (width, height)) {
                        if let Some(aligner) = &mut self.align {
                            aligner.abort("the video mode changed");
                        }
                        self.align_lost("the video mode changed");
                    }
                    self.remote = Some((width, height));
                    self.menu.remote = Some((width, height));
                    self.no_signal = false;
                    // New geometry: the switch landed.
                    self.finish_switch();
                    self.screen.mark_all();
                    self.dirty = true;

                    // Size to the first mode only.
                    if !self.sized_once {
                        self.sized_once = true;
                        host.request_size(width as u32, height as u32 + self.bar_height());
                    }
                }
                Event::Noise(noise) => self.menu.noise = Some(noise),
                Event::NoSignal => {
                    // Also means the switch landed.
                    self.finish_switch();
                    self.no_signal = true;
                    self.screen.blank();
                    // No more noise reports will arrive.
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

    /// The device's config; after a port change it confirms the switch.
    fn on_config(&mut self, xml: &str, host: &dyn Host) {
        // Supersedes the adjustment window's values.
        if let Some(video) = netcmdr_session::parse_video_settings(xml) {
            self.menu.video = Some(video);
            // This is how auto-adjust signals completion.
            self.menu.work_done(Work::VideoAdjust);
            // Our own stored write's acknowledgement.
            if self.own_config > 0 {
                self.own_config -= 1;
                // The aligner has its own progress note.
                if self.align.is_none() {
                    self.note("saved to the device");
                }
            } else {
                // The device re-derived the offsets itself.
                self.align_lost("the device re-adjusted its capture");
            }
            self.dirty = true;
        }

        let servers = netcmdr_session::parse_servers(xml);
        if let Some(active) = servers.iter().find(|s| s.active) {
            // Confirms a switch between targets with the same resolution.
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

    /// Process pending events; false once the session has ended.
    pub fn poll(&mut self, host: &dyn Host) -> bool {
        if self.quitting {
            return false;
        }
        self.pump_mgmt(host);
        if !self.pump_session(host) {
            return false;
        }
        self.pump_align();
        if self
            .switching
            .as_ref()
            .is_some_and(|s| s.since.elapsed() > SWITCH_TIMEOUT)
        {
            tracing::warn!("target switch was never confirmed; showing video again");
            self.finish_switch();
        }
        // Expire notes even when the session is quiet.
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

        // Upload inside the pass; nothing mid-switch.
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

    /// Carry out the frame's actions, outside the egui pass.
    pub fn finish_frame(&mut self, host: &dyn Host) {
        self.dirty = false;
        for action in std::mem::take(&mut self.actions) {
            self.act(action, host);
        }
    }

    /// Menu bar height in physical pixels.
    fn bar_height(&self) -> u32 {
        // `Rect::NOTHING` before the first pass.
        let top = self.video_rect.top() * self.pixels_per_point;
        if !top.is_finite() {
            return 0;
        }
        top.round().max(0.0) as u32
    }

    pub fn needs_repaint(&self) -> bool {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Window point to remote pixel.
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

    pub fn keyboard_goes_to_target(&self) -> bool {
        !self.ui_wants_keyboard && !self.menu.captures_keyboard()
    }

    pub fn pointer_over_video(&self) -> bool {
        self.pointer_over_video
    }

    /// In egui points.
    pub fn on_pointer_moved(&mut self, point: egui::Pos2) {
        if let Some((x, y)) = self.to_remote(point) {
            self.send(Input::MouseMove { x, y });
        }
    }

    /// Forwarded only over the video.
    pub fn on_mouse_button(&mut self, button: MouseButton, down: bool) {
        if self.pointer_over_video {
            self.send(Input::MouseButton { button, down });
        }
    }

    /// Forwarded only over the video.
    pub fn on_scroll(&mut self, notches: i32) {
        if notches != 0 && self.pointer_over_video {
            self.send(Input::MouseWheel(notches));
        }
    }

    pub fn set_modifiers(&mut self, shift: bool, ctrl: bool, alt: bool) {
        self.modifiers.shift = shift;
        self.modifiers.ctrl = ctrl;
        self.modifiers.alt = alt;
    }

    /// Release all modifiers on focus loss.
    pub fn on_focus_lost(&mut self) {
        self.release_held_keys();
        self.modifiers = Modifiers::default();
    }

    pub fn on_key(&mut self, code: PhysicalKey, down: bool, host: &dyn Host) {
        // Local shortcuts (also in the menu), not forwarded.
        if self.modifiers.ctrl && self.modifiers.alt && down {
            match code {
                PhysicalKey::End => return self.act(Action::CtrlAltDel, host),
                PhysicalKey::Home => return self.act(Action::SyncPointer, host),
                // Shifted: our aligner instead of the device's auto-adjust.
                PhysicalKey::KeyA if self.modifiers.shift => {
                    return self.act(Action::AlignCapture, host)
                }
                PhysicalKey::KeyA => return self.act(Action::AutoAdjustVideo, host),
                _ => {}
            }
        }

        // Tracked locally; the platform doesn't expose LED state.
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

/// Placeholder shown when there's no picture.
fn draw_placeholder(ui: &mut egui::Ui, waiting: &Waiting) {
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            // Nudge down to centre within `centered_and_justified`.
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

/// Draw the remote screen and return its rect, or [`egui::Rect::NOTHING`] for
/// a placeholder (which disables pointer mapping).
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
                    // 1:1. Pan only via scroll bars; wheel and drags go to the
                    // target.
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

#[cfg(test)]
mod tests {
    use super::*;
    use netcmdr_proto::command::{VideoFilter, VideoSetting};
    use netcmdr_session::video::Framebuffer;
    use netcmdr_session::{Error, SharedFrame};
    use std::sync::{Arc, Mutex};

    /// Records requests and shows a fixed picture. The aligner itself is
    /// tested in `netcmdr_session::align`.
    #[derive(Default)]
    struct FakeSession {
        frame: SharedFrame,
        events: Vec<Event>,
        sets: Vec<(VideoSetting, i32)>,
        auto_adjusts: u32,
        stores: Vec<Vec<(VideoSetting, i32)>>,
    }

    impl FakeSession {
        /// Enough content to measure.
        fn with_picture() -> FakeSession {
            let mut fb = Framebuffer::new();
            fb.resize(64, 64);
            for y in 16..48 {
                for x in 16..48 {
                    fb.pixels[y * 64 + x] = 0x00FF_FFFF;
                }
            }
            FakeSession {
                frame: Arc::new(Mutex::new(fb)),
                ..FakeSession::default()
            }
        }
    }

    impl SessionLink for FakeSession {
        fn frame(&self) -> &SharedFrame {
            &self.frame
        }
        fn next_event(&mut self) -> Option<Event> {
            if self.events.is_empty() {
                None
            } else {
                Some(self.events.remove(0))
            }
        }
        fn is_ended(&self) -> bool {
            false
        }
        fn send_input(&mut self, _: Input) -> Result<(), Error> {
            Ok(())
        }
        fn send_xml(&mut self, _: &str) -> Result<(), Error> {
            Ok(())
        }
        fn refresh_screen(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn auto_adjust_video(&mut self) -> Result<(), Error> {
            self.auto_adjusts += 1;
            Ok(())
        }
        fn stop_auto_adjust(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn set_video_number(&mut self, setting: VideoSetting, value: i32) -> Result<(), Error> {
            self.sets.push((setting, value));
            Ok(())
        }
        fn store_video_settings(
            &mut self,
            _: &netcmdr_session::VideoSettings,
            changes: &[(VideoSetting, i32)],
        ) -> Result<(), Error> {
            self.stores.push(changes.to_vec());
            Ok(())
        }
        fn set_video_filter(&mut self, _: VideoFilter) -> Result<(), Error> {
            Ok(())
        }
        fn change_target(&mut self, _: &str, _: u32) -> Result<(), Error> {
            Ok(())
        }
        fn calibrate_mouse(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn align_mouse(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn set_exclusive(&mut self, _: bool) -> Result<(), Error> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeMgmt;

    impl MgmtLink for FakeMgmt {
        fn request_targets(&mut self) {}
        fn request_power(&mut self, _: u32, _: netcmdr_mgmt::PowerCommand) {}
        fn next_reply(&mut self) -> Option<MgmtReply> {
            None
        }
        fn busy(&self) -> bool {
            false
        }
    }

    struct FakeHost;

    impl Host for FakeHost {
        fn set_title(&self, _: &str) {}
        fn set_cursor(&self, _: bool, _: crate::menu::CursorMode) {}
    }

    const VIDEO_XML: &str = "<client_config><video_settings cx='1360' cy='768' fq='60' \
                             hoffset='368' hoffset_min='113' hoffset_max='431' \
                             voffset='24' voffset_min='7' voffset_max='26' \
                             brightness='50' contrast='50' phase='4'/></client_config>";

    fn viewer(session: FakeSession) -> Viewer<FakeSession, FakeMgmt> {
        Viewer::new(
            session,
            FakeMgmt,
            Config {
                host: "device".into(),
                user: "admin".into(),
                target: 1,
                exclusive: true,
            },
        )
    }

    #[test]
    fn aligning_before_the_device_has_reported_says_so() {
        let mut v = viewer(FakeSession::with_picture());
        v.act(Action::AlignCapture, &FakeHost);
        assert!(v.align.is_none(), "nothing to measure against yet");

        v.poll(&FakeHost);
        assert!(v.session.sets.is_empty());
    }

    #[test]
    fn aligning_puts_the_first_probe_on_the_wire() {
        let mut v = viewer(FakeSession::with_picture());
        v.on_config(VIDEO_XML, &FakeHost);
        v.act(Action::AlignCapture, &FakeHost);
        assert!(v.align.is_some());

        v.poll(&FakeHost);
        assert_eq!(
            v.session.sets,
            vec![(VideoSetting::HOffset, 352)],
            "16 below the device's default is where the measurement starts"
        );
    }

    /// A config reply during alignment is its own write's acknowledgement.
    #[test]
    fn a_config_mid_run_belongs_to_the_run() {
        let mut v = viewer(FakeSession::with_picture());
        v.on_config(VIDEO_XML, &FakeHost);
        v.aligned = Some("hoffset 368 → 364".into());
        v.act(Action::AlignCapture, &FakeHost);

        v.on_config(VIDEO_XML, &FakeHost);
        assert!(
            v.aligned.is_some(),
            "the run is not news of its own undoing"
        );
    }

    /// Likewise for a save from the adjustment window.
    #[test]
    fn a_config_answering_a_save_is_not_a_correction_lost() {
        let mut v = viewer(FakeSession::with_picture());
        v.on_config(VIDEO_XML, &FakeHost);
        v.aligned = Some("hoffset 368 → 364".into());

        v.act(
            Action::StoreVideoNumbers(vec![(VideoSetting::HOffset, 364)]),
            &FakeHost,
        );
        assert_eq!(v.session.stores, vec![vec![(VideoSetting::HOffset, 364)]]);

        v.on_config(VIDEO_XML, &FakeHost);
        assert!(v.aligned.is_some(), "nothing was lost; it was saved");
        assert!(
            v.menu.note_text().is_some_and(|n| n.contains("saved")),
            "the bar says so: {:?}",
            v.menu.note_text()
        );
    }

    #[test]
    fn the_device_readjusting_reports_the_correction_gone() {
        let mut v = viewer(FakeSession::with_picture());
        v.aligned = Some("hoffset 368 → 364".into());

        v.on_config(VIDEO_XML, &FakeHost);
        assert!(v.aligned.is_none());
        assert!(
            v.menu.note_text().is_some_and(|n| n.contains("is gone")),
            "the bar says so: {:?}",
            v.menu.note_text()
        );
    }

    /// A mode change aborts and restores the probed value.
    #[test]
    fn a_mode_change_abandons_a_run_and_restores_the_capture() {
        let mut v = viewer(FakeSession::with_picture());
        v.on_config(VIDEO_XML, &FakeHost);
        v.act(Action::AlignCapture, &FakeHost);
        v.poll(&FakeHost);
        assert_eq!(v.session.sets.len(), 1);

        v.remote = Some((1360, 768));
        v.session.events.push(Event::Resolution {
            width: 1024,
            height: 768,
        });
        v.poll(&FakeHost);
        assert_eq!(
            v.session.stores,
            vec![vec![(VideoSetting::HOffset, 368)]],
            "the probe was written back"
        );

        v.poll(&FakeHost);
        assert!(v.align.is_none(), "the run is over");
        assert!(
            v.menu
                .note_text()
                .is_some_and(|n| n.contains("the video mode changed")),
            "{:?}",
            v.menu.note_text()
        );
    }

    /// A target switch aborts the run (resolutions may match).
    #[test]
    fn switching_target_abandons_a_run() {
        let mut v = viewer(FakeSession::with_picture());
        v.on_config(VIDEO_XML, &FakeHost);
        v.act(Action::AlignCapture, &FakeHost);
        v.poll(&FakeHost);

        v.act(
            Action::SwitchTarget {
                port: 2,
                name: "other".into(),
            },
            &FakeHost,
        );
        v.poll(&FakeHost);
        assert_eq!(
            v.session.stores,
            vec![vec![(VideoSetting::HOffset, 368)]],
            "the probe was written back"
        );

        v.poll(&FakeHost);
        assert!(v.align.is_none());
    }

    #[test]
    fn the_same_mode_announced_again_leaves_a_run_alone() {
        let mut v = viewer(FakeSession::with_picture());
        v.on_config(VIDEO_XML, &FakeHost);
        v.act(Action::AlignCapture, &FakeHost);
        v.poll(&FakeHost);

        v.remote = Some((1360, 768));
        v.session.events.push(Event::Resolution {
            width: 1360,
            height: 768,
        });
        v.poll(&FakeHost);

        assert!(v.align.is_some(), "still measuring");
        assert_eq!(v.session.auto_adjusts, 0);
    }
}
