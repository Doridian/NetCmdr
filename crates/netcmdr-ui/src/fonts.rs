// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Subset fonts for egui. The stock faces are 1.3 MiB, so `default_fonts` is
//! off and these are cut down by `tools/subset-fonts.py`.
//!
//! Every host has to call [`install`]: without it egui has no fonts and draws
//! each glyph as a square.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontTweak};

/// Ubuntu Light, cut to ASCII, Latin-1 and the punctuation the viewer uses.
/// Licence: `fonts/UFL.txt`.
const UBUNTU_LIGHT: &[u8] = include_bytes!("../fonts/Ubuntu-Light-subset.ttf");

/// Hack, same coverage, for anything asking for [`FontFamily::Monospace`].
/// Licence: `fonts/Hack-Regular.txt`.
const HACK_REGULAR: &[u8] = include_bytes!("../fonts/Hack-Regular-subset.ttf");

/// The four triangles and warning sign egui's widgets use.
/// Licence: `fonts/emoji-icon-font-mit-license.txt`.
const EMOJI_ICON: &[u8] = include_bytes!("../fonts/emoji-icon-subset.ttf");

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();

    fonts.font_data.insert(
        "Ubuntu-Light".to_owned(),
        Arc::new(FontData::from_static(UBUNTU_LIGHT)),
    );
    fonts.font_data.insert(
        "Hack".to_owned(),
        Arc::new(FontData::from_static(HACK_REGULAR)),
    );
    fonts.font_data.insert(
        "emoji-icon-font".to_owned(),
        Arc::new(FontData::from_static(EMOJI_ICON).tweak(FontTweak {
            // Upstream's tweak, so the submenu arrow matches desktop.
            scale: 0.90,
            ..Default::default()
        })),
    );

    // Icons last in both. Hack backs up Ubuntu for the thin space DragValue
    // uses.
    fonts.families.insert(
        FontFamily::Proportional,
        vec![
            "Ubuntu-Light".to_owned(),
            "Hack".to_owned(),
            "emoji-icon-font".to_owned(),
        ],
    );
    fonts.families.insert(
        FontFamily::Monospace,
        vec![
            "Hack".to_owned(),
            "Ubuntu-Light".to_owned(),
            "emoji-icon-font".to_owned(),
        ],
    );

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without the fonts egui draws every glyph as a square, on native and
    /// web alike. The viewer's own punctuation and icons must be covered.
    #[test]
    fn install_covers_the_viewers_glyphs() {
        let ctx = egui::Context::default();
        install(&ctx);
        let text = "Connecting… — ⏵ ⏷ ⚠ − € ‘’“” é";
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {
            ctx.fonts_mut(|fonts| {
                assert!(fonts.has_glyphs(&egui::FontId::proportional(14.0), text));
                assert!(fonts.has_glyphs(&egui::FontId::monospace(14.0), text));
            });
        });
        // epaint panics if the font atlas delta is dropped.
        out.textures_delta.clear();
    }
}
