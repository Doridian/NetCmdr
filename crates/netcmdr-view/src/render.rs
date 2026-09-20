// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The window, its OpenGL context and the egui painter.
//!
//! This is the native half of the viewer's rendering: everything that needs a
//! windowing system. What is drawn, including the remote screen's texture, is
//! `netcmdr_ui`'s and is shared with the web client.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
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
