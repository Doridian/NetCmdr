// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The window, its OpenGL context and the egui painter. Content is drawn by
//! `netcmdr_ui`.

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

pub struct Renderer {
    pub window: Arc<Window>,
    painter: egui_glow::Painter,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    /// Last size given to the GL surface. Checked before every paint, since
    /// `request_inner_size` may apply synchronously without a `Resized` event,
    /// leaving a stretched stale buffer.
    surface_size: Cell<(u32, u32)>,
}

impl Renderer {
    /// Create the window and GL context together (via glutin, so the pixel
    /// format matches).
    pub fn new(event_loop: &ActiveEventLoop, attrs: WindowAttributes) -> Result<Renderer> {
        let template = ConfigTemplateBuilder::new().with_alpha_size(0);
        let (window, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(event_loop, template, |configs| {
                // No multisampling needed.
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

        // Vsync, or tile updates would burn a core.
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
