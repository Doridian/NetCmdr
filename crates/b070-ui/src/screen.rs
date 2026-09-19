// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The remote screen as an egui texture.
//!
//! The session decodes into a plain `u32` framebuffer; everything on screen is
//! drawn by egui, so the framebuffer is uploaded as a texture and the video is
//! just an image the menu is drawn over.
//!
//! The upload is incremental. A 1280×1024 screen is five megabytes as RGBA, and
//! the device sends a message per changed tile, so re-uploading the whole thing
//! for every message would cost far more than decoding it. `Event::Frame`
//! carries the rectangle that changed, [`Screen::mark`] accumulates those into
//! one bounding box, and [`Screen::upload`] sends only that box.

use b070_session::Rect;

/// The remote screen, held as an egui texture.
pub struct Screen {
    texture: Option<egui::TextureHandle>,
    size: (u16, u16),
    /// Union of the rectangles changed since the last upload.
    dirty: Option<Rect>,
    /// Scratch buffer for the converted pixels, so an upload allocates nothing.
    scratch: Vec<egui::Color32>,
}

impl Default for Screen {
    fn default() -> Screen {
        Screen::new()
    }
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
