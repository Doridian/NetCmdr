//! The window: the menu bar, the remote screen, and the input routing between
//! them.
//!
//! Two things share one window, and the interesting part is deciding which gets
//! an event. Everything is handed to egui first; whatever egui does not want
//! goes to the target. That single rule keeps the menu usable without the
//! target seeing stray clicks, and keeps typing going to the target without the
//! menu swallowing it — the device wants **absolute coordinates in remote
//! screen pixels**, so pointer positions are mapped back through the rectangle
//! the image was actually drawn in.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use b070_session::{Event, Input, Modifiers, MouseButton, Session};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowId};

use crate::keys::scancode;
use crate::macros::{self, KeyMacro};
use crate::menu::{Action, CursorMode, Menu, Scaling};
use crate::mgmt::{self, Mgmt};
use crate::render::{Renderer, Screen};

/// How often the session's events are drained and the window repainted. The
/// device sends tile updates far faster than a screen refreshes.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How long to keep the screen blank waiting for a target switch to land.
///
/// The device takes its time: measured against the hardware, a port change
/// answered after 4.5 s when the new target was driving video and 9.6 s when it
/// was not. This is only a backstop so a switch that never completes cannot
/// leave the window blank forever.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(25);

/// Modifiers that must be released when the window loses focus, or the target
/// is left believing one is still held.
const STICKY_MODIFIERS: [KeyCode; 8] = [
    KeyCode::ControlLeft,
    KeyCode::ControlRight,
    KeyCode::AltLeft,
    KeyCode::AltRight,
    KeyCode::ShiftLeft,
    KeyCode::ShiftRight,
    KeyCode::SuperLeft,
    KeyCode::SuperRight,
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
    /// Kept for power control and the target list, both of which live on the
    /// management API rather than the session.
    pub client: Arc<b070_mgmt::Client>,
}

pub fn run(session: Session, cfg: Config) -> Result<()> {
    let event_loop = EventLoop::new().context("creating the event loop")?;
    event_loop.set_control_flow(ControlFlow::wait_duration(FRAME_INTERVAL));
    let mut app = App::new(session, cfg);
    event_loop.run_app(&mut app).context("running the viewer")?;
    std::mem::replace(&mut app.result, Ok(()))
}

struct App {
    session: Session,
    mgmt: Mgmt,
    menu: Menu,

    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    renderer: Option<Renderer>,
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
    result: Result<()>,
}

impl App {
    fn new(session: Session, cfg: Config) -> App {
        let mut menu = Menu::new(cfg.host.clone(), cfg.user.clone(), cfg.target);
        menu.exclusive = cfg.exclusive;

        let mut mgmt = Mgmt::spawn(cfg.client);
        mgmt.send(mgmt::Request::Targets);

        App {
            session,
            mgmt,
            menu,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            renderer: None,
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
            result: Ok(()),
        }
    }

    fn send(&self, input: Input) {
        if let Err(e) = self.session.send_input(input) {
            tracing::warn!(error = %e, "input dropped");
        }
    }

    fn note(&mut self, text: impl Into<String>) {
        self.menu.status = text.into();
        self.dirty = true;
    }

    /// Report a command that failed, without taking the session down with it.
    fn failed(&mut self, what: &str, e: impl std::fmt::Display) {
        tracing::warn!(error = %e, "{what} failed");
        self.note(format!("{what} failed: {e}"));
    }

    fn title(&self) -> String {
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

    fn act(&mut self, action: Action) {
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
                Ok(()) => self.note("calibrating the mouse…"),
                Err(e) => self.failed("calibrate", e),
            },
            Action::AlignMouse => match self.session.align_mouse() {
                Ok(()) => self.note("aligning the mouse…"),
                Err(e) => self.failed("align", e),
            },
            Action::SetCursorMode(mode) => {
                self.menu.cursor = mode;
                self.dirty = true;
            }

            Action::RefreshVideo => match self.session.refresh_screen() {
                Ok(()) => self.note("refreshing…"),
                Err(e) => self.failed("refresh", e),
            },
            Action::AutoAdjustVideo => match self.session.auto_adjust_video() {
                Ok(()) => self.note("adjusting video…"),
                Err(e) => self.failed("auto adjust", e),
            },
            Action::StopAutoAdjust => match self.session.stop_auto_adjust() {
                Ok(()) => self.note("stopped adjusting"),
                Err(e) => self.failed("stop adjusting", e),
            },
            Action::SetVideoNumber(setting, value) => {
                match self.session.set_video_number(setting, value) {
                    Ok(()) => self.note(format!("{} = {value}", setting.as_str())),
                    Err(e) => self.failed(setting.as_str(), e),
                }
            }
            Action::SetVideoFilter(filter) => match self.session.set_video_filter(filter) {
                // Picking `auto` runs the adjustment rather than setting
                // anything, so say what actually happened.
                Ok(()) if filter == b070_proto::command::VideoFilter::Auto => {
                    self.note("adjusting video…")
                }
                Ok(()) => self.note(format!("filter = {}", filter.as_str())),
                Err(e) => self.failed("filter", e),
            },
            Action::SetScaling(mode) => {
                self.menu.scaling = mode;
                self.dirty = true;
            }
            Action::ResizeToRemote => {
                if let (Some((w, h)), Some(renderer)) = (self.remote, &self.renderer) {
                    let bar = self.video_rect.top() * self.egui_ctx.pixels_per_point();
                    let _ = renderer.window.request_inner_size(PhysicalSize::new(
                        w as u32,
                        h as u32 + bar.round() as u32,
                    ));
                }
            }

            Action::Power(command) => {
                let port = self.menu.current_port;
                self.mgmt.send(mgmt::Request::Power { port, command });
                self.note("sending power command…");
            }

            Action::SwitchTarget { port, name } => match self.session.change_target(&name, port) {
                Ok(()) => {
                    self.menu.current_port = port;
                    self.menu.current_name = name.clone();
                    if let Some(r) = &self.renderer {
                        r.window.set_title(&self.title());
                    }
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
                    self.note(format!("switching to {name}…"));
                }
                Err(e) => self.failed("switch target", e),
            },
            Action::RefreshTargets => {
                self.mgmt.send(mgmt::Request::Targets);
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
            if let Some((scancode, extended)) = scancode(*key) {
                self.send(Input::Key {
                    scancode,
                    extended,
                    down: true,
                    modifiers: mods,
                });
            }
        }
        for key in m.keys.iter().rev() {
            if let Some((scancode, extended)) = scancode(*key) {
                self.send(Input::Key {
                    scancode,
                    extended,
                    down: false,
                    modifiers: mods,
                });
            }
            apply_modifier(&mut mods, *key, false);
        }
        self.note(format!("sent {}", m.label));
    }

    /// Send a key-up for every modifier, held or not.
    fn release_held_keys(&mut self) {
        for key in STICKY_MODIFIERS {
            if let Some((scancode, extended)) = scancode(key) {
                self.send(Input::Key {
                    scancode,
                    extended,
                    down: false,
                    modifiers: Modifiers::default(),
                });
            }
        }
        self.modifiers.shift = false;
        self.modifiers.ctrl = false;
        self.modifiers.alt = false;
    }

    /// The switch has landed (or given up): start drawing video again.
    fn finish_switch(&mut self) {
        if self.switching.take().is_some() {
            self.menu.switching = false;
            // "switching to X…" has stopped being true; whatever happens next
            // writes its own note.
            self.menu.status.clear();
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
    fn pump_session(&mut self) -> bool {
        loop {
            match self.session.events.try_recv() {
                Ok(Event::Frame(rect)) => {
                    // Frames arriving mid-switch are still the old target's;
                    // they are decoded into the framebuffer either way, but
                    // nothing is drawn from it until the switch lands.
                    if self.switching.is_none() {
                        self.no_signal = false;
                        self.screen.mark(rect);
                        self.dirty = true;
                    }
                }
                Ok(Event::Resolution { width, height }) => {
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
                        if let Some(renderer) = &self.renderer {
                            let bar =
                                (self.video_rect.top() * self.egui_ctx.pixels_per_point()).round();
                            let _ = renderer.window.request_inner_size(PhysicalSize::new(
                                width as u32,
                                height as u32 + bar as u32,
                            ));
                        }
                    }
                }
                Ok(Event::Noise(noise)) => self.menu.noise = Some(noise),
                Ok(Event::NoSignal) => {
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
                Ok(Event::Gui(msg)) if !msg.is_empty() => {
                    tracing::info!(%msg, "device message");
                    self.note(msg);
                }
                Ok(Event::Gui(_)) => {}
                Ok(Event::ConfigXml(xml)) => self.on_config(&xml),
                Ok(Event::Disconnected(reason)) => {
                    tracing::info!(%reason, "session ended");
                    return false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// The device's own view of which target is on screen.
    ///
    /// It re-sends this after a port change, so it is what confirms the switch
    /// actually happened rather than what we asked for.
    fn on_config(&mut self, xml: &str) {
        // The device re-derives these whenever it re-reads the video mode, and
        // an auto-adjust answers with a fresh document — so whatever it says
        // here supersedes anything the adjustment window has been told.
        if let Some(video) = b070_session::parse_video_settings(xml) {
            self.menu.video = Some(video);
            self.dirty = true;
        }

        let servers = b070_session::parse_servers(xml);
        if let Some(active) = servers.iter().find(|s| s.active) {
            // Two targets can share a resolution, in which case no `Resolution`
            // event marks the switch — this document is what confirms it.
            if self.switching.as_ref().is_some_and(|s| s.port == active.port) {
                self.finish_switch();
            }
            let changed = active.port != self.menu.current_port;
            self.menu.current_port = active.port;
            if !active.name.is_empty() {
                self.menu.current_name = active.name.clone();
            }
            if changed {
                if let Some(r) = &self.renderer {
                    r.window.set_title(&self.title());
                }
                self.screen.mark_all();
                self.dirty = true;
            }
        }
    }

    fn pump_mgmt(&mut self) {
        while let Ok(reply) = self.mgmt.replies.try_recv() {
            self.mgmt.answered();
            match reply {
                mgmt::Reply::Targets(targets) => {
                    if let Some(t) = targets.iter().find(|t| t.port == self.menu.current_port) {
                        self.menu.current_name = t.name.clone();
                        if let Some(r) = &self.renderer {
                            r.window.set_title(&self.title());
                        }
                    }
                    self.menu.targets = targets;
                }
                mgmt::Reply::Note(text) => self.note(text),
                mgmt::Reply::Failed(text) => {
                    tracing::warn!(%text, "management request failed");
                    self.note(text);
                }
            }
            self.dirty = true;
        }
        self.menu.busy = self.mgmt.busy();
    }

    // ------------------------------------------------------------------- draw

    fn draw(&mut self) {
        let waiting = self.waiting();
        let (Some(renderer), Some(state)) = (&mut self.renderer, &mut self.egui_state) else {
            return;
        };
        let window = renderer.window.clone();
        let raw_input = state.take_egui_input(&window);

        // Uploading inside the egui pass keeps the texture and the frame that
        // draws it in step; the lock is held only for the copy. Mid-switch
        // there is deliberately nothing to upload.
        if waiting.is_none() || matches!(waiting, Some(Waiting::FirstFrame)) {
            if let Ok(frame) = self.session.frame().lock() {
                self.screen.upload(&self.egui_ctx, &frame);
            }
        }

        let mut actions = Vec::new();
        let menu = &mut self.menu;
        let screen = &self.screen;
        let scaling = menu.scaling;
        let mut video_rect = egui::Rect::NOTHING;

        let output = self.egui_ctx.run_ui(raw_input, |ui| {
            actions = menu.show(ui);
            video_rect = draw_screen(ui, screen, scaling, waiting.as_ref());
        });

        self.video_rect = video_rect;
        self.pointer_over_video = self
            .egui_ctx
            .pointer_latest_pos()
            .is_some_and(|p| video_rect.contains(p))
            && !self.egui_ctx.is_pointer_over_egui();

        state.handle_platform_output(&window, output.platform_output.clone());
        apply_cursor(&window, self.pointer_over_video, self.menu.cursor);
        renderer.paint(&self.egui_ctx, output);

        self.dirty = false;
        for action in actions {
            self.act(action);
        }
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

    /// Window coordinates (physical) to egui points.
    fn to_points(&self, pos: winit::dpi::PhysicalPosition<f64>) -> egui::Pos2 {
        let ppp = self.egui_ctx.pixels_per_point().max(f32::EPSILON);
        egui::pos2(pos.x as f32 / ppp, pos.y as f32 / ppp)
    }

    /// Whether keystrokes belong to the target rather than to the UI.
    fn keyboard_goes_to_target(&self) -> bool {
        !self.egui_ctx.egui_wants_keyboard_input() && !self.menu.captures_keyboard()
    }

    fn on_key(&mut self, event: &winit::event::KeyEvent) {
        let PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let down = event.state == ElementState::Pressed;

        // Local shortcuts, not forwarded: the target can never see
        // Ctrl+Alt+Delete otherwise, and the pointer needs a resync whenever
        // the target's own acceleration drifts from ours. Each has a menu entry
        // too; these are the fast paths.
        if self.modifiers.ctrl && self.modifiers.alt && down {
            match code {
                KeyCode::End => return self.act(Action::CtrlAltDel),
                KeyCode::Home => return self.act(Action::SyncPointer),
                KeyCode::KeyA => return self.act(Action::AutoAdjustVideo),
                _ => {}
            }
        }

        // Lock keys are tracked locally: the platform does not tell us the LED
        // state, and the device only uses these as a hint.
        if down {
            match code {
                KeyCode::CapsLock => self.modifiers.caps_lock = !self.modifiers.caps_lock,
                KeyCode::NumLock => self.modifiers.num_lock = !self.modifiers.num_lock,
                KeyCode::ScrollLock => self.modifiers.scroll_lock = !self.modifiers.scroll_lock,
                _ => {}
            }
        }

        match scancode(code) {
            Some((scancode, extended)) => {
                let modifiers = self.modifiers;
                self.send(Input::Key {
                    scancode,
                    extended,
                    down,
                    modifiers,
                });
            }
            None => tracing::debug!(?code, "no scancode for key"),
        }
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

/// Our pointer style wins over egui's, but only while the pointer is over the
/// video: everywhere else egui owns it, so its resize and text cursors survive.
fn apply_cursor(window: &Window, over_video: bool, mode: CursorMode) {
    if !over_video {
        window.set_cursor_visible(true);
        return;
    }
    match mode {
        CursorMode::Arrow => {
            window.set_cursor_visible(true);
            window.set_cursor(CursorIcon::Default);
        }
        CursorMode::Crosshair => {
            window.set_cursor_visible(true);
            window.set_cursor(CursorIcon::Crosshair);
        }
        CursorMode::Hidden => window.set_cursor_visible(false),
    }
}

/// Track a modifier key's contribution to the word sent with each event.
fn apply_modifier(mods: &mut Modifiers, key: KeyCode, down: bool) {
    match key {
        KeyCode::ControlLeft | KeyCode::ControlRight => mods.ctrl = down,
        KeyCode::AltLeft | KeyCode::AltRight => mods.alt = down,
        KeyCode::ShiftLeft | KeyCode::ShiftRight => mods.shift = down,
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }
        let (w, h) = self.remote.unwrap_or((1024, 768));
        let attrs = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(PhysicalSize::new(w as u32, h as u32));

        match Renderer::new(event_loop, attrs) {
            Ok(renderer) => {
                let state = egui_winit::State::new(
                    self.egui_ctx.clone(),
                    egui::ViewportId::ROOT,
                    &renderer.window,
                    Some(renderer.window.scale_factor() as f32),
                    None,
                    None,
                );
                self.egui_state = Some(state);
                self.renderer = Some(renderer);
            }
            Err(e) => {
                self.result = Err(e);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(renderer), Some(state)) = (&self.renderer, &mut self.egui_state) else {
            return;
        };
        let window = renderer.window.clone();
        // egui sees everything first; `consumed` is what says the UI wanted it.
        let response = state.on_window_event(&window, &event);
        if response.repaint {
            self.dirty = true;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(r) = &self.renderer {
                    r.resize();
                }
                self.dirty = true;
            }
            WindowEvent::ModifiersChanged(mods) => {
                let s = mods.state();
                self.modifiers.shift = s.shift_key();
                self.modifiers.ctrl = s.control_key();
                self.modifiers.alt = s.alt_key();
            }
            WindowEvent::CursorMoved { position, .. } => {
                if response.consumed {
                    return;
                }
                let point = self.to_points(position);
                if let Some((x, y)) = self.to_remote(point) {
                    self.send(Input::MouseMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if response.consumed || !self.pointer_over_video {
                    return;
                }
                let button = match button {
                    winit::event::MouseButton::Left => Some(MouseButton::Left),
                    winit::event::MouseButton::Middle => Some(MouseButton::Middle),
                    winit::event::MouseButton::Right => Some(MouseButton::Right),
                    _ => None,
                };
                if let Some(button) = button {
                    self.send(Input::MouseButton {
                        button,
                        down: state == ElementState::Pressed,
                    });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if response.consumed || !self.pointer_over_video {
                    return;
                }
                let notches = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0).round() as i32,
                };
                if notches != 0 {
                    self.send(Input::MouseWheel(notches));
                }
            }
            WindowEvent::Focused(false) => {
                self.release_held_keys();
                self.modifiers = Modifiers::default();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if response.consumed || !self.keyboard_goes_to_target() {
                    return;
                }
                self.on_key(&event);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.quitting || !self.pump_session() {
            event_loop.exit();
            return;
        }
        self.pump_mgmt();

        // A switch the device never confirms must not blank the window forever.
        if self
            .switching
            .as_ref()
            .is_some_and(|s| s.since.elapsed() > SWITCH_TIMEOUT)
        {
            tracing::warn!("the device did not confirm the target switch");
            self.finish_switch();
            self.note("target switch not confirmed; showing video anyway");
        }

        if self.dirty || self.egui_ctx.has_requested_repaint() {
            if let Some(r) = &self.renderer {
                r.window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_INTERVAL));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(r) = &mut self.renderer {
            r.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_modifier_key_sets_and_clears_its_bit() {
        let mut mods = Modifiers::default();
        apply_modifier(&mut mods, KeyCode::ControlLeft, true);
        apply_modifier(&mut mods, KeyCode::AltRight, true);
        assert!(mods.ctrl && mods.alt && !mods.shift);
        apply_modifier(&mut mods, KeyCode::ControlLeft, false);
        assert!(!mods.ctrl && mods.alt);
    }

    #[test]
    fn a_normal_key_leaves_the_modifier_word_alone() {
        let mut mods = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        apply_modifier(&mut mods, KeyCode::KeyA, true);
        assert!(mods.ctrl);
        apply_modifier(&mut mods, KeyCode::Delete, false);
        assert!(mods.ctrl);
    }

    #[test]
    fn every_waiting_state_says_something_and_only_spins_when_it_should() {
        let states = [
            Waiting::FirstFrame,
            Waiting::Switching {
                name: "IslandFox".into(),
                port: 2,
            },
            Waiting::NoSignal,
        ];
        for state in &states {
            assert!(!state.message().is_empty());
        }
        assert!(states[0].busy());
        assert!(states[1].busy());
        // Nothing is on its way when the target simply is not driving video.
        assert!(!states[2].busy());
    }

    #[test]
    fn a_switch_names_the_target_it_is_going_to() {
        let w = Waiting::Switching {
            name: "IslandFox".into(),
            port: 2,
        };
        assert!(w.message().contains("IslandFox"));
        assert_eq!(w.detail().as_deref(), Some("port 2"));
        assert_eq!(Waiting::FirstFrame.detail(), None);
    }

    /// Everything the focus-loss handler releases must have a scancode, or the
    /// target keeps believing the key is held.
    #[test]
    fn every_sticky_modifier_can_actually_be_released() {
        for key in STICKY_MODIFIERS {
            assert!(scancode(key).is_some(), "{key:?}");
        }
    }
}
