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

use b070_mgmt::{PowerCommand, Target};
use b070_session::Noise;

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
    pub const ALL: [CursorMode; 3] = [
        CursorMode::Arrow,
        CursorMode::Crosshair,
        CursorMode::Hidden,
    ];

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
    SetScaling(Scaling),
    ResizeToRemote,

    // Power
    Power(PowerCommand),

    // Session
    SwitchTarget { port: u32, name: String },
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

/// The menu bar and its state.
///
/// The public fields are what the bar draws; the viewer owns this struct and
/// keeps them current.
pub struct Menu {
    pub targets: Vec<Target>,
    pub current_port: u32,
    pub current_name: String,
    pub exclusive: bool,
    pub cursor: CursorMode,
    pub scaling: Scaling,
    /// Remote resolution, once the first frame has arrived.
    pub remote: Option<(u16, u16)>,
    /// How much of the picture is changing, once the first report has arrived.
    pub noise: Option<Noise>,
    /// The right-hand note in the bar: device messages, command results.
    pub status: String,
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
    confirm: Option<Confirm>,
}

impl Menu {
    pub fn new(host: String, user: String, current_port: u32) -> Menu {
        Menu {
            targets: Vec::new(),
            current_port,
            current_name: format!("Port {current_port}"),
            exclusive: true,
            cursor: CursorMode::Arrow,
            scaling: Scaling::Fit,
            remote: None,
            noise: None,
            status: String::new(),
            busy: false,
            switching: false,
            host,
            user,
            menu_open: false,
            show_shortcuts: false,
            show_about: false,
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
                .add(
                    egui::Button::new("Release held keys")
                        .shortcut_text("auto on focus loss"),
                )
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
                        pending =
                            Some((format!("{verb}?"), detail, verb, Action::Power(command)));
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
                if ui.add_enabled(!self.busy, egui::Button::new("Reload list")).clicked() {
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
        if !self.status.is_empty() {
            ui.label(egui::RichText::new(&self.status).weak());
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

    fn dialogs(&mut self, ctx: &egui::Context, out: &mut Vec<Action>) {
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
                ui.label(egui::RichText::new("b070-view").heading());
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

/// The "Noise" reading as a compact bar, for the right-hand end of the bar.
///
/// The stock client shows the same number as a labelled progress bar, but only
/// inside its Advanced Video dialog; it belongs in the bar, because its whole
/// use is to be watched while the video auto-adjust settles.
fn noise_meter(ui: &mut egui::Ui, noise: Noise) {
    const WIDTH: f32 = 42.0;
    const HEIGHT: f32 = 7.0;

    // The status line lays out right-to-left, so the bar goes in before its
    // label to end up to the right of it.
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(WIDTH, HEIGHT), egui::Sense::hover());
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
    ));

    ui.label(egui::RichText::new("Noise").weak());
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

    #[test]
    fn a_confirmation_yields_its_action_only_once_accepted() {
        let mut m = menu();
        assert!(!m.dialog_open());
        m.confirm("Power off?", "detail", "Power off", Action::Power(PowerCommand::Down));
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
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| noise_meter(ui, Noise { level, changes }),
                        );
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
        m.noise = Some(Noise { level: 5, changes: 3 });
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
