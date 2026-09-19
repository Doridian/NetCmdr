//! Video frame decoding: DIB parsing, decompression, colour conversion and the
//! tile blit into the framebuffer.
//!
//! Transcribed from `com.minicom.plugin.video_impl.{VideoPayloadReader,
//! VideoServiceImpl,VideoModel}` and `com.minicom.plugin.data.DibSection`
//! (`research/decompiled/app-live/`).
//!
//! # How a frame arrives
//!
//! Video is **tile-based**, not full-frame. Each `CmdFlag::Bmp` message carries
//!
//! * a **bitfield** with one bit per tile of the screen, marking which tiles
//!   changed, and
//! * the pixel data for exactly those tiles, stacked vertically as if they were
//!   one narrow image `tile_cx` wide.
//!
//! Decoding runs in four stages, in this order (`VideoServiceImpl.onBmp`):
//!
//! 1. [`VideoDecoder::on_bmp`] notices resolution changes and sizes the
//!    framebuffer.
//! 2. *restore* — decompress the bitfield, then the pixel payload, into the
//!    scratch buffer. Both zlib and LZW modes are **streaming**: state carries
//!    from frame to frame until an `_INIT`/`_START` method restarts it. The zlib
//!    modes share one inflate stream between the bitfield and the pixels, so the
//!    two must be inflated in that order or the stream desynchronises.
//! 3. *post-uncompress* — palette lookup, RGB555 expansion or YCbCr conversion
//!    into 32-bit pixels.
//! 4. *unpack* — blit each decoded tile to its place in the framebuffer and
//!    report the rectangle that changed.
//!
//! # Pixel format
//!
//! The framebuffer holds one `u32` per pixel as `0x00RRGGBB`. That is what the
//! vendor's client ends up with too: it stores bytes B, G, R, x and then reads
//! them back with a little-endian `getInt` into a `TYPE_INT_RGB` image.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::{Decompress, FlushDecompress};

use crate::lzw::{Expander, DEF_LZW_CORDER};
use crate::ycbcr::{InputMode, YCbCrToRgb};
use crate::Error;

/// Compression methods (`AlxCompress` ordinals).
mod compress {
    pub const UNCOMPRESSED: i16 = 0;
    pub const ZIP_STREAM: i16 = 4;
    pub const ZIP_STREAM_INIT: i16 = 5;
    pub const ZIP_STREAM_BF: i16 = 6;
    pub const ZIP_STREAM_INIT_BF: i16 = 7;
    pub const LZW_TCB_UNC_START: i16 = 8;
    pub const LZW_TCB_UNC_CONT: i16 = 9;
    pub const LZW_TCB_COMP_START: i16 = 10;
    pub const LZW_TCB_COMP_CONT: i16 = 11;
}

/// Pixel formats (`AlxBmpFormat` codes).
mod format {
    pub const PACK_4: i16 = 3;
    pub const PACK_8: i16 = 4;
    pub const PACK_16: i16 = 5;
    pub const YCBCR_MODE: i16 = 7;
    pub const YCBCR_4_2_2: i16 = 8;
    pub const YCBCR_4_0_0: i16 = 10;
}

/// Largest frame the client will accept (`VideoModel.MAX_HRES/MAX_VRES`).
pub const MAX_WIDTH: usize = 2560;
pub const MAX_HEIGHT: usize = 1600;
/// Size of each scratch buffer, matching `KVMDIBBuffer(2560, 1600, 4)`.
const SCRATCH_BYTES: usize = MAX_WIDTH * MAX_HEIGHT * 4;
/// `VideoModel.MAX_INDEX_ALLOCK` — the tile index table's fixed size.
const MAX_TILES: usize = 30000;
/// The tile bitfield is `MAX_TILES / 8` bytes; the reader rejects anything
/// larger (`m_wIdxSizeComp > 3750`).
const BITFIELD_BYTES: usize = 3750;

/// A rectangle of the framebuffer, in pixels. `right`/`bottom` are exclusive.
///
/// The vendor packs these into an AWT `Rectangle`'s `width`/`height` fields
/// without converting, so the names there are misleading; these are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

/// The header of one video message (`DibSection`), parsed from the payload.
#[derive(Debug, Clone, Default)]
pub struct Dib {
    pub compress_method: i16,
    pub compressed_size: usize,
    pub alx_format: i16,
    pub tile_cx: u16,
    pub tile_cy: u16,
    pub source_cx: u16,
    pub source_cy: u16,
    /// Number of changed tiles this message describes.
    pub idx_size: u16,
    pub bitfield_comp: Vec<u8>,
    pub frame_count: i32,
}

impl Dib {
    /// Whether this format carries tile geometry, i.e. whether the fields after
    /// the pixel payload are present at all.
    fn is_tiled(alx_format: i16) -> bool {
        matches!(
            alx_format,
            format::PACK_4 | format::PACK_8 | format::PACK_16
        ) || alx_format >= format::YCBCR_MODE
    }
}

/// A little-endian or big-endian cursor over the message payload.
///
/// The stream announces its own byte order with a magic short — `GY`/`YG` —
/// which is what `VideoPayloadReader` tests for before reading anything else.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
    le: bool,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::ShortVideoPayload)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(Error::ShortVideoPayload)?;
        self.pos = end;
        Ok(slice)
    }

    /// A 16-bit read in the payload's declared byte order.
    fn i16(&mut self) -> Result<i16, Error> {
        let b = self.take(2)?;
        Ok(if self.le {
            i16::from_le_bytes([b[0], b[1]])
        } else {
            i16::from_be_bytes([b[0], b[1]])
        })
    }

    fn i32(&mut self) -> Result<i32, Error> {
        let b = self.take(4)?;
        Ok(if self.le {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            i32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }
}

/// Parse one `CmdFlag::Bmp` payload, copying the pixel bytes into `scratch`.
///
/// Mirrors `VideoPayloadReader.readDib`, including its sanity checks — those
/// bounds are what keep a corrupt frame from being turned into a huge
/// allocation or an out-of-range blit.
pub fn parse_dib(payload: &[u8], scratch: &mut [u8]) -> Result<Dib, Error> {
    let mut cur = Cursor {
        buf: payload,
        pos: 0,
        le: true,
    };

    // The byte-order marker is itself read little-endian: "YG" seen as 0x4759
    // means the rest is big-endian, "GY" (0x5947) means little-endian, and
    // anything else is already the compression method with no marker present.
    let first = cur.i16()?;
    let compress_method = match first {
        0x4759 => {
            cur.le = false;
            cur.i16()?
        }
        0x5947 => {
            cur.le = true;
            cur.i16()?
        }
        other => other,
    };
    let mut dib = Dib {
        compress_method,
        ..Dib::default()
    };

    let compressed_size = cur.i32()?;
    if compressed_size < 0 || compressed_size as usize > scratch.len() {
        return Err(Error::BadVideoField("compressed size out of range"));
    }
    dib.compressed_size = compressed_size as usize;
    dib.alx_format = cur.i16()?;

    let bi_header_size = cur.i32()?;
    if !(0..100).contains(&bi_header_size) {
        return Err(Error::BadVideoField("illegal biHeaderSize"));
    }
    if bi_header_size == 4 {
        dib.frame_count = cur.i32()?;
    } else {
        // A BITMAPINFOHEADER we have no use for; the geometry below supersedes it.
        cur.take(bi_header_size as usize)?;
    }

    scratch[..dib.compressed_size].copy_from_slice(cur.take(dib.compressed_size)?);

    if !Dib::is_tiled(dib.alx_format) {
        // The client never stores such a message, so there is nothing to draw.
        return Err(Error::UnsupportedFormat(dib.alx_format));
    }

    dib.tile_cx = cur.i16()? as u16;
    dib.tile_cy = cur.i16()? as u16;
    dib.source_cx = cur.i16()? as u16;
    dib.source_cy = cur.i16()? as u16;
    dib.idx_size = cur.i16()? as u16;
    let idx_size_comp = cur.i16()?;
    if idx_size_comp < 0 {
        return Err(Error::BadVideoField("negative compressed bitfield size"));
    }
    if idx_size_comp as usize > BITFIELD_BYTES {
        return Err(Error::BadVideoField("compressed bitfield too large"));
    }
    dib.bitfield_comp = cur.take(idx_size_comp as usize)?.to_vec();

    if dib.tile_cx == 0 || dib.tile_cy == 0 {
        return Err(Error::BadVideoField("zero tile size"));
    }
    let tiles_on_screen = (dib.source_cx as usize * dib.source_cy as usize)
        / (dib.tile_cx as usize * dib.tile_cy as usize);
    if dib.idx_size as usize > tiles_on_screen {
        return Err(Error::BadVideoField(
            "more changed tiles than the screen holds",
        ));
    }
    if dib.tile_cx > dib.source_cx || dib.tile_cy > dib.source_cy {
        return Err(Error::BadVideoField("tile larger than the screen"));
    }

    if cur.pos != payload.len() {
        tracing::debug!(
            consumed = cur.pos,
            payload = payload.len(),
            "video payload length disagrees with the header size"
        );
    }
    Ok(dib)
}

/// The decoded screen.
#[derive(Debug)]
pub struct Framebuffer {
    pub width: u16,
    pub height: u16,
    /// `width * height` pixels, each `0x00RRGGBB`.
    pub pixels: Vec<u32>,
}

/// A framebuffer shared between the decoder and whoever is drawing it.
pub type SharedFrame = Arc<Mutex<Framebuffer>>;

/// A palette handed over from the command channel, picked up by the decoder
/// before its next frame (`VideoServiceImpl.onPalette`).
pub type SharedPalette = Arc<Mutex<Option<[u32; 256]>>>;

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framebuffer {
    pub fn new() -> Self {
        Framebuffer {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(width as usize * height as usize, 0);
    }
}

/// How much of the picture is changing, as the stock client measures it.
///
/// This is the value behind the vendor viewer's "Noise / Level" bar, and it is
/// meant to be read as an analogue-noise reading: on a quiet desktop almost no
/// tiles change, so a steady non-zero level means the capture is picking up
/// noise on the VGA link rather than real activity. Turning the gain down —
/// with the device's video auto-adjust — is what settles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Noise {
    /// 0..=100. Percent of the screen changing per second, clamped, and
    /// rounded up to 1 whenever anything changed at all.
    pub level: u8,
    /// Tiles that changed over the reporting interval, unscaled.
    pub changes: u32,
}

/// Accumulates [`Noise`] over one-second intervals.
///
/// Transcribed from `KVMClientProxy::SetNoiseLevel(bool)` in the vendor's
/// native client library (`research/software/native/libkvmipjproxy.so`,
/// `0x440c0`), which is where the number is computed — the device sends only
/// the per-frame changed-tile count that feeds it. Per frame the vendor adds
/// that count as a percentage of the tiles on screen; once a second it divides
/// the running total by the elapsed seconds, so the reading is a *rate*: the
/// per-frame percentage multiplied by the frame rate, capped at 100.
#[derive(Debug)]
struct NoiseMeter {
    /// Running total of per-frame changed-tile percentages.
    sum_percent: u32,
    /// Running total of raw changed-tile counts.
    sum_changes: u32,
    last_report: Instant,
}

impl NoiseMeter {
    fn new() -> Self {
        NoiseMeter {
            sum_percent: 0,
            sum_changes: 0,
            last_report: Instant::now(),
        }
    }

    /// Fold one decoded frame in.
    fn record(&mut self, dib: &Dib) {
        // `parse_dib` has already rejected zero tile sizes, tiles bigger than
        // the screen and more changed tiles than the screen holds, so the
        // divisor is at least 1 and the percentage is at most 100. The vendor
        // guards all three here instead, having validated none of them.
        let tiles = (dib.source_cx as u32 * dib.source_cy as u32)
            / (dib.tile_cx as u32 * dib.tile_cy as u32);
        self.sum_percent += dib.idx_size as u32 * 100 / tiles.max(1);
        self.sum_changes += dib.idx_size as u32;
    }

    /// Report, if a whole second has passed since the last one.
    ///
    /// The vendor compares `time()` values, so its interval is whole seconds
    /// and the report lands on the first frame after the second turns over;
    /// this keeps that cadence but off the monotonic clock. The interval is
    /// advanced by the seconds it actually covered rather than reset to *now*,
    /// which is what the vendor's integer clock does implicitly and which keeps
    /// the sub-second remainder from being dropped every report. A gap of
    /// several seconds — a frozen or idle target — divides by that many, so it
    /// reads as quiet rather than as one large spike.
    fn take(&mut self) -> Option<Noise> {
        let secs = self.last_report.elapsed().as_secs();
        if secs == 0 {
            return None;
        }
        self.last_report += Duration::from_secs(secs);
        let changes = self.sum_changes;
        let rate = self.sum_percent as u64 / secs;
        self.sum_percent = 0;
        self.sum_changes = 0;
        // A screen that is changing, however little, should not read as silent.
        let level = match rate {
            0 if changes != 0 => 1,
            r => r.min(100) as u8,
        };
        Some(Noise { level, changes })
    }
}

/// Stateful video decoder. One instance decodes a whole session: the zlib and
/// LZW streams, the tile bitfield and the framebuffer all persist between
/// messages.
pub struct VideoDecoder {
    /// Receives the compressed payload, then the converted 32-bit pixels
    /// (`DibSection.m_dbCashBits`).
    cash: Vec<u8>,
    /// Decompressed pixel data (`VideoModel.m_dbCashUncompBits`).
    uncomp: Vec<u8>,
    bitfield: Vec<u8>,
    /// Tile indices for the current frame. Deliberately *not* cleared between
    /// frames — the vendor leaves stale entries past the ones it refills.
    indexes: Vec<u16>,
    lzw: Expander,
    ycbcr: YCbCrToRgb,
    inflate: Decompress,
    lut: [u32; 256],
    palette_in: SharedPalette,
    tile_cx: u16,
    tile_cy: u16,
    /// Cached copy of the shared frame's geometry, so the common path does not
    /// have to take the lock.
    width: u16,
    height: u16,
    resolution_changed: bool,
    noise: NoiseMeter,
    frame: SharedFrame,
}

impl VideoDecoder {
    /// Build a decoder that draws into `frame` and picks up palettes posted to
    /// `palette_in` by the command channel.
    pub fn new(frame: SharedFrame, palette_in: SharedPalette) -> Self {
        VideoDecoder {
            cash: vec![0; SCRATCH_BYTES],
            uncomp: vec![0; SCRATCH_BYTES],
            bitfield: vec![0; BITFIELD_BYTES],
            indexes: vec![0; MAX_TILES],
            lzw: Expander::new(DEF_LZW_CORDER),
            ycbcr: YCbCrToRgb::new(),
            inflate: Decompress::new(false),
            lut: [0; 256],
            palette_in,
            tile_cx: 0,
            tile_cy: 0,
            width: 0,
            height: 0,
            resolution_changed: false,
            noise: NoiseMeter::new(),
            frame,
        }
    }

    /// The framebuffer this decoder draws into.
    pub fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    /// Where to post a palette for the next frame to pick up.
    pub fn palette_slot(&self) -> &SharedPalette {
        &self.palette_in
    }

    /// Decode one video message into the framebuffer, returning the rectangle
    /// that changed. `payload` is the message body; the caller has already read
    /// it in full.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Rect, Error> {
        if let Ok(mut pending) = self.palette_in.try_lock() {
            if let Some(lut) = pending.take() {
                self.lut = lut;
            }
        }
        let dib = parse_dib(payload, &mut self.cash)?;
        tracing::debug!(
            compress = dib.compress_method,
            format = dib.alx_format,
            tiles = dib.idx_size,
            tile = format_args!("{}x{}", dib.tile_cx, dib.tile_cy),
            source = format_args!("{}x{}", dib.source_cx, dib.source_cy),
            bytes = dib.compressed_size,
            "video message"
        );
        self.noise.record(&dib);
        self.update_resolution(&dib);
        self.restore(&dib)?;
        self.post_uncompress(&dib)?;
        Ok(self.unpack_bitmap(&dib))
    }

    /// True when the last [`decode`](Self::decode) changed the frame geometry.
    /// Reading it clears the flag.
    pub fn take_resolution_change(&mut self) -> Option<(u16, u16)> {
        if std::mem::take(&mut self.resolution_changed) {
            Some((self.width, self.height))
        } else {
            None
        }
    }

    /// The [`Noise`] reading, once a second's worth of frames has gone by.
    /// Returns `None` in between, so the caller can poll it per frame.
    pub fn take_noise(&mut self) -> Option<Noise> {
        self.noise.take()
    }

    fn update_resolution(&mut self, dib: &Dib) {
        if self.width != dib.source_cx || self.height != dib.source_cy {
            // Note the vendor sets its reported tile height from the tile
            // *width* here; that bug only affects the `<resolution_change/>`
            // string it publishes, so we keep both dimensions honest.
            self.tile_cx = dib.tile_cx;
            self.tile_cy = dib.tile_cy;
            self.width = dib.source_cx;
            self.height = dib.source_cy;
            if let Ok(mut frame) = self.frame.lock() {
                frame.resize(dib.source_cx, dib.source_cy);
            }
            self.resolution_changed = true;
        }
    }

    /// Stage 2: decompress the bitfield and the pixel payload
    /// (`VideoModel.restoreEx`).
    fn restore(&mut self, dib: &Dib) -> Result<(), Error> {
        let n = dib.compressed_size;
        match dib.compress_method {
            compress::UNCOMPRESSED | compress::ZIP_STREAM_BF => {
                self.decompress_bitfield(dib)?;
                self.uncomp[..n].copy_from_slice(&self.cash[..n]);
            }
            compress::ZIP_STREAM_INIT_BF => {
                self.inflate = Decompress::new(false);
                self.decompress_bitfield(dib)?;
                self.uncomp[..n].copy_from_slice(&self.cash[..n]);
            }
            compress::ZIP_STREAM => {
                self.decompress_bitfield(dib)?;
                self.inflate_pixels(n)?;
            }
            compress::ZIP_STREAM_INIT => {
                self.inflate = Decompress::new(false);
                self.decompress_bitfield(dib)?;
                self.inflate_pixels(n)?;
            }
            compress::LZW_TCB_UNC_START | compress::LZW_TCB_COMP_START => {
                self.decompress_bitfield(dib)?;
                let Self {
                    lzw, cash, uncomp, ..
                } = self;
                lzw.reset();
                lzw.expand(cash, n, uncomp)?;
            }
            compress::LZW_TCB_UNC_CONT | compress::LZW_TCB_COMP_CONT => {
                self.decompress_bitfield(dib)?;
                if n > 0 {
                    let Self {
                        lzw, cash, uncomp, ..
                    } = self;
                    lzw.expand(cash, n, uncomp)?;
                }
            }
            other => return Err(Error::UnsupportedCompression(other)),
        }
        Ok(())
    }

    /// One `Inflater.inflate` call's worth: as much as the output holds.
    fn inflate_pixels(&mut self, n: usize) -> Result<(), Error> {
        self.inflate
            .decompress(&self.cash[..n], &mut self.uncomp, FlushDecompress::None)
            .map_err(|e| Error::Inflate(e.to_string()))?;
        Ok(())
    }

    /// Decompress the changed-tile bitfield and collect the tile indices
    /// (`VideoModel.decompressBitField`).
    fn decompress_bitfield(&mut self, dib: &Dib) -> Result<(), Error> {
        let src = &dib.bitfield_comp;
        match dib.compress_method {
            compress::UNCOMPRESSED | compress::LZW_TCB_UNC_START | compress::LZW_TCB_UNC_CONT => {
                let len = src.len().min(self.bitfield.len());
                self.bitfield[..len].copy_from_slice(&src[..len]);
            }
            compress::LZW_TCB_COMP_START | compress::LZW_TCB_COMP_CONT => {
                rle_decode(src, &mut self.bitfield)?;
            }
            _ => {
                // Shares the pixel stream, and must be inflated before it.
                self.inflate
                    .decompress(src, &mut self.bitfield, FlushDecompress::None)
                    .map_err(|e| Error::Inflate(e.to_string()))?;
            }
        }

        let tiles = (dib.source_cx as usize * dib.source_cy as usize)
            / (dib.tile_cx as usize * dib.tile_cy as usize);
        let tiles = tiles.min(MAX_TILES);
        let idx_size = dib.idx_size as usize;
        let mut found = 0usize;
        let mut tile = 0usize;
        while tile < tiles {
            if self.bitfield[tile >> 3] & (1 << (tile & 7)) != 0 {
                self.indexes[found] = tile as u16;
                found += 1;
            }
            if found >= idx_size {
                break;
            }
            tile += 1;
        }
        Ok(())
    }

    /// Stage 3: turn the decompressed payload into 32-bit pixels
    /// (`VideoModel.postUnCompress`).
    fn post_uncompress(&mut self, dib: &Dib) -> Result<(), Error> {
        let pixels = dib.tile_cx as usize * dib.tile_cy as usize * dib.idx_size as usize;
        if pixels * 4 > self.cash.len() {
            return Err(Error::BadVideoField("tile payload larger than the buffer"));
        }

        match dib.alx_format {
            format::PACK_8 => {
                // Palette indices, one byte per pixel.
                for i in 0..pixels {
                    let colour = self.lut[self.uncomp[i] as usize];
                    self.cash[4 * i..4 * i + 4].copy_from_slice(&colour.to_le_bytes());
                }
            }
            format::PACK_16 => {
                // RGB555 in 16 bits, expanded to 8 bits per channel.
                for i in 0..pixels {
                    let s = u16::from_le_bytes([self.uncomp[i * 2], self.uncomp[i * 2 + 1]]);
                    self.cash[4 * i] = ((s & 0x1F) << 3) as u8;
                    self.cash[4 * i + 1] = ((s & 0x3E0) >> 2) as u8;
                    self.cash[4 * i + 2] = ((s & 0x7C00) >> 7) as u8;
                }
            }
            format::YCBCR_4_2_2 | format::YCBCR_4_0_0 => {
                let (mode, input_bytes) = if dib.alx_format == format::YCBCR_4_2_2 {
                    (InputMode::Ycbcr422, pixels * 2)
                } else {
                    (InputMode::Ycbcr400, pixels)
                };
                // Frames that came through the LZW expander are already in the
                // byte order the converter wants; zlib frames are not.
                let byte_swap = !matches!(
                    dib.compress_method,
                    compress::LZW_TCB_UNC_START
                        | compress::LZW_TCB_UNC_CONT
                        | compress::LZW_TCB_COMP_START
                        | compress::LZW_TCB_COMP_CONT
                );

                let Self {
                    ycbcr,
                    uncomp,
                    cash,
                    ..
                } = self;
                ycbcr.set_input_mode(mode);
                ycbcr.set_tile_size(dib.tile_cx as usize, dib.tile_cy as usize);
                ycbcr.convert(uncomp, input_bytes, byte_swap);

                let n = pixels.min(ycbcr.color_buf_size());
                for i in 0..n {
                    // Undo the converter's deliberate R/B crossover.
                    cash[4 * i + 2] = ycbcr.r_plane()[i];
                    cash[4 * i + 1] = ycbcr.g_plane()[i];
                    cash[4 * i] = ycbcr.b_plane()[i];
                }
            }
            other => return Err(Error::UnsupportedFormat(other)),
        }
        Ok(())
    }

    /// Stage 4: blit each changed tile into the framebuffer
    /// (`VideoModel.unpackBitmap` plus `bitcopy`).
    fn unpack_bitmap(&mut self, dib: &Dib) -> Rect {
        if dib.idx_size == 0 {
            return Rect::default();
        }
        let tile_cx = dib.tile_cx as usize;
        let tile_cy = dib.tile_cy as usize;
        let tiles_per_row = (self.width / dib.tile_cx) as usize;
        if tiles_per_row == 0 {
            return Rect::default();
        }

        let Ok(mut frame) = self.frame.lock() else {
            return Rect::default();
        };
        if frame.pixels.is_empty() {
            return Rect::default();
        }
        let stride = frame.width as usize;
        let height = frame.height as usize;

        let (mut min_x, mut min_y) = (frame.width, frame.height);
        let (mut max_x, mut max_y) = (0u16, 0u16);

        for i in 0..dib.idx_size as usize {
            let index = self.indexes[i] as usize;
            let tile_row = index / tiles_per_row;
            let tile_col = index - tile_row * tiles_per_row;
            let x0 = tile_col * tile_cx;
            let y0 = tile_row * tile_cy;

            min_x = min_x.min(x0 as u16);
            min_y = min_y.min(y0 as u16);
            max_x = max_x.max((x0 + tile_cx) as u16);
            max_y = max_y.max((y0 + tile_cy) as u16);

            // Source tiles are stacked vertically, `tile_cx` pixels wide.
            let src_base = 4 * i * tile_cx * tile_cy;
            for row in 0..tile_cy {
                let y = y0 + row;
                if y >= height {
                    break;
                }
                let src_row = src_base + 4 * row * tile_cx;
                let dst_row = y * stride;
                for col in 0..tile_cx {
                    let x = x0 + col;
                    if x >= stride {
                        break;
                    }
                    let s = src_row + 4 * col;
                    if s + 2 >= self.cash.len() {
                        break;
                    }
                    frame.pixels[dst_row + x] = u32::from(self.cash[s])
                        | u32::from(self.cash[s + 1]) << 8
                        | u32::from(self.cash[s + 2]) << 16;
                }
            }
        }

        Rect {
            left: min_x,
            top: min_y,
            right: max_x.min(frame.width),
            bottom: max_y.min(frame.height),
        }
    }
}

/// Decode a `CmdFlag::Palette` payload: 256 little-endian `0x00RRGGBB` words.
///
/// Palettes arrive on the *command* channel, so the session hands the result to
/// the video decoder through a [`SharedPalette`].
pub fn parse_palette(data: &[u8]) -> Result<[u32; 256], Error> {
    if data.len() < 1024 {
        return Err(Error::BadVideoField("palette shorter than 256 entries"));
    }
    let mut lut = [0u32; 256];
    for (i, c) in data[..1024].as_chunks::<4>().0.iter().enumerate() {
        lut[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }
    Ok(lut)
}

/// The bitfield's run-length coding (`VideoModel.decompressRLE`).
///
/// A control byte under 0x80 introduces `b + 1` literal bytes; 0x80 or above
/// repeats the next byte `(b & 0x7F) + 2` times.
fn rle_decode(input: &[u8], out: &mut [u8]) -> Result<(), Error> {
    let mut i = 0usize;
    let mut o = 0usize;
    while i < input.len() {
        let ctrl = input[i];
        i += 1;
        if ctrl & 0x80 == 0 {
            let count = ctrl as usize + 1;
            for _ in 0..count {
                let b = *input
                    .get(i)
                    .ok_or(Error::BadVideoField("truncated RLE run"))?;
                *out.get_mut(o)
                    .ok_or(Error::BadVideoField("RLE output overflow"))? = b;
                i += 1;
                o += 1;
            }
        } else {
            let count = (ctrl & 0x7F) as usize + 2;
            let b = *input
                .get(i)
                .ok_or(Error::BadVideoField("truncated RLE run"))?;
            for _ in 0..count {
                *out.get_mut(o)
                    .ok_or(Error::BadVideoField("RLE output overflow"))? = b;
                o += 1;
            }
            i += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder() -> (VideoDecoder, SharedFrame) {
        let frame: SharedFrame = Arc::new(Mutex::new(Framebuffer::new()));
        let palette: SharedPalette = Arc::new(Mutex::new(None));
        (VideoDecoder::new(frame.clone(), palette), frame)
    }

    fn pixel(frame: &SharedFrame, idx: usize) -> u32 {
        frame.lock().unwrap().pixels[idx]
    }

    #[test]
    fn palette_needs_a_full_256_entries() {
        assert!(parse_palette(&[0u8; 512]).is_err());
        assert_eq!(parse_palette(&[0xFFu8; 1024]).unwrap()[255], 0xFFFF_FFFF);
    }

    #[test]
    fn rle_expands_literal_and_repeat_runs() {
        let mut out = [0u8; 16];
        // 0x02 -> three literals, then 0x81 -> three copies of 0xAA.
        rle_decode(&[0x02, 1, 2, 3, 0x81, 0xAA], &mut out).unwrap();
        assert_eq!(&out[..6], &[1, 2, 3, 0xAA, 0xAA, 0xAA]);
    }

    #[test]
    fn rle_rejects_a_truncated_run() {
        let mut out = [0u8; 16];
        assert!(rle_decode(&[0x05, 1, 2], &mut out).is_err());
    }

    #[test]
    fn rle_rejects_output_overflow() {
        let mut out = [0u8; 2];
        assert!(rle_decode(&[0xFF, 0xAA], &mut out).is_err());
    }

    /// Build the payload `VideoPayloadReader` expects, with no byte-order
    /// marker (so little-endian) and uncompressed pixels.
    fn make_payload(
        method: i16,
        alx_format: i16,
        pixels: &[u8],
        tile: (u16, u16),
        source: (u16, u16),
        idx_size: u16,
        bitfield: &[u8],
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&method.to_le_bytes());
        p.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        p.extend_from_slice(&alx_format.to_le_bytes());
        p.extend_from_slice(&0i32.to_le_bytes()); // biHeaderSize
        p.extend_from_slice(pixels);
        p.extend_from_slice(&tile.0.to_le_bytes());
        p.extend_from_slice(&tile.1.to_le_bytes());
        p.extend_from_slice(&source.0.to_le_bytes());
        p.extend_from_slice(&source.1.to_le_bytes());
        p.extend_from_slice(&idx_size.to_le_bytes());
        p.extend_from_slice(&(bitfield.len() as u16).to_le_bytes());
        p.extend_from_slice(bitfield);
        p
    }

    #[test]
    fn parses_geometry_and_pixels() {
        let pixels = vec![7u8; 64];
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_16,
            &pixels,
            (8, 8),
            (64, 32),
            1,
            &[0x01],
        );
        let mut scratch = vec![0u8; 1024];
        let dib = parse_dib(&payload, &mut scratch).unwrap();
        assert_eq!(dib.compress_method, compress::UNCOMPRESSED);
        assert_eq!(dib.alx_format, format::PACK_16);
        assert_eq!(dib.compressed_size, 64);
        assert_eq!((dib.tile_cx, dib.tile_cy), (8, 8));
        assert_eq!((dib.source_cx, dib.source_cy), (64, 32));
        assert_eq!(dib.idx_size, 1);
        assert_eq!(dib.bitfield_comp, vec![0x01]);
        assert_eq!(&scratch[..64], &pixels[..]);
    }

    #[test]
    fn honours_the_big_endian_marker() {
        let mut p = Vec::new();
        p.extend_from_slice(&0x4759u16.to_le_bytes()); // "YG": big-endian body
        p.extend_from_slice(&compress::UNCOMPRESSED.to_be_bytes());
        p.extend_from_slice(&0i32.to_be_bytes()); // compressed size
        p.extend_from_slice(&format::PACK_16.to_be_bytes());
        p.extend_from_slice(&0i32.to_be_bytes()); // biHeaderSize
        for v in [8u16, 8, 64, 32, 0, 0] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        let mut scratch = vec![0u8; 64];
        let dib = parse_dib(&p, &mut scratch).unwrap();
        assert_eq!((dib.tile_cx, dib.tile_cy), (8, 8));
        assert_eq!((dib.source_cx, dib.source_cy), (64, 32));
    }

    #[test]
    fn rejects_a_tile_bigger_than_the_screen() {
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_16,
            &[0u8; 8],
            (128, 128),
            (64, 32),
            0,
            &[],
        );
        let mut scratch = vec![0u8; 1024];
        assert!(matches!(
            parse_dib(&payload, &mut scratch),
            Err(Error::BadVideoField(_))
        ));
    }

    #[test]
    fn rejects_a_truncated_payload() {
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_16,
            &[0u8; 8],
            (8, 8),
            (64, 32),
            1,
            &[0x01],
        );
        let mut scratch = vec![0u8; 1024];
        assert!(matches!(
            parse_dib(&payload[..payload.len() - 4], &mut scratch),
            Err(Error::ShortVideoPayload)
        ));
    }

    /// An end-to-end frame: one uncompressed RGB555 tile, in the top-left
    /// corner, drawn into a fresh framebuffer.
    #[test]
    fn decodes_an_uncompressed_tile_into_the_framebuffer() {
        // RGB555 0x7C00 is full red in the top five bits.
        let mut pixels = Vec::new();
        for _ in 0..64 {
            pixels.extend_from_slice(&0x7C00u16.to_le_bytes());
        }
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_16,
            &pixels,
            (8, 8),
            (64, 32),
            1,
            &[0x01], // tile 0 changed
        );

        let (mut dec, frame) = decoder();
        let rect = dec.decode(&payload).unwrap();
        assert_eq!(dec.take_resolution_change(), Some((64, 32)));
        assert_eq!(
            rect,
            Rect {
                left: 0,
                top: 0,
                right: 8,
                bottom: 8
            }
        );
        assert_eq!(pixel(&frame, 0), 0x00F8_0000, "red tile pixel");
        assert_eq!(pixel(&frame, 7 * 64 + 7), 0x00F8_0000, "tile corner");
        assert_eq!(pixel(&frame, 8), 0, "outside the tile");
    }

    /// The tile index picks where the blit lands.
    #[test]
    fn blits_a_tile_at_its_bitfield_position() {
        let mut pixels = Vec::new();
        for _ in 0..64 {
            pixels.extend_from_slice(&0x001Fu16.to_le_bytes()); // full blue
        }
        // 64x32 screen with 8x8 tiles = 8 tiles per row; tile 9 is row 1, col 1.
        let mut bitfield = [0u8; 2];
        bitfield[1] = 1 << 1;
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_16,
            &pixels,
            (8, 8),
            (64, 32),
            1,
            &bitfield,
        );
        let (mut dec, frame) = decoder();
        let rect = dec.decode(&payload).unwrap();
        assert_eq!(
            rect,
            Rect {
                left: 8,
                top: 8,
                right: 16,
                bottom: 16
            }
        );
        assert_eq!(pixel(&frame, 8 * 64 + 8), 0x0000_00F8);
        assert_eq!(pixel(&frame, 0), 0);
    }

    #[test]
    fn palette_frames_use_the_lut() {
        let mut palette = Vec::new();
        for i in 0..256u32 {
            palette.extend_from_slice(&(i * 0x010101).to_le_bytes());
        }
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_8,
            &[5u8; 64],
            (8, 8),
            (64, 32),
            1,
            &[0x01],
        );
        let frame: SharedFrame = Arc::new(Mutex::new(Framebuffer::new()));
        let pending: SharedPalette = Arc::new(Mutex::new(Some(parse_palette(&palette).unwrap())));
        let mut dec = VideoDecoder::new(frame.clone(), pending);
        dec.decode(&payload).unwrap();
        assert_eq!(pixel(&frame, 0), 5 * 0x010101);
    }

    #[test]
    fn unsupported_compression_is_reported_not_panicked() {
        let payload = make_payload(
            2, // ALX_VFW, which the client never handles either
            format::PACK_16,
            &[0u8; 8],
            (8, 8),
            (64, 32),
            1,
            &[0x01],
        );
        let (mut dec, _frame) = decoder();
        assert!(matches!(
            dec.decode(&payload),
            Err(Error::UnsupportedCompression(2))
        ));
    }

    /// A `Dib` carrying only the fields the noise meter reads.
    fn noise_dib(idx_size: u16) -> Dib {
        Dib {
            tile_cx: 16,
            tile_cy: 16,
            source_cx: 640,
            source_cy: 480,
            idx_size,
            ..Dib::default()
        }
    }

    #[test]
    fn noise_reports_nothing_before_a_second_has_passed() {
        let mut m = NoiseMeter::new();
        m.record(&noise_dib(100));
        assert_eq!(m.take(), None);
    }

    #[test]
    fn noise_is_the_change_percentage_per_second() {
        // 640x480 in 16x16 tiles is 1200 tiles; 120 of them is 10%.
        let mut m = NoiseMeter::new();
        m.last_report -= Duration::from_secs(1);
        for _ in 0..3 {
            m.record(&noise_dib(120));
        }
        // Three frames at 10% inside one second reads as 30.
        assert_eq!(m.take(), Some(Noise { level: 30, changes: 360 }));
        // Reporting resets the accumulators.
        m.last_report -= Duration::from_secs(1);
        assert_eq!(m.take(), Some(Noise { level: 0, changes: 0 }));
    }

    #[test]
    fn noise_is_clamped_to_a_hundred() {
        let mut m = NoiseMeter::new();
        m.last_report -= Duration::from_secs(1);
        // Twelve full-screen frames in a second is 1200%, not 1200.
        for _ in 0..12 {
            m.record(&noise_dib(1200));
        }
        assert_eq!(m.take().unwrap().level, 100);
    }

    #[test]
    fn a_barely_changing_screen_does_not_read_as_silent() {
        let mut m = NoiseMeter::new();
        m.last_report -= Duration::from_secs(1);
        // One tile of 1200 rounds to 0%, but something did change.
        m.record(&noise_dib(1));
        assert_eq!(m.take(), Some(Noise { level: 1, changes: 1 }));
    }

    #[test]
    fn a_long_gap_divides_by_its_whole_length() {
        let mut m = NoiseMeter::new();
        m.last_report -= Duration::from_secs(4);
        // 10% once, four seconds ago: a quiet target, not a spike.
        m.record(&noise_dib(120));
        assert_eq!(m.take(), Some(Noise { level: 2, changes: 120 }));
    }

    #[test]
    fn decoding_a_frame_feeds_the_meter() {
        let payload = make_payload(
            compress::UNCOMPRESSED,
            format::PACK_16,
            &[0u8; 128],
            (8, 8),
            (64, 32),
            1,
            &[0x01],
        );
        let (mut dec, _frame) = decoder();
        dec.decode(&payload).unwrap();
        assert_eq!(dec.noise.sum_changes, 1);
        dec.noise.last_report -= Duration::from_secs(1);
        assert_eq!(dec.take_noise().map(|n| n.changes), Some(1));
    }
}
