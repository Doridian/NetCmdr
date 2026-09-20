// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The fonts the page draws with.
//!
//! egui rasterises glyphs itself and paints them from its own texture atlas,
//! so it needs real font files — the fonts installed on the machine are not
//! reachable from WebAssembly, and the page's CSS reaches nothing egui draws.
//!
//! What it does not need is egui's four stock faces whole: those are 1.3 MiB,
//! close to a quarter of the module, and the viewer draws ASCII, a dash, an
//! ellipsis and a submenu arrow. So `default_fonts` is off in `Cargo.toml` and
//! the faces here are cut down to what can actually appear — see
//! `tools/subset-fonts.py`, which regenerates them and copies the licences.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontTweak};

/// Ubuntu Light, cut to ASCII, Latin-1 and the punctuation the viewer uses.
/// Licence: `fonts/UFL.txt`.
const UBUNTU_LIGHT: &[u8] = include_bytes!("../fonts/Ubuntu-Light-subset.ttf");

/// Hack, same coverage, for anything asking for [`FontFamily::Monospace`].
/// Licence: `fonts/Hack-Regular.txt`.
const HACK_REGULAR: &[u8] = include_bytes!("../fonts/Hack-Regular-subset.ttf");

/// Five glyphs from emoji-icon-font: the four triangles and the warning sign.
/// egui's own widgets draw these, and no text face carries them.
/// Licence: `fonts/emoji-icon-font-mit-license.txt`.
const EMOJI_ICON: &[u8] = include_bytes!("../fonts/emoji-icon-subset.ttf");

/// Install them, replacing the empty set a build without `default_fonts` has.
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
            // Upstream's tweak, kept so the submenu arrow sits as it does in
            // the desktop window rather than towering over the menu text.
            scale: 0.90,
            ..Default::default()
        })),
    );

    // Both families end in the icon face, so a glyph missing from the text
    // faces still lands somewhere. Hack backs up Ubuntu because Ubuntu has no
    // thin space, which is what egui's DragValue puts between sign and digits.
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
