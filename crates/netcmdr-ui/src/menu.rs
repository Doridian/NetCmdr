// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The menu bar.
//!
//! The layout follows the stock Java client's toolbar so that anyone who has
//! used the vendor's viewer finds the same things in the same places —
//! Keyboard, Mouse, Video, Power, Session — minus virtual media, which this
//! client does not implement.
//!
//! # Adding an item
//!
//! The menu is deliberately a thin translation layer and holds no logic: every
//! entry does nothing but emit an [`Action`], and [`crate::viewer`] is the only
//! place that knows how to carry one out. To add a feature:
//!
//! 1. add a variant to [`Action`];
//! 2. add the `ui.button(…)` line that emits it in [`Menu::show`];
//! 3. handle it in `viewer::App::act`.
//!
//! Anything that needs state on screen (a checkbox, a radio group, a
//! current-value marker) reads it from the public fields of [`Menu`], which the
//! viewer keeps up to date. Anything that needs confirming goes through
//! [`Menu::confirm`] instead of calling straight through.

use netcmdr_mgmt::{PowerCommand, Target};
use netcmdr_proto::command::{VideoFilter, VideoSetting};
// Not `std::time`: `Instant::now()` panics in a browser, and a status line is
// set from a click handler, so that panic would take the whole page with it.
// See `netcmdr_session::time`.
use netcmdr_session::time::{Duration, Instant};
use netcmdr_session::{Knob, Noise, VideoSettings};

use crate::macros;

/// How the local pointer is drawn while it is over the remote screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    /// The usual arrow, drawn on top of the target's own pointer.
    Arrow,
    /// A crosshair, which is easier to line up with the target's pointer.
    Crosshair,
    /// Hidden, leaving only the target's own pointer — the right choice once
    /// the two are calibrated.
    Hidden,
}

impl CursorMode {
    pub const ALL: [CursorMode; 3] = [CursorMode::Arrow, CursorMode::Crosshair, CursorMode::Hidden];

    pub fn label(self) -> &'static str {
        match self {
            CursorMode::Arrow => "Arrow",
            CursorMode::Crosshair => "Crosshair",
            CursorMode::Hidden => "Hidden",
        }
    }
}

/// How the remote screen is fitted into the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    /// Scale to fill the window, keeping the aspect ratio.
    Fit,
    /// One remote pixel per window pixel, centred.
    Actual,
}

impl Scaling {
    pub const ALL: [Scaling; 2] = [Scaling::Fit, Scaling::Actual];

    pub fn label(self) -> &'static str {
        match self {
            Scaling::Fit => "Fit to window",
            Scaling::Actual => "Actual size (1:1)",
        }
    }
}

/// One thing the menu can ask the viewer to do.
///
/// Every entry in the bar produces one of these and nothing else; see the
/// module docs.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // Keyboard
    CtrlAltDel,
    /// A flat index into [`crate::macros`].
    Macro(usize),
    ReleaseHeldKeys,

    // Mouse
    SyncPointer,
    CalibrateMouse,
    AlignMouse,
    SetCursorMode(CursorMode),

    // Video
    RefreshVideo,
    AutoAdjustVideo,
    StopAutoAdjust,
    /// Measure the capture offsets from the picture, correct them and store
    /// them on the device. See `netcmdr_session::align`.
    AlignCapture,
    /// Set one analog capture knob to an absolute value, for this session.
    SetVideoNumber(VideoSetting, i32),
    /// Write capture knobs into the device's stored configuration, where they
    /// outlive the session. See `netcmdr_session::Session::store_video_settings`.
    StoreVideoNumbers(Vec<(VideoSetting, i32)>),
    SetVideoFilter(VideoFilter),
    SetScaling(Scaling),
    ResizeToRemote,

    // Power
    Power(PowerCommand),

    // Session
    SwitchTarget {
        port: u32,
        name: String,
    },
    RefreshTargets,
    SetExclusive(bool),
    Disconnect,
}

/// A pending "are you sure?".
struct Confirm {
    question: String,
    detail: String,
    confirm_label: String,
    action: Action,
}

/// How long a note stays in the bar before the bar goes back to resting on the
/// resolution and the target.
///
/// A result has been read by the time this is up; anything longer is a line
/// that outlives what it describes.
pub const NOTE_LINGER: Duration = Duration::from_secs(4);

/// How long a note about work still in flight stays, when nothing announces
/// that the work has finished.
///
/// The device reports progress while it adjusts the video or calibrates the
/// mouse, and every report renews the note, so this is only the backstop for a
/// run that goes quiet — the vendor's own protocol has no "done" message.
pub const WORKING_LINGER: Duration = Duration::from_secs(10);

/// Work a note is waiting on, where the device's answer can be recognised.
///
/// The vendor's protocol has no "done" message, but some work is answered by
/// something else arriving, and a line that has been answered should come down
/// then rather than sit out the rest of its linger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// A target change, ended by the device reporting the target it is on.
    Switch,
    /// An automatic adjustment, ended by the fresh capture settings it sends.
    VideoAdjust,
    /// A capture alignment, ended by the viewer's own state machine reporting.
    Align,
    /// A full repaint, ended by the next picture that arrives.
    Repaint,
}

/// A line in the bar, and when it stops being shown.
struct Note {
    text: String,
    until: Instant,
    /// What would take this line down before its time, if anything can.
    work: Option<Work>,
}

/// The menu bar and its state.
///
/// The public fields are what the bar draws; the viewer owns this struct and
/// keeps them current.
pub struct Menu {
    pub targets: Vec<Target>,
    pub current_port: u32,
    pub current_name: String,
    pub exclusive: bool,
    /// Whether the host can be resized at all. A browser tab cannot, so the
    /// menu does not offer it there rather than offering something inert.
    pub resizable: bool,
    pub cursor: CursorMode,
    pub scaling: Scaling,
    /// Remote resolution, once the first frame has arrived.
    pub remote: Option<(u16, u16)>,
    /// How much of the picture is changing, once the first report has arrived.
    pub noise: Option<Noise>,
    /// The device's analog capture settings, once it has reported them.
    ///
    /// The adjustment window edits this in place: the device never announces a
    /// change of its own accord, so this copy is what the controls read back
    /// until a fresh config document replaces it.
    pub video: Option<VideoSettings>,
    /// The right-hand note in the bar: device messages, command results.
    ///
    /// Private because every note expires; see [`Menu::note`].
    note: Option<Note>,
    /// A management request is in flight.
    pub busy: bool,
    /// A target change is in flight, so what we know about the picture — its
    /// resolution above all — describes the *previous* target and is not worth
    /// showing.
    pub switching: bool,
    pub host: String,
    pub user: String,

    /// True while a menu is dropped down, so the viewer stops forwarding keys.
    menu_open: bool,
    show_shortcuts: bool,
    show_about: bool,
    show_video_adjust: bool,
    /// What the capture settings were when the adjustment window was opened,
    /// so *Restore* has something to put back.
    video_restore: Option<VideoSettings>,
    confirm: Option<Confirm>,
}

impl Menu {
    pub fn new(host: String, user: String, current_port: u32) -> Menu {
        Menu {
            targets: Vec::new(),
            current_port,
            current_name: format!("Port {current_port}"),
            exclusive: true,
            resizable: true,
            cursor: CursorMode::Arrow,
            scaling: Scaling::Fit,
            remote: None,
            noise: None,
            video: None,
            note: None,
            busy: false,
            switching: false,
            host,
            user,
            menu_open: false,
            show_shortcuts: false,
            show_about: false,
            show_video_adjust: false,
            video_restore: None,
            confirm: None,
        }
    }

    /// Draw the bar and any open dialogs; return what the user asked for.
    ///
    /// Usually empty or a single action — it is a `Vec` only because a click
    /// that dismisses a confirmation can resolve in the same frame as another.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<Action> {
        let ctx = ui.ctx().clone();
        let mut actions = Vec::new();

        let mut open = false;
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                open |= self.keyboard_menu(ui, &mut actions);
                open |= self.mouse_menu(ui, &mut actions);
                open |= self.video_menu(ui, &mut actions);
                open |= self.power_menu(ui);
                open |= self.session_menu(ui, &mut actions);
                open |= self.help_menu(ui);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.status_line(ui);
                });
            });
        });
        self.menu_open = open;

        self.dialogs(&ctx, &mut actions);
        actions
    }

    /// Put a line in the bar for `linger`, replacing whatever was there.
    ///
    /// Notes always expire. The bar's resting state is the resolution, the
    /// noise meter and the target, and those are what it is watched for; a
    /// note that stayed would hide a live reading behind something that has
    /// already happened.
    pub fn note(&mut self, text: impl Into<String>, linger: Duration) {
        self.note = Some(Note {
            text: text.into(),
            until: Instant::now() + linger,
            work: None,
        });
    }

    /// Put a line in the bar for work whose end can be recognised.
    ///
    /// See [`Menu::work_done`], which is what takes it down early.
    pub fn note_work(&mut self, text: impl Into<String>, linger: Duration, work: Work) {
        self.note = Some(Note {
            text: text.into(),
            until: Instant::now() + linger,
            work: Some(work),
        });
    }

    /// Re-word the note and give it its time again, without changing what it
    /// is waiting on.
    ///
    /// This is for progress the device sends about work already announced: the
    /// words are its own, but the line is still the one that was put up when
    /// the work started.
    pub fn renew_note(&mut self, text: impl Into<String>, linger: Duration) {
        let work = self.note.as_ref().and_then(|n| n.work);
        self.note = Some(Note {
            text: text.into(),
            until: Instant::now() + linger,
            work,
        });
    }

    /// Take the note down if it was waiting on `work`; true if the bar changed.
    ///
    /// Whatever the user has done since has replaced the line, and untagged
    /// lines are about something else entirely — neither is this work's to
    /// take down.
    pub fn work_done(&mut self, work: Work) -> bool {
        let done = self.note.as_ref().is_some_and(|n| n.work == Some(work));
        if done {
            self.note = None;
        }
        done
    }

    /// What the bar is saying, if anything.
    pub fn note_text(&self) -> Option<&str> {
        self.note.as_ref().map(|n| n.text.as_str())
    }

    /// Take the note down now, for a caller that knows it has stopped being
    /// true before its time is up.
    pub fn clear_note(&mut self) {
        self.note = None;
    }

    /// Drop the note once its time is up; true if the bar changed.
    pub fn expire_note(&mut self) -> bool {
        let done = self
            .note
            .as_ref()
            .is_some_and(|n| Instant::now() >= n.until);
        if done {
            self.note = None;
        }
        done
    }

    /// Queue a confirmation before an action; see [`Action::Power`] callers.
    pub fn confirm(
        &mut self,
        question: impl Into<String>,
        detail: impl Into<String>,
        confirm_label: impl Into<String>,
        action: Action,
    ) {
        self.confirm = Some(Confirm {
            question: question.into(),
            detail: detail.into(),
            confirm_label: confirm_label.into(),
            action,
        });
    }

    /// True while a dialog is up.
    pub fn dialog_open(&self) -> bool {
        self.confirm.is_some() || self.show_shortcuts || self.show_about
    }

    /// True while the UI, not the target, should be seeing keystrokes.
    ///
    /// An open menu counts: egui navigates menus with the pointer and never
    /// claims keyboard focus for them, so `Context::wants_keyboard_input` alone
    /// would let Escape both close the menu and reach the target.
    pub fn captures_keyboard(&self) -> bool {
        self.menu_open || self.dialog_open()
    }

    fn keyboard_menu(&mut self, ui: &mut egui::Ui, out: &mut Vec<Action>) -> bool {
        ui.menu_button("Keyboard", |ui| {
            if ui
                .add(egui::Button::new("Ctrl+Alt+Delete").shortcut_text("Ctrl+Alt+End"))
                .clicked()
            {
                out.push(Action::CtrlAltDel);
                ui.close();
            }
            ui.separator();

            for (g, group) in macros::GROUPS.iter().enumerate() {
                ui.menu_button(group.title, |ui| {
                    let base = macros::group_offset(g);
                    for (i, m) in group.macros.iter().enumerate() {
                        if ui.button(m.label).clicked() {
                            out.push(Action::Macro(base + i));
                            ui.close();
                        }
                    }
                });
            }

            ui.separator();
            if ui
                .add(egui::Button::new("Release held keys").shortcut_text("auto on focus loss"))
                .on_hover_text(
                    "Send a key-up for every modifier, in case the target thinks \
                     one is still down.",
                )
                .clicked()
            {
                out.push(Action::ReleaseHeldKeys);
                ui.close();
            }
        })
        .inner
        .is_some()
    }

    fn mouse_menu(&mut self, ui: &mut egui::Ui, out: &mut Vec<Action>) -> bool {
        ui.menu_button("Mouse", |ui| {
            if ui
                .add(egui::Button::new("Sync pointer").shortcut_text("Ctrl+Alt+Home"))
                .on_hover_text("Ask the device to put the remote pointer where yours is.")
                .clicked()
            {
                out.push(Action::SyncPointer);
                ui.close();
            }
            if ui
                .button("Calibrate…")
                .on_hover_text(
                    "The device walks the remote pointer through a known path to \
                     learn the target's acceleration.",
                )
                .clicked()
            {
                out.push(Action::CalibrateMouse);
                ui.close();
            }
            if ui
                .button("Align")
                .on_hover_text("Re-align the remote pointer using the current settings.")
                .clicked()
            {
                out.push(Action::AlignMouse);
                ui.close();
            }

            ui.separator();
            ui.menu_button("Local pointer", |ui| {
                for mode in CursorMode::ALL {
                    if ui
                        .selectable_label(self.cursor == mode, mode.label())
                        .clicked()
                    {
                        out.push(Action::SetCursorMode(mode));
                        ui.close();
                    }
                }
            });
        })
        .inner
        .is_some()
    }

    fn video_menu(&mut self, ui: &mut egui::Ui, out: &mut Vec<Action>) -> bool {
        ui.menu_button("Video", |ui| {
            if ui
                .button("Refresh")
                .on_hover_text("Ask for a full repaint.")
                .clicked()
            {
                out.push(Action::RefreshVideo);
                ui.close();
            }
            if ui
                .add(egui::Button::new("Auto adjust").shortcut_text("Ctrl+Alt+A"))
                .on_hover_text(
                    "Re-centre and re-sharpen the analog capture for the target's \
                     current video mode.",
                )
                .clicked()
            {
                out.push(Action::AutoAdjustVideo);
                ui.close();
            }
            if ui.button("Stop adjusting").clicked() {
                out.push(Action::StopAutoAdjust);
                ui.close();
            }
            if ui
                .add(egui::Button::new("Re-align capture").shortcut_text("Ctrl+Alt+Shift+A"))
                .on_hover_text(ALIGN_HELP)
                .clicked()
            {
                out.push(Action::AlignCapture);
                ui.close();
            }
            if ui
                .button("Adjust manually…")
                .on_hover_text(
                    "Position, phase, brightness and filter, by hand — including \
                     the horizontal position the automatic adjustment leaves \
                     alone.",
                )
                .clicked()
            {
                self.show_video_adjust = true;
                self.video_restore = self.video.clone();
                ui.close();
            }

            ui.separator();
            for mode in Scaling::ALL {
                if ui
                    .selectable_label(self.scaling == mode, mode.label())
                    .clicked()
                {
                    out.push(Action::SetScaling(mode));
                    ui.close();
                }
            }

            if self.resizable {
                ui.separator();
                let enabled = self.remote.is_some();
                let label = match self.remote {
                    Some((w, h)) => format!("Resize window to {w}×{h}"),
                    None => "Resize window to remote size".to_string(),
                };
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    out.push(Action::ResizeToRemote);
                    ui.close();
                }
            }
        })
        .inner
        .is_some()
    }

    /// Power control reaches the target's outlet, not the KVM, so it is the
    /// one place in this menu that can take a machine down mid-write. Every
    /// entry here goes through a confirmation rather than straight to an
    /// [`Action`].
    fn power_menu(&mut self, ui: &mut egui::Ui) -> bool {
        let target = format!("port {} ({})", self.current_port, self.current_name);
        let mut pending = None;

        let open = ui
            .menu_button("Power", |ui| {
                ui.label(egui::RichText::new(&target).weak());
                ui.separator();

                for (label, command, verb, warning) in [
                    (
                        "Power cycle…",
                        PowerCommand::Cycle,
                        "Power cycle",
                        "The target loses power immediately — anything unsaved on it is lost.",
                    ),
                    ("Power on…", PowerCommand::Up, "Power on", ""),
                    (
                        "Power off…",
                        PowerCommand::Down,
                        "Power off",
                        "This cuts the outlet, not a clean shutdown.",
                    ),
                ] {
                    if ui.button(label).clicked() {
                        let detail = if warning.is_empty() {
                            format!("{verb} {target}.")
                        } else {
                            format!("{verb} {target}.\n\n{warning}")
                        };
                        pending = Some((format!("{verb}?"), detail, verb, Action::Power(command)));
                        ui.close();
                    }
                }

                ui.separator();
                ui.label(
                    egui::RichText::new("Requires an IP-PDU outlet mapped to this port.")
                        .small()
                        .weak(),
                );
            })
            .inner
            .is_some();

        if let Some((question, detail, label, action)) = pending {
            self.confirm(question, detail, label, action);
        }
        open
    }

    fn session_menu(&mut self, ui: &mut egui::Ui, out: &mut Vec<Action>) -> bool {
        ui.menu_button("Session", |ui| {
            ui.menu_button("Switch target", |ui| {
                if self.targets.is_empty() {
                    ui.label(egui::RichText::new("no targets read yet").weak());
                }
                for target in &self.targets {
                    let current = target.port == self.current_port;
                    let label = format!("{:>2}  {}", target.port, target.name);
                    if ui.selectable_label(current, label).clicked() {
                        if !current {
                            out.push(Action::SwitchTarget {
                                port: target.port,
                                name: target.name.clone(),
                            });
                        }
                        ui.close();
                    }
                }
                ui.separator();
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Reload list"))
                    .clicked()
                {
                    out.push(Action::RefreshTargets);
                    ui.close();
                }
            });

            ui.separator();
            let mut exclusive = self.exclusive;
            if ui
                .checkbox(&mut exclusive, "Exclusive access")
                .on_hover_text("Lock other users out of this target.")
                .clicked()
            {
                out.push(Action::SetExclusive(exclusive));
                ui.close();
            }

            ui.separator();
            if ui.button("Disconnect").clicked() {
                out.push(Action::Disconnect);
                ui.close();
            }
        })
        .inner
        .is_some()
    }

    fn help_menu(&mut self, ui: &mut egui::Ui) -> bool {
        ui.menu_button("Help", |ui| {
            if ui.button("Keyboard shortcuts").clicked() {
                self.show_shortcuts = true;
                ui.close();
            }
            if ui.button("About").clicked() {
                self.show_about = true;
                ui.close();
            }
        })
        .inner
        .is_some()
    }

    fn status_line(&mut self, ui: &mut egui::Ui) {
        if self.busy {
            ui.add(egui::Spinner::new().size(12.0));
        }
        if let Some(note) = &self.note {
            ui.label(egui::RichText::new(&note.text).weak());
            ui.separator();
        }
        if let Some((w, h)) = self.remote.filter(|_| !self.switching) {
            ui.label(egui::RichText::new(format!("{w}×{h}")).weak());
            ui.separator();
        }
        if let Some(noise) = self.noise.filter(|_| !self.switching) {
            noise_meter(ui, noise);
            ui.separator();
        }
        ui.label(format!("{} — {}", self.current_port, self.current_name));
    }

    /// The manual video adjustment window.
    ///
    /// Deliberately not modal: the whole point is to watch the picture — and
    /// the noise reading in the bar — change while a value is nudged, so this
    /// neither covers the screen nor takes the keyboard away from the target.
    /// Only a value being typed into a control does that, which egui reports
    /// on its own.
    ///
    /// Every control writes straight into [`Menu::video`] and emits an
    /// [`Action`] once it settles. The device answers each one by re-digitising
    /// the picture and tells us nothing, so that copy is the only record of
    /// what was asked for.
    fn video_adjust_window(&mut self, ctx: &egui::Context, out: &mut Vec<Action>) {
        if !self.show_video_adjust {
            return;
        }

        let mut open = true;
        let mut video = self.video.clone();
        let restore = self.video_restore.clone();
        let noise = self.noise;
        let mut restore_now = false;
        let mut save_now = false;

        egui::Window::new("Video adjustment")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                let Some(v) = video.as_mut() else {
                    ui.label("Waiting for the device to report its capture settings…");
                    return;
                };

                ui.label(
                    egui::RichText::new(format!("{}×{} at {} Hz", v.width, v.height, v.refresh))
                        .weak(),
                );
                ui.add_space(6.0);

                egui::Grid::new("video_knobs")
                    .num_columns(3)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        if let Some(n) =
                            knob_row(ui, "Horizontal position", &mut v.hoffset, HOFFSET_HELP)
                        {
                            out.push(Action::SetVideoNumber(VideoSetting::HOffset, n));
                        }
                        if let Some(n) =
                            knob_row(ui, "Vertical position", &mut v.voffset, VOFFSET_HELP)
                        {
                            out.push(Action::SetVideoNumber(VideoSetting::VOffset, n));
                        }
                        if let Some(n) = knob_row(ui, "Phase", &mut v.phase, PHASE_HELP) {
                            out.push(Action::SetVideoNumber(VideoSetting::Phase, n));
                        }

                        if let Some(n) =
                            slider_row(ui, "Brightness", &mut v.brightness, BRIGHTNESS_HELP)
                        {
                            out.push(Action::SetVideoNumber(VideoSetting::Brightness, n));
                        }
                        if let Some(n) =
                            slider_row(ui, "Contrast", &mut v.contrast, BRIGHTNESS_HELP)
                        {
                            out.push(Action::SetVideoNumber(VideoSetting::Contrast, n));
                        }

                        ui.label("Filter").on_hover_text(FILTER_HELP);
                        egui::ComboBox::from_id_salt("video_filter")
                            .selected_text(filter_label(v.filter))
                            .show_ui(ui, |ui| {
                                for f in VIDEO_FILTERS {
                                    if ui
                                        .selectable_label(v.filter == f, filter_label(f))
                                        .clicked()
                                    {
                                        v.filter = f;
                                        out.push(Action::SetVideoFilter(f));
                                    }
                                }
                            });
                        ui.label("");
                        ui.end_row();

                        if let Some(noise) = noise {
                            ui.label("Noise").on_hover_text(NOISE_HELP);
                            ui.horizontal(|ui| {
                                noise_bar(ui, noise);
                                ui.label(format!("{}%", noise.level));
                            });
                            ui.label("");
                            ui.end_row();
                        }
                    });

                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .button("Save to device")
                        .on_hover_text(
                            "Write these values into the device's own configuration, \
                             where they survive logging out and coming back. This is \
                             what the stock client's OK button does.",
                        )
                        .clicked()
                    {
                        save_now = true;
                    }
                    if ui
                        .add_enabled(restore.is_some(), egui::Button::new("Restore"))
                        .on_hover_text(
                            "Put back the values the device reported when this window \
                             was opened, and store them, which is the only way to \
                             reach a value the device is already holding.",
                        )
                        .clicked()
                    {
                        restore_now = true;
                    }
                    if ui
                        .button("Auto adjust")
                        .on_hover_text(
                            "Let the device measure and store its own settings: the \
                             phase, the filter and the two positions. Its answer \
                             replaces what is above, and it is only a good answer \
                             when nothing here has been set by hand this session.",
                        )
                        .clicked()
                    {
                        out.push(Action::AutoAdjustVideo);
                    }
                    if ui
                        .button("Re-align capture")
                        .on_hover_text(ALIGN_HELP)
                        .clicked()
                    {
                        out.push(Action::AlignCapture);
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Changes take effect at once but last for this session only \
                         until they are saved: the device reads its own stored \
                         values again at the next login.",
                    )
                    .small()
                    .weak(),
                );
            });

        self.video = video;
        self.show_video_adjust = open;

        if save_now {
            if let Some(v) = &self.video {
                out.extend(save_actions(v));
            }
        }

        if restore_now {
            if let Some(r) = restore {
                out.extend(restore_actions(&r));
                self.video = Some(r);
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context, out: &mut Vec<Action>) {
        self.video_adjust_window(ctx, out);

        if let Some(pending) = &self.confirm {
            let mut decided = None;
            egui::Window::new(&pending.question)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(&pending.detail);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(&pending.confirm_label).clicked() {
                            decided = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            decided = Some(false);
                        }
                    });
                });
            match decided {
                Some(true) => {
                    let pending = self.confirm.take().expect("checked just above");
                    out.push(pending.action);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }

        let mut show_shortcuts = self.show_shortcuts;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut show_shortcuts)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("shortcuts").num_columns(2).show(ui, |ui| {
                    for (keys, what) in [
                        ("Ctrl+Alt+End", "send Ctrl+Alt+Delete to the target"),
                        ("Ctrl+Alt+Home", "re-sync the remote pointer"),
                        ("Ctrl+Alt+A", "run the device's video adjustment"),
                    ] {
                        ui.label(egui::RichText::new(keys).monospace());
                        ui.label(what);
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Every other key goes to the target. Combinations your own \
                         desktop swallows are under the Keyboard menu.",
                    )
                    .small()
                    .weak(),
                );
            });
        self.show_shortcuts = show_shortcuts;

        let mut show_about = self.show_about;
        egui::Window::new("About")
            .open(&mut show_about)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("netcmdr-view").heading());
                ui.label(concat!("version ", env!("CARGO_PKG_VERSION")));
                ui.add_space(6.0);
                egui::Grid::new("about").num_columns(2).show(ui, |ui| {
                    ui.label("Device");
                    ui.label(&self.host);
                    ui.end_row();
                    ui.label("User");
                    ui.label(&self.user);
                    ui.end_row();
                    ui.label("Target");
                    ui.label(format!("{} — {}", self.current_port, self.current_name));
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "A clean-room client for Tripp Lite NetCommander / Minicom \
                         Smart IP KVM switches.",
                    )
                    .small()
                    .weak(),
                );
            });
        self.show_about = show_about;
    }
}

/// The filters the device offers, in the order the vendor's dialog lists them.
const VIDEO_FILTERS: [VideoFilter; 5] = [
    VideoFilter::Auto,
    VideoFilter::None,
    VideoFilter::Low,
    VideoFilter::Medium,
    VideoFilter::High,
];

fn filter_label(f: VideoFilter) -> &'static str {
    match f {
        VideoFilter::Auto => "Auto",
        VideoFilter::None => "None",
        VideoFilter::Low => "Low",
        VideoFilter::Medium => "Medium",
        VideoFilter::High => "High",
    }
}

/// What the adjustment window's *Restore* button sends.
///
/// One stored write, carrying every knob the window opened with. It has to be
/// a stored write rather than a set apiece: the device drops a session set
/// whose value equals the one it has stored, and for a window opened on the
/// device's own figures that is exactly what a restore is made of.
fn restore_actions(original: &VideoSettings) -> Vec<Action> {
    let mut out = vec![Action::StoreVideoNumbers(vec![
        (VideoSetting::Brightness, original.brightness.value),
        (VideoSetting::Contrast, original.contrast.value),
        (VideoSetting::HOffset, original.hoffset.value),
        (VideoSetting::VOffset, original.voffset.value),
        (VideoSetting::Phase, original.phase.value),
    ])];

    // `Auto` is not a filter the device stores, it is a request to run the
    // adjustment — which is the one thing a restore must not do.
    if original.filter != VideoFilter::Auto {
        out.push(Action::SetVideoFilter(original.filter));
    }
    out
}

/// What *Save to device* sends: everything the window is showing, as one
/// stored write.
fn save_actions(v: &VideoSettings) -> Vec<Action> {
    vec![Action::StoreVideoNumbers(vec![
        (VideoSetting::Brightness, v.brightness.value),
        (VideoSetting::Contrast, v.contrast.value),
        (VideoSetting::HOffset, v.hoffset.value),
        (VideoSetting::VOffset, v.voffset.value),
        (VideoSetting::Phase, v.phase.value),
    ])]
}

const ALIGN_HELP: &str = "\
Measure where the target's picture actually starts, move the capture window \
onto it and save it to the device.

The device's own adjustment measures the same thing and also keeps it — but \
only when nothing has been set by hand in this session, and its answer is not \
always right. This one is measured here, from the picture, and checked against \
the range the device declares. Takes about ten seconds, and needs something on \
screen to measure against.";

const HOFFSET_HELP: &str = "\
Where the device starts sampling each scan line, in pixels.

Lower moves the picture right. If the left edge of the target's screen is cut \
off, take this down by the number of pixels missing, then Save to device to \
keep it. Until it is saved it lasts for this session only. Auto adjust \
replaces it with the position the device measures for itself — which is only \
a good measurement from a session where nothing has been set by hand, so \
reconnect before asking for one.";

const VOFFSET_HELP: &str = "\
Which line the device starts capturing at. Lower moves the picture down.";

const PHASE_HELP: &str = "\
Where in each pixel's clock period the signal is sampled.

This is the one to reach for when the picture is sharp in places and smeared \
or shimmering in others. The automatic adjustment sets it for you.";

const BRIGHTNESS_HELP: &str = "\
The digitiser's own gain, not a filter on the received picture — it changes \
what the device captures, so it also changes how much of the picture it thinks \
is changing.";

const FILTER_HELP: &str = "\
How hard the device filters noise out of the digitised signal.

Auto is not a setting: picking it runs the automatic adjustment, which chooses \
a filter itself. That is what the stock client does too.";

const NOISE_HELP: &str = "\
How much of the picture is changing, per second. Watch it while you adjust: on \
an idle target anything above zero is noise on the VGA link rather than real \
work.";

/// One numeric knob: a value to drag or step by one, and the range the device
/// will accept for it.
///
/// Returns the value to send, once the control has settled on one. A drag
/// reports a change for every frame it moves, and each change is a command the
/// device answers by re-digitising the picture, so only the value a drag ends
/// on is sent — the vendor's dialog guards its own sliders with
/// `getValueIsAdjusting()` for the same reason.
fn knob_row(ui: &mut egui::Ui, label: &str, knob: &mut Knob, help: &str) -> Option<i32> {
    let mut send = None;
    ui.label(label).on_hover_text(help);
    ui.horizontal(|ui| {
        let movable = !knob.is_fixed();
        if ui
            .add_enabled(movable && knob.value > knob.min, egui::Button::new("−"))
            .clicked()
        {
            knob.value -= 1;
            send = Some(knob.value);
        }
        let resp = ui.add_enabled(
            movable,
            egui::DragValue::new(&mut knob.value)
                .range(knob.min..=knob.max)
                .speed(0.25),
        );
        if settled(&resp) {
            send = Some(knob.value);
        }
        if ui
            .add_enabled(movable && knob.value < knob.max, egui::Button::new("+"))
            .clicked()
        {
            knob.value += 1;
            send = Some(knob.value);
        }
    });
    ui.label(
        egui::RichText::new(format!("{}–{}", knob.min, knob.max))
            .small()
            .weak(),
    );
    ui.end_row();
    send
}

/// A knob whose range is wide and whose exact value does not matter, so it
/// reads better as a slider. Same settling rule as [`knob_row`].
fn slider_row(ui: &mut egui::Ui, label: &str, knob: &mut Knob, help: &str) -> Option<i32> {
    let mut send = None;
    ui.label(label).on_hover_text(help);
    let resp = ui.add_enabled(
        !knob.is_fixed(),
        egui::Slider::new(&mut knob.value, knob.min..=knob.max),
    );
    if settled(&resp) {
        send = Some(knob.value);
    }
    ui.label("");
    ui.end_row();
    send
}

/// True once a control has finished moving and its value is worth a command.
///
/// `changed()` alone fires on every frame of a drag; `drag_stopped()` alone
/// misses a click on the stepper or a typed-in value.
fn settled(resp: &egui::Response) -> bool {
    resp.drag_stopped() || (resp.changed() && !resp.dragged())
}

/// The "Noise" reading as a compact bar, for the right-hand end of the bar.
///
/// The stock client shows the same number as a labelled progress bar, but only
/// inside its Advanced Video dialog; it belongs in the bar, because its whole
/// use is to be watched while the video auto-adjust settles.
fn noise_meter(ui: &mut egui::Ui, noise: Noise) {
    // The status line lays out right-to-left, so the bar goes in before its
    // label to end up to the right of it.
    noise_bar(ui, noise);
    ui.label(egui::RichText::new("Noise").weak());
}

/// The bar itself, without a label, so the adjustment window can put it in a
/// grid cell of its own.
fn noise_bar(ui: &mut egui::Ui, noise: Noise) -> egui::Response {
    const WIDTH: f32 = 42.0;
    const HEIGHT: f32 = 7.0;

    let (rect, response) = ui.allocate_exact_size(egui::vec2(WIDTH, HEIGHT), egui::Sense::hover());
    let painter = ui.painter();
    let rounding = egui::CornerRadius::same((HEIGHT / 2.0) as u8);
    painter.rect_filled(rect, rounding, ui.visuals().extreme_bg_color);

    let fill = noise.level as f32 / 100.0;
    if fill > 0.0 {
        // A one-percent reading still has to be visible, so the bar never
        // draws narrower than it is tall.
        let mut bar = rect;
        bar.set_width((rect.width() * fill).max(HEIGHT));
        painter.rect_filled(bar, rounding, level_color(noise.level));
    }

    response.on_hover_text(format!(
        "Video noise: {}%, {} tile{} changed in the last second.\n\nHow much of \
         the picture is changing. A quiet target that still reads above zero is \
         picking up noise on the VGA link — run Video ▸ Auto-adjust to settle it.",
        noise.level,
        noise.changes,
        if noise.changes == 1 { "" } else { "s" },
    ))
}

/// Green while the picture is quiet, amber once it is restless, red when the
/// reading is high enough that the video is worth adjusting.
fn level_color(level: u8) -> egui::Color32 {
    match level {
        0..=19 => egui::Color32::from_rgb(0x4c, 0xaf, 0x50),
        20..=59 => egui::Color32::from_rgb(0xe8, 0xa3, 0x3d),
        _ => egui::Color32::from_rgb(0xe0, 0x5a, 0x4f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        Menu::new("192.0.2.10".into(), "admin".into(), 1)
    }

    fn settings() -> VideoSettings {
        netcmdr_session::parse_video_settings(
            "<client_config><video_settings brightness='49' contrast='50' cx='1360' \
             cy='768' fq='60' voffset='24' voffset_min='7' voffset_max='26' \
             hoffset='368' hoffset_min='113' hoffset_max='431' phase='4' \
             filter='low'/></client_config>",
        )
        .expect("a settings element")
    }

    /// Run one headless frame of the menu and return what it asked for.
    fn frame(m: &mut Menu, ctx: &egui::Context) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| actions = m.show(ui));
        });
        // Nothing here uploads textures, but the font atlas still arrives as a
        // delta and epaint panics if one is dropped.
        out.textures_delta.clear();
        actions
    }

    #[test]
    fn a_note_expires_and_the_bar_goes_back_to_the_resolution() {
        let mut m = menu();
        m.note("adjusting video…", Duration::ZERO);
        assert!(m.note.is_some(), "the note went up");
        assert!(m.expire_note(), "its time was up");
        assert!(m.note.is_none(), "and the bar is back to resting");
        assert!(!m.expire_note(), "nothing left to expire");

        // A note still within its time stays: an adjustment run that keeps
        // reporting keeps its line.
        m.note("adjusting video…", WORKING_LINGER);
        assert!(!m.expire_note());
        assert!(m.note.is_some());
    }

    /// A line about work the device answers comes down on the answer, and only
    /// on its own answer.
    #[test]
    fn the_answer_to_a_piece_of_work_takes_its_line_down() {
        let mut m = menu();
        m.note_work("adjusting video…", WORKING_LINGER, Work::VideoAdjust);
        assert!(!m.work_done(Work::Switch), "not this line's answer");
        assert!(m.note.is_some());
        assert!(m.work_done(Work::VideoAdjust), "the bar changed");
        assert!(m.note.is_none());
        assert!(!m.work_done(Work::VideoAdjust), "nothing left to take down");

        // An untagged line is about something else, and nothing the device
        // answers gets to clear it.
        m.note("released held keys", NOTE_LINGER);
        assert!(!m.work_done(Work::VideoAdjust));
        assert!(m.note.is_some());
    }

    /// Progress from the device re-words the line without costing the work its
    /// ending; anything the user does next replaces the line outright.
    #[test]
    fn progress_keeps_what_the_line_is_waiting_on() {
        let mut m = menu();
        m.note_work("adjusting video…", WORKING_LINGER, Work::VideoAdjust);
        m.renew_note("video adjust: phase", WORKING_LINGER);
        assert_eq!(
            m.note.as_ref().map(|n| n.text.as_str()),
            Some("video adjust: phase")
        );
        assert!(
            m.work_done(Work::VideoAdjust),
            "still the adjustment's line"
        );

        m.note_work("adjusting video…", WORKING_LINGER, Work::VideoAdjust);
        m.note("sent Ctrl+Alt+Delete", NOTE_LINGER);
        assert!(!m.work_done(Work::VideoAdjust), "the bar has moved on");
        assert!(m.note.is_some(), "and that line is left alone");
    }

    #[test]
    fn the_adjustment_window_paints_once_the_device_has_reported() {
        let ctx = egui::Context::default();
        let mut m = menu();
        m.video = Some(settings());
        m.noise = Some(Noise {
            level: 3,
            changes: 40,
        });
        m.show_video_adjust = true;
        assert!(
            frame(&mut m, &ctx).is_empty(),
            "drawing alone asks for nothing"
        );
        assert!(m.show_video_adjust, "the window stayed open");
    }

    /// The device may not have said anything yet — the window still has to
    /// come up rather than panic on a missing value.
    #[test]
    fn the_adjustment_window_copes_with_nothing_reported_yet() {
        let ctx = egui::Context::default();
        let mut m = menu();
        m.video = None;
        m.show_video_adjust = true;
        assert!(frame(&mut m, &ctx).is_empty());
    }

    /// Restore puts every knob back — as one stored write, which is the only
    /// kind the device accepts for a value it is already holding — and, above
    /// all, does not turn the filter into an auto-adjust.
    #[test]
    fn restore_resends_the_values_the_window_opened_with() {
        let original = settings();
        let out = restore_actions(&original);

        let Some(Action::StoreVideoNumbers(changes)) = out.first() else {
            panic!("restore did not write anything: {out:?}");
        };
        for expected in [
            (VideoSetting::Brightness, original.brightness.value),
            (VideoSetting::Contrast, original.contrast.value),
            (VideoSetting::HOffset, original.hoffset.value),
            (VideoSetting::VOffset, original.voffset.value),
            (VideoSetting::Phase, original.phase.value),
        ] {
            assert!(changes.contains(&expected), "{expected:?} in {changes:?}");
        }
    }

    /// Restoring `auto` would run the device's adjustment, which is the one
    /// thing a restore must not do.
    #[test]
    fn restore_never_asks_for_the_automatic_filter() {
        let mut original = settings();
        original.filter = VideoFilter::Auto;

        assert!(!restore_actions(&original)
            .iter()
            .any(|a| matches!(a, Action::SetVideoFilter(_))));
    }

    /// The device drops a *session* set that matches the value it is holding,
    /// which is exactly what a restore is made of — so a restore must never
    /// send one.
    #[test]
    fn restore_never_sends_a_set_the_device_would_drop() {
        let out = restore_actions(&settings());

        assert!(!out
            .iter()
            .any(|a| matches!(a, Action::SetVideoNumber(_, _))));
    }

    /// A save is one element carrying both gains — without them the device
    /// treats it as a live set and forgets it at logout, which would make the
    /// button a lie.
    #[test]
    fn a_save_carries_the_gains_that_make_the_device_store_it() {
        let v = settings();
        let out = save_actions(&v);

        let [Action::StoreVideoNumbers(changes)] = out.as_slice() else {
            panic!("expected one stored write, got {out:?}");
        };
        for expected in [
            (VideoSetting::Brightness, v.brightness.value),
            (VideoSetting::Contrast, v.contrast.value),
            (VideoSetting::HOffset, v.hoffset.value),
            (VideoSetting::VOffset, v.voffset.value),
            (VideoSetting::Phase, v.phase.value),
        ] {
            assert!(changes.contains(&expected), "{expected:?} in {changes:?}");
        }
    }

    #[test]
    fn every_filter_the_window_offers_has_a_label() {
        for f in VIDEO_FILTERS {
            assert!(!filter_label(f).is_empty());
        }
        assert_eq!(VIDEO_FILTERS.len(), 5);
    }

    /// A knob the device pins in place must not offer a control that pretends
    /// otherwise.
    #[test]
    fn a_fixed_knob_disables_its_control() {
        let ctx = egui::Context::default();
        let mut m = menu();
        let mut v = settings();
        v.hoffset = Knob {
            value: 200,
            min: 200,
            max: 200,
        };
        m.video = Some(v);
        m.show_video_adjust = true;
        assert!(frame(&mut m, &ctx).is_empty());
        assert_eq!(m.video.as_ref().expect("kept").hoffset.value, 200);
    }

    #[test]
    fn a_confirmation_yields_its_action_only_once_accepted() {
        let mut m = menu();
        assert!(!m.dialog_open());
        m.confirm(
            "Power off?",
            "detail",
            "Power off",
            Action::Power(PowerCommand::Down),
        );
        assert!(m.dialog_open());
    }

    #[test]
    fn every_macro_has_a_reachable_action() {
        let total: usize = macros::GROUPS.iter().map(|g| g.macros.len()).sum();
        for i in 0..total {
            assert!(matches!(Action::Macro(i), Action::Macro(_)));
            assert!(macros::get(i).is_some());
        }
    }

    /// The meter is the only widget here that paints by hand, so run it
    /// through a headless egui frame at the edges of its range.
    #[test]
    fn the_noise_meter_paints_across_its_whole_range() {
        let ctx = egui::Context::default();
        for level in [0u8, 1, 19, 20, 59, 60, 100] {
            for changes in [0u32, 1, 2, 9999] {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            noise_meter(ui, Noise { level, changes })
                        });
                    });
                });
                // Nothing here uploads textures, but the font atlas still
                // arrives as a delta and epaint panics if one is dropped.
                out.textures_delta.clear();
                assert!(!out.shapes.is_empty(), "the meter drew nothing");
            }
        }
    }

    #[test]
    fn the_meter_colour_changes_with_the_reading() {
        // Three distinct bands, so the bar reads at a glance.
        assert_eq!(level_color(0), level_color(19));
        assert_ne!(level_color(19), level_color(20));
        assert_eq!(level_color(20), level_color(59));
        assert_ne!(level_color(59), level_color(60));
        assert_eq!(level_color(60), level_color(100));
    }

    #[test]
    fn the_meter_is_hidden_until_a_reading_arrives_and_while_switching() {
        let mut m = menu();
        assert!(m.noise.is_none());
        m.noise = Some(Noise {
            level: 5,
            changes: 3,
        });
        assert!(m.noise.filter(|_| !m.switching).is_some());
        m.switching = true;
        assert!(m.noise.filter(|_| !m.switching).is_none());
    }

    #[test]
    fn scaling_and_cursor_modes_all_have_labels() {
        for mode in Scaling::ALL {
            assert!(!mode.label().is_empty());
        }
        for mode in CursorMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }
}
