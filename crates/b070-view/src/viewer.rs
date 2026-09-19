//! The window: draws the framebuffer and forwards keyboard and mouse.
//!
//! The image is scaled to fit the window with the aspect ratio preserved, and
//! pointer positions are mapped back through that same transform, because the
//! device wants **absolute coordinates in remote screen pixels**.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use b070_session::{Event, Input, Modifiers, MouseButton, Session};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::keys::scancode;

/// Redraw at most this often; the device sends tile updates far faster than a
/// screen refresh.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub fn run(session: Session) -> Result<()> {
    let event_loop = EventLoop::new().context("creating the event loop")?;
    event_loop.set_control_flow(ControlFlow::wait_duration(FRAME_INTERVAL));
    let mut app = App::new(session);
    event_loop.run_app(&mut app).context("running the viewer")?;
    app.result
}

struct App {
    session: Session,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    /// Remote resolution, once the first frame has arrived.
    remote: Option<(u16, u16)>,
    /// Scale and offset currently used to draw, for mapping the pointer back.
    transform: Transform,
    modifiers: Modifiers,
    dirty: bool,
    result: Result<()>,
}

#[derive(Debug, Clone, Copy)]
struct Transform {
    scale: f64,
    offset_x: f64,
    offset_y: f64,
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

impl App {
    fn new(session: Session) -> Self {
        App {
            session,
            window: None,
            surface: None,
            remote: None,
            transform: Transform::default(),
            modifiers: Modifiers::default(),
            dirty: false,
            result: Ok(()),
        }
    }

    fn send(&mut self, input: Input) {
        if let Err(e) = self.session.send_input(input) {
            tracing::warn!(error = %e, "input dropped");
        }
    }

    /// Drain session events, returning false when the session has ended.
    fn pump_events(&mut self) -> bool {
        loop {
            match self.session.events.try_recv() {
                Ok(Event::Frame(_)) => self.dirty = true,
                Ok(Event::Resolution { width, height }) => {
                    tracing::info!(width, height, "remote resolution");
                    self.remote = Some((width, height));
                    if let Some(window) = &self.window {
                        let _ = window
                            .request_inner_size(PhysicalSize::new(width as u32, height as u32));
                    }
                    self.dirty = true;
                }
                Ok(Event::NoSignal) => {
                    tracing::warn!("no video signal from the target");
                    self.dirty = true;
                }
                Ok(Event::Gui(msg)) if !msg.is_empty() => tracing::info!(%msg, "device message"),
                Ok(Event::Gui(_)) | Ok(Event::ConfigXml(_)) => {}
                Ok(Event::Disconnected(reason)) => {
                    tracing::info!(%reason, "session ended");
                    return false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(win_w), Some(win_h)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if let Err(e) = surface.resize(win_w, win_h) {
            tracing::warn!(error = %e, "surface resize failed");
            return;
        }

        let frame = match self.session.frame().lock() {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "no draw buffer");
                return;
            }
        };
        buffer.fill(0);

        if frame.width > 0 && frame.height > 0 {
            let (fw, fh) = (frame.width as f64, frame.height as f64);
            let scale = (size.width as f64 / fw).min(size.height as f64 / fh);
            let draw_w = (fw * scale).round() as u32;
            let draw_h = (fh * scale).round() as u32;
            let off_x = (size.width - draw_w.min(size.width)) / 2;
            let off_y = (size.height - draw_h.min(size.height)) / 2;
            self.transform = Transform {
                scale,
                offset_x: off_x as f64,
                offset_y: off_y as f64,
            };

            // Nearest-neighbour: the source is a captured screen, so sharp
            // pixels read better than a blurred resample.
            for y in 0..draw_h.min(size.height.saturating_sub(off_y)) {
                let src_y = ((y as f64 / scale) as usize).min(frame.height as usize - 1);
                let src_row = src_y * frame.width as usize;
                let dst_row = (y + off_y) as usize * size.width as usize + off_x as usize;
                for x in 0..draw_w.min(size.width.saturating_sub(off_x)) {
                    let src_x = ((x as f64 / scale) as usize).min(frame.width as usize - 1);
                    buffer[dst_row + x as usize] = frame.pixels[src_row + src_x];
                }
            }
        }

        if let Err(e) = buffer.present() {
            tracing::warn!(error = %e, "present failed");
        }
        self.dirty = false;
    }

    /// Window position to remote pixel.
    fn to_remote(&self, pos: LogicalPosition<f64>) -> Option<(i32, i32)> {
        let (w, h) = self.remote?;
        let x = (pos.x - self.transform.offset_x) / self.transform.scale;
        let y = (pos.y - self.transform.offset_y) / self.transform.scale;
        Some((
            (x.round() as i32).clamp(0, w as i32 - 1),
            (y.round() as i32).clamp(0, h as i32 - 1),
        ))
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let (w, h) = self.remote.unwrap_or((800, 600));
        let attrs = Window::default_attributes()
            .with_title("b070-view")
            .with_inner_size(PhysicalSize::new(w as u32, h as u32));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.result = Err(anyhow::anyhow!("creating the window: {e}"));
                event_loop.exit();
                return;
            }
        };
        match softbuffer::Context::new(window.clone())
            .and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()))
        {
            Ok(surface) => self.surface = Some(surface),
            Err(e) => {
                self.result = Err(anyhow::anyhow!("creating the draw surface: {e}"));
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(mods) => {
                let state = mods.state();
                self.modifiers.shift = state.shift_key();
                self.modifiers.ctrl = state.control_key();
                self.modifiers.alt = state.alt_key();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
                if let Some((x, y)) = self.to_remote(position.to_logical(1.0 / scale.max(0.0001))) {
                    self.send(Input::MouseMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
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
                let notches = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0).round() as i32,
                };
                if notches != 0 {
                    self.send(Input::MouseWheel(notches));
                }
            }
            WindowEvent::Focused(false) => {
                // Release anything the target might think is still held.
                for key in [KeyCode::ControlLeft, KeyCode::AltLeft, KeyCode::ShiftLeft] {
                    if let Some((scancode, extended)) = scancode(key) {
                        self.send(Input::Key {
                            scancode,
                            extended,
                            down: false,
                            modifiers: Modifiers::default(),
                        });
                    }
                }
                self.modifiers = Modifiers::default();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let down = event.state == ElementState::Pressed;

                // Local shortcuts, not forwarded: the target can never see
                // Ctrl+Alt+Delete otherwise, and the pointer needs a resync
                // whenever the target's own acceleration drifts from ours.
                if self.modifiers.ctrl && self.modifiers.alt && down {
                    match code {
                        KeyCode::End => {
                            self.send(Input::CtrlAltDel);
                            return;
                        }
                        KeyCode::Home => {
                            self.send(Input::MouseQuickSync);
                            return;
                        }
                        KeyCode::KeyA => {
                            if let Err(e) = self.session.auto_adjust_video() {
                                tracing::warn!(error = %e, "auto adjust failed");
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                // Lock keys are tracked locally: the platform does not tell us
                // the LED state, and the device only uses these as a hint.
                if down {
                    match code {
                        KeyCode::CapsLock => self.modifiers.caps_lock = !self.modifiers.caps_lock,
                        KeyCode::NumLock => self.modifiers.num_lock = !self.modifiers.num_lock,
                        KeyCode::ScrollLock => {
                            self.modifiers.scroll_lock = !self.modifiers.scroll_lock
                        }
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
            WindowEvent::Resized(_) => self.dirty = true,
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if !self.pump_events() {
            event_loop.exit();
            return;
        }
        if self.dirty {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_INTERVAL));
    }
}
