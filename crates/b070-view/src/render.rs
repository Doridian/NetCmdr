// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! OpenGL context, the egui painter, and the remote screen as a texture.
//!
//! The session decodes into a plain `u32` framebuffer; everything on screen is
//! drawn by egui, so the framebuffer is uploaded as an egui texture and the
//! video is just an image the menu is drawn over.
//!
//! The upload is incremental. A 1280×1024 screen is five megabytes as RGBA, and
//! the device sends a message per changed tile, so re-uploading the whole thing
//! for every message would cost far more than decoding it. `Event::Frame`
//! carries the rectangle that changed, [`Screen::mark`] accumulates those into
//! one bounding box, and [`Screen::upload`] sends only that box.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use b070_session::Rect;
use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

/// The window, its GL context and the egui painter.
pub struct Renderer {
    pub window: Arc<Window>,
    painter: egui_glow::Painter,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    /// The size the GL surface was last told about.
    ///
    /// The surface does not follow the window on its own, and the window does
    /// not always announce a change: `request_inner_size` may apply the new
    /// size *synchronously*, and winit then deliberately emits no `Resized`
    /// event. Waiting for one leaves the surface at the old size while the
    /// compositor scales that stale buffer up to the new window — the picture
    /// comes out zoomed and stretched. So the size is checked before every
    /// paint rather than trusted to arrive as an event.
    surface_size: Cell<(u32, u32)>,
}

impl Renderer {
    /// Create the window together with a GL context for it.
    ///
    /// The two cannot be built independently: the window has to be made with a
    /// pixel format the driver will give us a context for, which is why glutin
    /// creates it rather than winit directly.
    pub fn new(event_loop: &ActiveEventLoop, attrs: WindowAttributes) -> Result<Renderer> {
        let template = ConfigTemplateBuilder::new().with_alpha_size(0);
        let (window, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(event_loop, template, |configs| {
                // Plain colour, no multisampling: every pixel we draw is either
                // a captured screen pixel or egui's own antialiased geometry.
                configs
                    .reduce(|best, config| {
                        if config.num_samples() < best.num_samples() {
                            config
                        } else {
                            best
                        }
                    })
                    .expect("the driver offered no GL configuration")
            })
            .map_err(|e| anyhow!("no usable OpenGL configuration: {e}"))?;
        let window = window.ok_or_else(|| anyhow!("glutin did not create a window"))?;

        let gl_display = gl_config.display();
        let raw_window_handle = {
            use raw_window_handle::HasWindowHandle as _;
            window.window_handle()?.as_raw()
        };
        let context_attributes = ContextAttributesBuilder::new().build(Some(raw_window_handle));
        let not_current = unsafe { gl_display.create_context(&gl_config, &context_attributes) }
            .context("creating the OpenGL context")?;

        let surface_attributes = window
            .build_surface_attributes(Default::default())
            .context("describing the window surface")?;
        let surface = unsafe { gl_display.create_window_surface(&gl_config, &surface_attributes) }
            .context("creating the window surface")?;
        let context = not_current
            .make_current(&surface)
            .context("making the OpenGL context current")?;

        // Vsync: the device pushes tiles far faster than a monitor refreshes,
        // and without this we would burn a core redrawing frames nobody sees.
        if let Err(e) = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::MIN)) {
            tracing::debug!(error = %e, "vsync unavailable");
        }

        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|s| gl_display.get_proc_address(s).cast())
        };
        let painter = egui_glow::Painter::new(Arc::new(gl), "", None, false)
            .map_err(|e| anyhow!("creating the egui painter: {e}"))?;

        let size = window.inner_size();
        Ok(Renderer {
            window: Arc::new(window),
            painter,
            surface,
            context,
            surface_size: Cell::new((size.width, size.height)),
        })
    }

    /// Match the GL surface to the window, if it has drifted.
    pub fn resize(&self) {
        let size = self.window.inner_size();
        if size.width == 0
            || size.height == 0
            || self.surface_size.get() == (size.width, size.height)
        {
            return;
        }
        self.window.resize_surface(&self.surface, &self.context);
        self.surface_size.set((size.width, size.height));
        tracing::debug!(width = size.width, height = size.height, "surface resized");
    }

    /// Paint one egui frame and present it.
    pub fn paint(&mut self, ctx: &egui::Context, output: egui::FullOutput) {
        let size = self.window.inner_size();
        let (Some(_), Some(_)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // minimised
        };
        self.resize();
        let pixels_per_point = ctx.pixels_per_point();
        let primitives = ctx.tessellate(output.shapes, pixels_per_point);

        self.painter.clear([size.width, size.height], [0.0; 4]);
        let mut textures_delta = output.textures_delta;
        self.painter.paint_and_update_textures(
            [size.width, size.height],
            pixels_per_point,
            &primitives,
            &mut textures_delta,
        );

        if let Err(e) = self.surface.swap_buffers(&self.context) {
            tracing::warn!(error = %e, "swap failed");
        }
    }

    pub fn destroy(&mut self) {
        self.painter.destroy();
    }
}

/// The remote screen, held as an egui texture.
pub struct Screen {
    texture: Option<egui::TextureHandle>,
    size: (u16, u16),
    /// Union of the rectangles changed since the last upload.
    dirty: Option<Rect>,
    /// Scratch buffer for the converted pixels, so an upload allocates nothing.
    scratch: Vec<egui::Color32>,
}

impl Screen {
    pub fn new() -> Screen {
        Screen {
            texture: None,
            size: (0, 0),
            dirty: None,
            scratch: Vec::new(),
        }
    }

    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture.as_ref().map(|t| t.id())
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Note that `rect` changed; uploads are deferred to the next [`upload`].
    ///
    /// [`upload`]: Screen::upload
    pub fn mark(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        self.dirty = Some(match self.dirty {
            None => rect,
            Some(d) => Rect {
                left: d.left.min(rect.left),
                top: d.top.min(rect.top),
                right: d.right.max(rect.right),
                bottom: d.bottom.max(rect.bottom),
            },
        });
    }

    /// Throw the picture away.
    ///
    /// Used when the session is about to show a *different* machine: the
    /// device keeps sending the old target's tiles for several seconds after a
    /// port change, so the safe thing is to have nothing to draw at all until
    /// it re-syncs. The next [`Screen::upload`] rebuilds the texture whole.
    pub fn blank(&mut self) {
        self.texture = None;
        self.dirty = None;
    }

    /// Mark the whole screen, for a resolution change or a repaint.
    pub fn mark_all(&mut self) {
        self.dirty = Some(Rect {
            left: 0,
            top: 0,
            right: self.size.0,
            bottom: self.size.1,
        });
    }

    /// Push whatever changed into the texture. Returns true if anything moved.
    pub fn upload(&mut self, ctx: &egui::Context, frame: &b070_session::Framebuffer) -> bool {
        if frame.width == 0 || frame.height == 0 {
            return false;
        }

        // A resolution change invalidates the texture outright: egui textures
        // cannot be resized in place.
        if self.size != (frame.width, frame.height) {
            self.size = (frame.width, frame.height);
            self.texture = None;
            self.dirty = None;
        }

        let options = egui::TextureOptions {
            // Sharp when magnified — the source is a captured screen and
            // blurring it makes small text unreadable — but smoothed when the
            // window is smaller than the target's screen, where nearest
            // neighbour would drop whole rows of pixels.
            magnification: egui::TextureFilter::Nearest,
            minification: egui::TextureFilter::Linear,
            ..Default::default()
        };

        let Some(texture) = &mut self.texture else {
            self.scratch.clear();
            self.scratch.extend(frame.pixels.iter().copied().map(rgb));
            let image = egui::ColorImage::new(
                [frame.width as usize, frame.height as usize],
                std::mem::take(&mut self.scratch),
            );
            self.texture = Some(ctx.load_texture("remote-screen", image, options));
            self.dirty = None;
            return true;
        };

        let Some(rect) = self.dirty.take() else {
            return false;
        };
        let (w, h) = (frame.width as usize, frame.height as usize);
        let left = rect.left as usize;
        let top = rect.top as usize;
        let right = (rect.right as usize).min(w);
        let bottom = (rect.bottom as usize).min(h);
        if right <= left || bottom <= top {
            return false;
        }

        self.scratch.clear();
        self.scratch.reserve((right - left) * (bottom - top));
        for y in top..bottom {
            let row = y * w;
            self.scratch.extend(
                frame.pixels[row + left..row + right]
                    .iter()
                    .copied()
                    .map(rgb),
            );
        }
        let image = egui::ColorImage::new(
            [right - left, bottom - top],
            std::mem::take(&mut self.scratch),
        );
        texture.set_partial([left, top], image, options);
        true
    }
}

/// `0x00RRGGBB` as the decoder writes it, to an opaque egui colour.
fn rgb(px: u32) -> egui::Color32 {
    egui::Color32::from_rgb((px >> 16) as u8, (px >> 8) as u8, px as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn dirty_rectangles_accumulate_into_one_box() {
        let mut s = Screen::new();
        s.mark(rect(10, 10, 20, 20));
        s.mark(rect(100, 5, 110, 15));
        assert_eq!(s.dirty, Some(rect(10, 5, 110, 20)));
    }

    #[test]
    fn empty_rectangles_are_ignored() {
        let mut s = Screen::new();
        s.mark(rect(10, 10, 10, 20));
        s.mark(rect(10, 10, 20, 10));
        assert_eq!(s.dirty, None);
    }

    #[test]
    fn colours_keep_their_channel_order() {
        assert_eq!(
            rgb(0x00_FF_80_01),
            egui::Color32::from_rgb(0xFF, 0x80, 0x01)
        );
        assert_eq!(rgb(0), egui::Color32::BLACK);
    }
}
