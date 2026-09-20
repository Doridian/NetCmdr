// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg(target_arch = "wasm32")]
//! `netcmdr-web` — the KVM viewer, in a browser.
//!
//! The same viewer the desktop client runs: [`netcmdr_ui`] holds the menu, the
//! screen and the input routing, and this crate supplies only what a browser
//! does differently — WebSockets instead of sockets, `fetch` instead of a
//! worker thread, and raw DOM key events instead of winit's.
//!
//! None of it talks to the device directly. `netcmdr-bridge` serves this page and
//! relays for it, because the device's TLS is one no browser can negotiate.

mod connect;
mod fonts;
mod keyboard;
mod link;
mod net;
mod ws;

use std::cell::RefCell;
use std::rc::Rc;

use netcmdr_ui::menu::CursorMode;
use netcmdr_ui::{Host, Viewer};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::keyboard::Keyboard;
use crate::link::{WebMgmt, WebSession};

/// The page, as the shared viewer sees it.
#[derive(Clone)]
struct PageHost {
    canvas: web_sys::HtmlCanvasElement,
}

impl Host for PageHost {
    fn set_title(&self, title: &str) {
        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            document.set_title(title);
        }
    }

    // `request_size` is left at its default. A page cannot resize the window it
    // is in, and `Host::resizable` says so, which is what keeps the menu from
    // offering it.

    fn set_cursor(&self, over_video: bool, mode: CursorMode) {
        let cursor = if !over_video {
            "default"
        } else {
            match mode {
                CursorMode::Arrow => "default",
                CursorMode::Crosshair => "crosshair",
                CursorMode::Hidden => "none",
            }
        };
        let _ = self.canvas.style().set_property("cursor", cursor);
    }
}

/// What the credentials form has collected.
struct LoginForm {
    user: String,
    password: String,
    target: u32,
    exclusive: bool,
}

impl Default for LoginForm {
    fn default() -> Self {
        LoginForm {
            user: "admin".into(),
            password: String::new(),
            target: 1,
            exclusive: true,
        }
    }
}

enum Phase {
    Login,
    Connecting,
    Running(Box<Viewer<WebSession, WebMgmt>>),
    Failed(String),
}

type Arriving = Rc<RefCell<Option<Result<connect::Ready, String>>>>;

struct App {
    phase: Phase,
    /// Where the connect task leaves its result.
    arriving: Arriving,
    keyboard: Keyboard,
    host: PageHost,
    /// Remembered so a reconnect after a failure does not ask again.
    last: LoginForm,
    device: String,
}

impl App {
    fn begin_connect(&mut self, form: LoginForm) {
        let arriving = self.arriving.clone();
        let (user, password) = (form.user.clone(), form.password.clone());
        let (target, exclusive) = (form.target, form.exclusive);
        self.last = form;
        self.phase = Phase::Connecting;
        spawn_local(async move {
            let result = connect::connect(&user, &password, target, exclusive).await;
            *arriving.borrow_mut() = Some(result);
        });
    }
}

/// Hand the viewer everything the browser has collected since the last frame.
fn feed_input(
    ctx: &egui::Context,
    viewer: &mut Viewer<WebSession, WebMgmt>,
    keyboard: &Keyboard,
    host: &PageHost,
) {
    // The pointer comes through egui, which already knows where the canvas is
    // and what the device pixel ratio is.
    if let Some(pos) = ctx.pointer_latest_pos() {
        viewer.on_pointer_moved(pos);
    }
    ctx.input(|i| {
        for event in &i.raw.events {
            match event {
                egui::Event::PointerButton {
                    button, pressed, ..
                } => {
                    let button = match button {
                        egui::PointerButton::Primary => Some(netcmdr_session::MouseButton::Left),
                        egui::PointerButton::Middle => Some(netcmdr_session::MouseButton::Middle),
                        egui::PointerButton::Secondary => Some(netcmdr_session::MouseButton::Right),
                        _ => None,
                    };
                    if let Some(button) = button {
                        viewer.on_mouse_button(button, *pressed);
                    }
                }
                egui::Event::MouseWheel { delta, .. } => {
                    viewer.on_scroll((delta.y / 40.0).round() as i32)
                }
                egui::Event::WindowFocused(false) => viewer.on_focus_lost(),
                _ => {}
            }
        }
    });

    // Keys never come through egui: see `keyboard`.
    for key in keyboard.drain() {
        viewer.set_modifiers(key.shift, key.ctrl, key.alt);
        viewer.on_key(key.key, key.down, host);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Video arrives on its own schedule, so the page never idles.
        ctx.request_repaint();

        if matches!(self.phase, Phase::Connecting) {
            if let Some(result) = self.arriving.borrow_mut().take() {
                self.phase = match result {
                    Ok(ready) => {
                        let mgmt = WebMgmt::new(ready.session_id.clone());
                        Phase::Running(Box::new(Viewer::new(
                            WebSession::new(ready.core),
                            mgmt,
                            netcmdr_ui::viewer::Config {
                                host: self.device.clone(),
                                user: self.last.user.clone(),
                                target: self.last.target,
                                exclusive: self.last.exclusive,
                            },
                        )))
                    }
                    Err(reason) => Phase::Failed(reason),
                };
            }
        }

        let host = self.host.clone();
        let keyboard = self.keyboard.clone();
        let mut next: Option<Phase> = None;

        match &mut self.phase {
            Phase::Running(viewer) => {
                viewer.session_mut().pump();
                if viewer.poll(&host) {
                    feed_input(&ctx, viewer, &keyboard, &host);
                    keyboard.set_target_has_focus(viewer.keyboard_goes_to_target());
                    // eframe hands us the root Ui, which is exactly what the
                    // shared viewer expects — the same one it gets natively.
                    viewer.ui(ui, &host);
                    viewer.finish_frame(&host);
                } else {
                    // Returning here would skip the phase change below and
                    // leave a dead session on screen.
                    next = Some(Phase::Failed(
                        viewer
                            .end_reason()
                            .unwrap_or("the session ended")
                            .to_string(),
                    ));
                }
            }

            // Login, connecting and a failure are the same panel with
            // different words in it.
            _ => {
                let connecting = matches!(self.phase, Phase::Connecting);
                let failure = match &self.phase {
                    Phase::Failed(reason) => Some(reason.clone()),
                    _ => None,
                };
                let device = self.device.clone();
                let form = &mut self.last;
                let mut submit = false;

                egui::CentralPanel::default().show(ui, |ui| {
                    if connecting {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() / 3.0);
                            ui.add(egui::Spinner::new().size(28.0));
                            ui.add_space(12.0);
                            ui.label("Connecting…");
                        });
                    } else {
                        submit = login_form(ui, form, &device, failure.as_deref());
                    }
                });

                keyboard.set_target_has_focus(false);
                if submit {
                    let form = LoginForm {
                        user: self.last.user.clone(),
                        password: self.last.password.clone(),
                        target: self.last.target,
                        exclusive: self.last.exclusive,
                    };
                    self.begin_connect(form);
                }
            }
        }

        if let Some(phase) = next {
            self.phase = phase;
        }
    }
}

/// The credentials panel, also used to report a failure and offer another go.
fn login_form(
    ui: &mut egui::Ui,
    form: &mut LoginForm,
    device: &str,
    failure: Option<&str>,
) -> bool {
    let mut submit = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 6.0);
        ui.heading("NetCommander IP KVM");
        ui.label(egui::RichText::new(device).weak());
        ui.add_space(16.0);

        if let Some(reason) = failure {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 90), reason);
            ui.add_space(12.0);
        }

        // `vertical_centered` centres each widget by its own width, and a
        // grid's width is not known until it is laid out — so give it one.
        ui.allocate_ui_with_layout(
            egui::vec2(320.0, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Grid::new("login").num_columns(2).show(ui, |ui| {
                    ui.label("User");
                    ui.text_edit_singleline(&mut form.user);
                    ui.end_row();
                    ui.label("Password");
                    let password =
                        ui.add(egui::TextEdit::singleline(&mut form.password).password(true));
                    submit |=
                        password.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                    ui.label("Target port");
                    ui.add(egui::DragValue::new(&mut form.target).range(1..=64));
                    ui.end_row();
                });
            },
        );
        ui.add_space(8.0);
        ui.checkbox(&mut form.exclusive, "Exclusive access");
        ui.add_space(12.0);
        submit |= ui.button("Connect").clicked();
    });
    submit
}

/// Start the client on the page's `<canvas id="netcmdr">`.
#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    console_error_panic_hook();

    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let canvas: web_sys::HtmlCanvasElement = document
        .get_element_by_id("netcmdr")
        .ok_or_else(|| JsValue::from_str("no <canvas id=\"netcmdr\">"))?
        .dyn_into()?;

    // The bridge says which device it is pointed at, purely so the page can
    // name it; everything else goes through the bridge regardless.
    let device = fetch_device_name().await.unwrap_or_else(|| "KVM".into());

    let app = App {
        phase: Phase::Login,
        arriving: Rc::new(RefCell::new(None)),
        keyboard: Keyboard::attach(),
        host: PageHost {
            canvas: canvas.clone(),
        },
        last: LoginForm::default(),
        device,
    };

    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(|cc| {
                // `default_fonts` is off, so the context starts with no fonts
                // at all: see `fonts`.
                fonts::install(&cc.egui_ctx);
                Ok(Box::new(app))
            }),
        )
        .await
}

async fn fetch_device_name() -> Option<String> {
    let text = net::get_text("api/info").await.ok()?;
    // A whole JSON parser for one field would be a poor trade.
    let at = text.find("\"device\":\"")? + "\"device\":\"".len();
    let rest = &text[at..];
    Some(rest[..rest.find('"')?].to_string())
}

fn console_error_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            web_sys::console::error_1(&JsValue::from_str(&format!("{info}")));
        }));
    });
}
