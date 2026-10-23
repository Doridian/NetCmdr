// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The native shell: window creation and `winit` event translation. egui sees
//! each event first; unconsumed ones go to the target.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use netcmdr_session::{MouseButton, Session};
use netcmdr_ui::menu::CursorMode;
use netcmdr_ui::{Host, Viewer};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{CursorIcon, Window, WindowId};

use crate::link::{NativeMgmt, NativeSession};
use crate::render::Renderer;

/// Event drain and repaint interval.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub struct Config {
    pub host: String,
    pub user: String,
    pub target: u32,
    pub exclusive: bool,
    /// For power control and the target list.
    pub client: Arc<netcmdr_mgmt::Client>,
}

pub fn run(session: Session, cfg: Config) -> Result<()> {
    let event_loop = EventLoop::new().context("creating the event loop")?;
    event_loop.set_control_flow(ControlFlow::wait_duration(FRAME_INTERVAL));

    let mgmt = NativeMgmt::spawn(cfg.client);
    let viewer = Viewer::new(
        NativeSession::new(session),
        mgmt,
        netcmdr_ui::viewer::Config {
            host: cfg.host,
            user: cfg.user,
            target: cfg.target,
            exclusive: cfg.exclusive,
        },
    );

    let mut app = App {
        viewer,
        egui_ctx: egui::Context::default(),
        egui_state: None,
        renderer: None,
        host: None,
        result: Ok(()),
    };
    netcmdr_ui::fonts::install(&app.egui_ctx);
    event_loop.run_app(&mut app).context("running the viewer")?;
    std::mem::replace(&mut app.result, Ok(()))
}

struct WindowHost {
    window: Arc<Window>,
    /// A resize was requested; winit may not emit `Resized`.
    resized: Cell<bool>,
}

impl Host for WindowHost {
    fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    fn request_size(&self, width: u32, height: u32) {
        let _ = self
            .window
            .request_inner_size(PhysicalSize::new(width, height));
        self.resized.set(true);
    }

    fn resizable(&self) -> bool {
        true
    }

    fn set_cursor(&self, over_video: bool, mode: CursorMode) {
        apply_cursor(&self.window, over_video, mode);
    }
}

struct App {
    viewer: Viewer<NativeSession, NativeMgmt>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    renderer: Option<Renderer>,
    host: Option<WindowHost>,
    result: Result<()>,
}

impl App {
    /// Window coordinates (physical) to egui points.
    fn to_points(&self, pos: PhysicalPosition<f64>) -> egui::Pos2 {
        let ppp = self.egui_ctx.pixels_per_point().max(f32::EPSILON);
        egui::pos2(pos.x as f32 / ppp, pos.y as f32 / ppp)
    }

    fn draw(&mut self) {
        let (Some(renderer), Some(state), Some(host)) =
            (&mut self.renderer, &mut self.egui_state, &self.host)
        else {
            return;
        };
        let window = renderer.window.clone();
        let raw_input = state.take_egui_input(&window);

        let viewer = &mut self.viewer;
        let output = self
            .egui_ctx
            .run_ui(raw_input, |ui| viewer.ui(ui, host as &dyn Host));

        state.handle_platform_output(&window, output.platform_output.clone());
        renderer.paint(&self.egui_ctx, output);
        self.viewer.finish_frame(host as &dyn Host);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(env!("CARGO_PKG_NAME"))
            .with_inner_size(PhysicalSize::new(1024u32, 768u32));

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
                let host = WindowHost {
                    window: renderer.window.clone(),
                    resized: Cell::new(false),
                };
                host.set_title(&self.viewer.title());
                self.host = Some(host);
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
        // egui first.
        let response = state.on_window_event(&window, &event);
        if response.repaint {
            self.viewer.mark_dirty();
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(r) = &self.renderer {
                    r.resize();
                }
                self.viewer.mark_dirty();
            }
            WindowEvent::ModifiersChanged(mods) => {
                let s = mods.state();
                self.viewer
                    .set_modifiers(s.shift_key(), s.control_key(), s.alt_key());
            }
            WindowEvent::CursorMoved { position, .. } => {
                if response.consumed {
                    return;
                }
                let point = self.to_points(position);
                self.viewer.on_pointer_moved(point);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if response.consumed {
                    return;
                }
                let button = match button {
                    winit::event::MouseButton::Left => Some(MouseButton::Left),
                    winit::event::MouseButton::Middle => Some(MouseButton::Middle),
                    winit::event::MouseButton::Right => Some(MouseButton::Right),
                    _ => None,
                };
                if let Some(button) = button {
                    self.viewer
                        .on_mouse_button(button, state == ElementState::Pressed);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if response.consumed {
                    return;
                }
                let notches = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0).round() as i32,
                };
                self.viewer.on_scroll(notches);
            }
            WindowEvent::Focused(false) => self.viewer.on_focus_lost(),
            WindowEvent::KeyboardInput { event, .. } => {
                if response.consumed || !self.viewer.keyboard_goes_to_target() {
                    return;
                }
                let winit::keyboard::PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(key) = netcmdr_ui::key::from_winit(code) else {
                    tracing::debug!(?code, "no scancode for key");
                    return;
                };
                let Some(host) = &self.host else { return };
                self.viewer
                    .on_key(key, event.state == ElementState::Pressed, host as &dyn Host);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(host) = &self.host else { return };
        if !self.viewer.poll(host as &dyn Host) {
            event_loop.exit();
            return;
        }
        // A requested size may apply without a `Resized` event.
        if host.resized.replace(false) {
            if let Some(r) = &self.renderer {
                r.resize();
            }
        }
        if self.viewer.needs_repaint() {
            if let Some(r) = &self.renderer {
                r.window.request_redraw();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(r) = &mut self.renderer {
            r.destroy();
        }
    }
}

/// Our pointer style over the video; egui's elsewhere.
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
