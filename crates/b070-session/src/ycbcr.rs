//! YCbCr → RGB conversion for the video stream.
//!
//! Transcribed from `com.minicom.plugin.ycbcr.YCbCrToRGB` in the current
//! firmware's client.
//!
//! The frame grabber packs each 32-bit word as two luma samples plus a shared
//! chroma pair. The low bit of each luma byte (word bits 0 and 16) is stolen to
//! form a two-bit *selector* saying how the chroma is laid out — 0 for a
//! self-contained pair, 1 or 2 when one chroma half must be borrowed from a
//! later word in the same tile row ([`find_next_order`]), 3 for "skip". That
//! theft is why every sample is masked to 7 bits and doubled on the way in.
//!
//! The R and B outputs are deliberately crossed over relative to the maths (the
//! `R` buffer receives the blue-difference result and vice versa); the caller
//! crosses them back when writing pixels, which is how the vendor's framebuffer
//! ends up in `0x00RRGGBB` order. Both halves of that swap are reproduced here
//! so the pipeline stays byte-identical to the client's.

/// Input modes (`YCbCrToRGB.YCBCRMODE*`). Mode 0 (RGB 5:5:5) exists in the Java
/// but `VideoModel` never selects it, so it is not ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// `ALX_YCBCRMODE4_2_2`: two pixels per 32-bit word.
    #[default]
    Ycbcr422,
    /// `ALX_YCBCRMODE4_0_0`: one luma-only pixel per byte.
    Ycbcr400,
}

/// The converter, holding its output planes across frames so they are not
/// reallocated per message.
#[derive(Debug, Default)]
pub struct YCbCrToRgb {
    mode: InputMode,
    /// Tile width; `find_next_order` scans at most half a tile row ahead.
    tile_cx: usize,
    r: Vec<u8>,
    g: Vec<u8>,
    b: Vec<u8>,
    color_buf_size: usize,
}

impl YCbCrToRgb {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_input_mode(&mut self, mode: InputMode) {
        self.mode = mode;
    }

    pub fn set_tile_size(&mut self, cx: usize, _cy: usize) {
        self.tile_cx = cx;
    }

    /// Number of valid pixels in the output planes.
    pub fn color_buf_size(&self) -> usize {
        self.color_buf_size
    }

    /// Blue-difference result — see the note on channel crossover above.
    pub fn r_plane(&self) -> &[u8] {
        &self.r
    }
    pub fn g_plane(&self) -> &[u8] {
        &self.g
    }
    /// Red-difference result — see the note on channel crossover above.
    pub fn b_plane(&self) -> &[u8] {
        &self.b
    }

    /// `Convert`. `len` bytes of `data` are consumed; `byte_swap` reverses each
    /// 32-bit group in place first (`Inplace_eShuttle2BSbuf`), which frames that
    /// came through the LZW expander have already had applied.
    pub fn convert(&mut self, data: &mut [u8], len: usize, byte_swap: bool) {
        let len = len.min(data.len());
        if byte_swap {
            inplace_word_swap(&mut data[..len]);
        }
        // `InitBuffers`: 4:2:2 packs two bytes per pixel, 4:0:0 one.
        self.color_buf_size = match self.mode {
            InputMode::Ycbcr422 => len * 8 / 16,
            InputMode::Ycbcr400 => len,
        };
        let size = self.color_buf_size;
        for plane in [&mut self.r, &mut self.g, &mut self.b] {
            if plane.len() < size {
                plane.resize(size, 0);
            }
        }
        match self.mode {
            InputMode::Ycbcr422 => self.convert_422(&data[..len]),
            InputMode::Ycbcr400 => self.convert_400(&data[..len]),
        }
    }

    fn convert_422(&mut self, data: &[u8]) {
        let mut ycc = [0i32; 3];
        let mut out = [0i32; 3];
        let mut o = 0usize;

        for i in 0..data.len() / 4 {
            let v = u32::from_le_bytes([
                data[i * 4],
                data[i * 4 + 1],
                data[i * 4 + 2],
                data[i * 4 + 3],
            ]);
            let selector = ((v >> 15) & 2) | (v & 1);
            match selector {
                0 => {
                    ycc[2] = ((v & 0xFF00_0000) >> 23) as i32;
                    ycc[1] = ((v & 0xFE00) >> 7) as i32;
                    ycc[0] = (v & 0xFE) as i32;
                    ycbcr_to_rgb(&ycc, &mut out);
                    self.put(o, &out);
                    o += 1;
                    ycc[0] = ((v & 0xFE_0000) >> 16) as i32;
                    ycbcr_to_rgb(&ycc, &mut out);
                    self.put(o, &out);
                    o += 1;
                }
                1 => {
                    // This word carries only the first chroma half; the second
                    // comes from the next word whose low bit is clear.
                    let saved = v;
                    let borrowed =
                        (v & 0xFE) | (find_next_order(self.tile_cx, i, data) & 0xFE00_FE00);
                    ycc[2] = ((borrowed & 0xFF00_0000) >> 23) as i32;
                    ycc[1] = ((borrowed & 0xFF00) >> 7) as i32;
                    ycc[0] = (borrowed & 0xFE) as i32;
                    ycbcr_to_rgb(&ycc, &mut out);
                    self.put(o, &out);
                    o += 1;
                    ycc[2] = ((saved & 0xFF00_0000) >> 23) as i32;
                    ycc[1] = ((saved & 0xFF00) >> 7) as i32;
                    ycc[0] = ((saved & 0xFE_0000) >> 16) as i32;
                    ycbcr_to_rgb(&ycc, &mut out);
                    self.put(o, &out);
                    o += 1;
                }
                2 => {
                    ycc[2] = ((v & 0xFF00_0000) >> 23) as i32;
                    ycc[1] = ((v & 0xFF00) >> 7) as i32;
                    ycc[0] = (v & 0xFE) as i32;
                    ycbcr_to_rgb(&ycc, &mut out);
                    self.put(o, &out);
                    o += 1;
                    let borrowed =
                        (v & 0xFE_0000) | (find_next_order(self.tile_cx, i, data) & 0xFE00_FE00);
                    ycc[2] = ((borrowed & 0xFF00_0000) >> 23) as i32;
                    ycc[1] = ((borrowed & 0xFF00) >> 7) as i32;
                    ycc[0] = ((borrowed & 0xFE_0000) >> 16) as i32;
                    ycbcr_to_rgb(&ycc, &mut out);
                    self.put(o, &out);
                    o += 1;
                }
                // Selector 3 emits nothing at all, not even a placeholder pixel.
                _ => {}
            }
        }
    }

    fn convert_400(&mut self, data: &[u8]) {
        let mut ycc = [0i32, 128, 128];
        let mut out = [0i32; 3];
        for i in 0..data.len() {
            if i % 2 == 0 {
                ycc[0] = data[i] as i32;
            } else {
                ycc[0] = ((data[i] as i32) << 1) & 0xFE;
                if ycc[0] == (data[i - 1] & 0xFE) as i32 {
                    ycc[0] = data[i - 1] as i32;
                }
            }
            ycbcr_to_rgb(&ycc, &mut out);
            self.put(i, &out);
        }
    }

    #[inline]
    fn put(&mut self, idx: usize, out: &[i32; 3]) {
        if idx >= self.color_buf_size {
            return;
        }
        // Crossed over exactly as the Java assigns them.
        self.r[idx] = out[2] as u8;
        self.g[idx] = out[1] as u8;
        self.b[idx] = out[0] as u8;
    }
}

/// `Inplace_eShuttle2BSbuf`: reverse every 4-byte group.
pub fn inplace_word_swap(data: &mut [u8]) {
    for chunk in data.as_chunks_mut::<4>().0 {
        chunk.swap(0, 3);
        chunk.swap(1, 2);
    }
}

/// `YCbCr2RGBcalc`. `ycc` is (Y, Cb, Cr); `out` receives (R, G, B) — which the
/// caller then stores crossed over.
fn ycbcr_to_rgb(ycc: &[i32; 3], out: &mut [i32; 3]) {
    let (y, cb, cr) = (ycc[0], ycc[1], ycc[2]);
    out[0] = ((298 * y + 408 * cr) >> 8) - 223;
    out[1] = ((298 * y - 100 * cb - 208 * cr) >> 8) + 135;
    out[2] = ((298 * y + 516 * cb) >> 8) - 277;
    for c in out.iter_mut() {
        *c = (*c).clamp(0, 255);
    }
}

/// `FindNextOrder`: scan forward for the word holding the borrowed chroma half.
///
/// The search walks at most half a tile row and stops at the first word whose
/// low bit is clear; running off the end of the buffer yields zero.
fn find_next_order(tile_cx: usize, idx: usize, data: &[u8]) -> u32 {
    let half = tile_cx / 2;
    if half == 0 {
        return 0;
    }
    let mut i = idx;
    loop {
        i += 1;
        let step = i % half + 1;
        if 4 * i + 3 >= data.len() {
            return 0;
        }
        if data[4 * i] & 1 == 0 || step >= half {
            break;
        }
    }
    u32::from_le_bytes([
        data[4 * i],
        data[4 * i + 1],
        data[4 * i + 2],
        data[4 * i + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coefficients_match_the_client() {
        // Mid grey: Y=128, neutral chroma.
        let mut out = [0i32; 3];
        ycbcr_to_rgb(&[128, 128, 128], &mut out);
        assert_eq!(out[0], ((298 * 128 + 408 * 128) >> 8) - 223);
        assert_eq!(out[1], ((298 * 128 - 100 * 128 - 208 * 128) >> 8) + 135);
        assert_eq!(out[2], ((298 * 128 + 516 * 128) >> 8) - 277);
    }

    #[test]
    fn results_are_clamped_to_a_byte() {
        let mut out = [0i32; 3];
        ycbcr_to_rgb(&[255, 255, 255], &mut out);
        assert!(out.iter().all(|c| (0..=255).contains(c)));
        // Black is luma 0 with *neutral* chroma, not zeroed chroma.
        ycbcr_to_rgb(&[0, 128, 128], &mut out);
        assert_eq!(out, [0, 0, 0]);
    }

    #[test]
    fn word_swap_reverses_each_group() {
        let mut d = [1u8, 2, 3, 4, 5, 6, 7, 8];
        inplace_word_swap(&mut d);
        assert_eq!(d, [4, 3, 2, 1, 8, 7, 6, 5]);
    }

    #[test]
    fn mode_400_is_grey() {
        let mut c = YCbCrToRgb::new();
        c.set_input_mode(InputMode::Ycbcr400);
        c.set_tile_size(8, 8);
        let mut data = vec![0u8, 64, 128, 200];
        c.convert(&mut data, 4, false);
        assert_eq!(c.color_buf_size(), 4);
        for i in 0..4 {
            // Neutral chroma: the three channels track each other.
            let (r, g, b) = (c.r_plane()[i], c.g_plane()[i], c.b_plane()[i]);
            assert!(
                r.abs_diff(g) <= 2 && g.abs_diff(b) <= 2,
                "pixel {i}: {r},{g},{b}"
            );
        }
    }

    #[test]
    fn mode_422_emits_two_pixels_per_word() {
        let mut c = YCbCrToRgb::new();
        c.set_input_mode(InputMode::Ycbcr422);
        c.set_tile_size(8, 8);
        // Selector 0: low bit of byte 0 clear and bit 15 clear.
        let mut data = vec![0x80u8, 0x40, 0x90, 0x40, 0x80, 0x40, 0x90, 0x40];
        c.convert(&mut data, 8, false);
        assert_eq!(c.color_buf_size(), 4, "8 bytes -> 4 pixels");
        // Both words are identical, so both pixel pairs must be too.
        assert_eq!(c.r_plane()[0], c.r_plane()[2]);
        assert_eq!(c.g_plane()[1], c.g_plane()[3]);
    }

    #[test]
    fn find_next_order_stops_at_the_buffer_end() {
        // Starting on the last word, the next step is already past the end.
        let data = vec![1u8; 16];
        assert_eq!(find_next_order(8, 3, &data), 0);
    }

    #[test]
    fn find_next_order_gives_up_after_half_a_tile_row() {
        // Every word's low bit is set, so the scan exhausts its step budget and
        // returns whatever it stopped on, exactly as the client does.
        let data = vec![1u8; 64];
        assert_eq!(find_next_order(8, 0, &data), 0x0101_0101);
    }

    #[test]
    fn find_next_order_takes_the_first_clear_word() {
        let mut data = vec![1u8; 32];
        data[12] = 0xF0; // word 3, low bit clear
        data[13] = 0x0F;
        data[14] = 0x00;
        data[15] = 0x00;
        assert_eq!(find_next_order(8, 2, &data), 0x0000_0FF0);
    }
}
