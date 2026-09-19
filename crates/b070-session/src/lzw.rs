//! The video stream's LZW expander.
//!
//! Transcribed from the current firmware's own client
//! (`com.minicom.plugin.LZW.decoder.{CLZWExpander,BufData,LZW_dictionary,
//! LZW_params,LZWStack}`), preserved under `research/decompiled/app-live/`.
//!
//! Two things make this variant unusual:
//!
//! * **Symbols are 32-bit**, not bytes — the codec compresses whole 32-bit
//!   pixels. A "character" is one `i32`; the dictionary maps codes to
//!   (prefix, appended pixel) pairs exactly as textbook LZW does for bytes.
//! * **The code width grows in place**, from 16 up to 20 bits and finally a
//!   32-bit escape mode, driven by in-band control words. Each width is read
//!   from a little-endian bit stream laid over the 32-bit input words.
//!
//! The Java reads each width with a hand-written 32-case switch per width
//! (`BufData.GetNext17Bits` and friends). Those tables are all equivalent to one
//! LSB-first bit reader over the word array, which is what [`BitReader`] is; the
//! tests pin a few of the transcribed cases against it.
//!
//! The decoder is stateful across messages: a delta frame continues the
//! dictionary built by the preceding key frame, so one [`Expander`] lives for
//! the whole session and is only [`Expander::reset`] on a stream restart.

use crate::Error;

/// First dictionary code. Values below this are literal 32-bit pixels.
const LZW_BASE_CODE: i32 = 32768;
/// Largest code the encoder will emit; the dictionary freezes here.
const MAX_CODE: i32 = 1_048_575;
/// Hard cap on dictionary entries (`LZWP.LZW_DIC_SIZE`).
const LZW_DIC_SIZE: usize = 1_015_808;
/// `VideoModel.DEF_LZW_CORDER` — the match-length limit the client asks for.
pub const DEF_LZW_CORDER: usize = 4095;

/// Operating modes, i.e. the current code width (`BufData.opmode`).
mod opmode {
    pub const BITS16: i32 = 0;
    pub const BITS17: i32 = 1;
    pub const BITS18: i32 = 2;
    pub const BITS19: i32 = 3;
    pub const BITS20: i32 = 4;
    pub const BITS32: i32 = 5;
}

/// Control words (`CLZWExpander.CW_*`).
mod ctrl {
    pub const START: i32 = 0;
    pub const RESUME: i32 = 1;
    pub const COMPLETE: i32 = 2;
    pub const FLUSH: i32 = 3;
    pub const FREEZE: i32 = 4;
    pub const INCREMENT: i32 = 5;
    pub const UFLUSH: i32 = 6;
}

/// One decoded stream symbol: either a literal 32-bit pixel or a dictionary
/// code (`LZWCodeType`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Symbol {
    value: i32,
    is_code: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct DictEntry {
    is_code: bool,
    prefix: i32,
    append_char: i32,
}

/// The code dictionary (`LZW_dictionary`). Entry *n* describes code
/// `LZW_BASE_CODE + n`.
#[derive(Debug, Default)]
struct Dictionary {
    entries: Vec<DictEntry>,
}

impl Dictionary {
    fn clear(&mut self) {
        self.entries.clear();
    }

    fn insert(&mut self, is_code: bool, prefix: i32, append_char: i32) -> Result<(), Error> {
        if self.entries.len() >= LZW_DIC_SIZE {
            return Err(Error::Lzw("dictionary overflow"));
        }
        self.entries.push(DictEntry {
            is_code,
            prefix,
            append_char,
        });
        Ok(())
    }

    fn entry(&self, code: i32) -> Result<DictEntry, Error> {
        let idx = code - LZW_BASE_CODE;
        if idx < 0 {
            return Err(Error::Lzw("dictionary lookup below base code"));
        }
        self.entries
            .get(idx as usize)
            .copied()
            .ok_or(Error::Lzw("dictionary lookup past end"))
    }
}

/// The match stack (`LZWStack`). Expanding a code pushes its pixels in reverse,
/// then they are popped into the output in order.
#[derive(Debug)]
struct Stack {
    table: Vec<i32>,
    /// The most recently pushed value, i.e. the one a `pop` would return next.
    first_pop: i32,
    capacity: usize,
}

impl Stack {
    fn new(capacity: usize) -> Self {
        Stack {
            table: Vec::with_capacity(capacity),
            first_pop: 0,
            capacity,
        }
    }

    fn push(&mut self, v: i32) -> Result<(), Error> {
        if self.table.len() >= self.capacity {
            return Err(Error::Lzw("match stack overflow"));
        }
        self.table.push(v);
        self.first_pop = v;
        Ok(())
    }

    fn clear(&mut self) {
        self.table.clear();
    }
}

/// An LSB-first bit reader over the 32-bit input words (`BufData`).
///
/// `bit_pos` replaces the Java pair (`pbufmem`, `mode_cnt`): those two always
/// agree with a single running bit offset, because every code width divides the
/// 32-bit word grid over one switch cycle.
struct BitReader<'a> {
    words: &'a [u32],
    bit_pos: usize,
    opmode: i32,
    /// Set when the last symbol read was a control word.
    ctrl_flag: bool,
    data_val: Symbol,
}

impl<'a> BitReader<'a> {
    fn new(words: &'a [u32]) -> Self {
        BitReader {
            words,
            bit_pos: 0,
            opmode: opmode::BITS16,
            ctrl_flag: false,
            data_val: Symbol::default(),
        }
    }

    /// `BufData.SetOpMode`: mode 0 rewinds to the start of the buffer, any other
    /// mode skips to the next word boundary if we are mid-word.
    fn set_opmode(&mut self, mode: i32) {
        if mode == opmode::BITS16 {
            self.bit_pos = 0;
        } else if self.bit_pos % 32 != 0 {
            self.bit_pos = (self.bit_pos / 32 + 1) * 32;
        }
        self.opmode = mode;
    }

    fn opmode(&self) -> i32 {
        self.opmode
    }

    fn bits(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!(n <= 32);
        let mut out: u32 = 0;
        let mut got = 0u32;
        while got < n {
            let word = *self
                .words
                .get(self.bit_pos / 32)
                .ok_or(Error::Lzw("input exhausted"))?;
            let bit_off = (self.bit_pos % 32) as u32;
            let take = (32 - bit_off).min(n - got);
            let mask = if take == 32 {
                u32::MAX
            } else {
                (1u32 << take) - 1
            };
            out |= ((word >> bit_off) & mask) << got;
            self.bit_pos += take as usize;
            got += take;
        }
        Ok(out)
    }

    /// `BufData.getNewBufData`: read one symbol at the current code width.
    ///
    /// At every width below 32 the escape value (all ones) introduces a control
    /// word, a value with bit 15 set is a dictionary code, and anything else is
    /// a literal whose upper half follows in a second read.
    fn next_symbol(&mut self) -> Result<Symbol, Error> {
        let width: u32 = match self.opmode {
            opmode::BITS16 => 16,
            opmode::BITS17 => 17,
            opmode::BITS18 => 18,
            opmode::BITS19 => 19,
            opmode::BITS20 => 20,
            opmode::BITS32 => 32,
            _ => return Err(Error::Lzw("illegal operation mode")),
        };

        let v = self.bits(width)?;
        if width == 32 {
            if v == u32::MAX {
                self.ctrl_flag = true;
                self.data_val = Symbol {
                    value: self.bits(32)? as i32,
                    is_code: false,
                };
            } else {
                self.ctrl_flag = false;
                self.data_val = Symbol {
                    value: v as i32,
                    is_code: false,
                };
            }
            return Ok(self.data_val);
        }

        let escape = if width == 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        if v == escape {
            self.ctrl_flag = true;
            self.data_val = Symbol {
                value: self.bits(width)? as i32,
                is_code: false,
            };
        } else if v >> 15 != 0 {
            self.ctrl_flag = false;
            self.data_val = Symbol {
                value: v as i32,
                is_code: true,
            };
        } else {
            // A literal pixel: low 16 bits here, high 16 bits in the next read.
            // The Java shifts a possibly-20-bit value left by 16 in an int, so
            // the surplus high bits simply fall off the top.
            let hi = self.bits(width)?;
            self.ctrl_flag = false;
            self.data_val = Symbol {
                value: ((v & 0xFFFF) | (hi << 16)) as i32,
                is_code: false,
            };
        }
        Ok(self.data_val)
    }

    fn is_ctrl(&self) -> bool {
        self.ctrl_flag
    }
}

#[derive(Debug)]
struct Params {
    next_code: i32,
    old_code: Symbol,
    new_code: Symbol,
    character: i32,
    /// The current code width, carried between messages.
    decoding_type: i32,
    /// Once set, the dictionary stops growing (`CW_FREEZE`).
    decode_only: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            next_code: LZW_BASE_CODE,
            old_code: Symbol::default(),
            new_code: Symbol::default(),
            character: 0,
            decoding_type: opmode::BITS16,
            decode_only: false,
        }
    }
}

/// The LZW expander (`CLZWExpander`).
pub struct Expander {
    params: Params,
    stack: Stack,
    dict: Dictionary,
    /// Decoded pixels for the message being expanded (`LZW_params.pUCompBuf`).
    out: Vec<i32>,
}

impl Expander {
    /// `new CLZWExpander(order)`; the client always passes
    /// [`DEF_LZW_CORDER`].
    pub fn new(order: usize) -> Self {
        Expander {
            params: Params::default(),
            stack: Stack::new(order + 2),
            dict: Dictionary::default(),
            out: Vec::new(),
        }
    }

    /// Drop the dictionary and all stream state, as the client does whenever a
    /// key frame starts a new LZW stream.
    pub fn reset(&mut self) {
        self.dict.clear();
        self.params = Params::default();
        self.stack.clear();
    }

    fn translate(&mut self, mut sym: Symbol) -> Result<i32, Error> {
        while sym.is_code {
            let entry = self.dict.entry(sym.value)?;
            self.stack.push(entry.append_char)?;
            sym = Symbol {
                value: entry.prefix,
                is_code: entry.is_code,
            };
        }
        self.stack.push(sym.value)?;
        Ok(sym.value)
    }

    /// Pop the match stack into the output buffer (`LZW_uncomp_store`).
    fn store(&mut self) {
        while let Some(v) = self.stack.table.pop() {
            self.out.push(v);
        }
        self.stack.clear();
    }

    /// `LZW_decode`. `freeze` suppresses the dictionary insert.
    fn decode(&mut self, freeze: bool) -> Result<(), Error> {
        if self.params.new_code.is_code && self.params.new_code.value == self.params.next_code {
            // The classic KwKwK case: the code is the one we are about to
            // define, so it expands to the previous match plus its first pixel.
            self.stack.push(self.params.character)?;
            let old = self.params.old_code;
            self.params.character = self.translate(old)?;
        } else {
            if self.params.new_code.is_code && self.params.new_code.value > self.params.next_code {
                return Err(Error::Lzw("code received is greater than next code"));
            }
            let new = self.params.new_code;
            self.params.character = self.translate(new)?;
        }
        self.params.character = self.stack.first_pop;
        if !freeze && self.params.next_code < MAX_CODE {
            self.dict.insert(
                self.params.old_code.is_code,
                self.params.old_code.value,
                self.params.character,
            )?;
            self.params.next_code += 1;
        }
        self.params.old_code = self.params.new_code;
        self.store();
        Ok(())
    }

    /// Expand one message's worth of compressed data.
    ///
    /// `input[..len]` is read as big-endian 32-bit words (that is how
    /// `CLZWExpander.LZW_expand(byte[], int, byte[])` packs them), and the
    /// decoded pixels are written to `out` as little-endian 32-bit words. The
    /// net byte swap is why the YCbCr stage skips its own swap on LZW frames.
    ///
    /// Returns the number of bytes written to `out`.
    pub fn expand(&mut self, input: &[u8], len: usize, out: &mut [u8]) -> Result<usize, Error> {
        let len = len.min(input.len());
        if len == 0 {
            return Err(Error::Lzw("zero sized input buffer"));
        }
        let words: Vec<u32> = input[..len]
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        self.expand_words(&words)?;

        let produced = self.out.len() * 4;
        if produced > out.len() {
            return Err(Error::Lzw("expanded data overruns the frame buffer"));
        }
        for (i, v) in self.out.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        Ok(produced)
    }

    /// The control-word state machine (`LZW_expand(int[])`).
    fn expand_words(&mut self, words: &[u32]) -> Result<(), Error> {
        let mut buf = BitReader::new(words);
        buf.set_opmode(self.params.decoding_type);
        self.out.clear();

        // `flush_mode`: after CW_FLUSH the stream carries raw 32-bit pixels
        // with no dictionary coding until CW_COMPLETE.
        let mut flush_mode = false;
        let mut in_ctrl = false;

        loop {
            if !in_ctrl {
                self.params.new_code = buf.next_symbol()?;
                if buf.is_ctrl() {
                    in_ctrl = true;
                    continue;
                }
                if flush_mode {
                    self.stack.push(self.params.new_code.value)?;
                    self.store();
                } else {
                    self.decode(self.params.decode_only)?;
                }
                continue;
            }

            in_ctrl = false;
            match self.params.new_code.value {
                ctrl::START => {
                    self.params.new_code = buf.next_symbol()?;
                    if buf.is_ctrl() {
                        in_ctrl = true;
                        continue;
                    }
                    self.params.old_code = self.params.new_code;
                    self.stack.push(self.params.old_code.value)?;
                    self.store();
                    self.params.character = self.params.old_code.value;
                }
                ctrl::RESUME | ctrl::UFLUSH => {
                    self.params.new_code = buf.next_symbol()?;
                    if buf.is_ctrl() {
                        in_ctrl = true;
                        continue;
                    }
                    self.decode(true)?;
                }
                ctrl::COMPLETE => {
                    if flush_mode {
                        buf.set_opmode(opmode::BITS16);
                        self.dict.clear();
                        self.params = Params::default();
                        self.params.decode_only = false;
                    }
                    return Ok(());
                }
                ctrl::FLUSH => {
                    flush_mode = true;
                    buf.set_opmode(opmode::BITS32);
                    self.params.decoding_type = opmode::BITS32;
                }
                ctrl::FREEZE => {
                    self.params.decode_only = true;
                }
                ctrl::INCREMENT => {
                    let next = match buf.opmode() {
                        opmode::BITS16 => opmode::BITS17,
                        opmode::BITS17 => opmode::BITS18,
                        opmode::BITS18 => opmode::BITS19,
                        opmode::BITS19 => opmode::BITS20,
                        _ => return Err(Error::Lzw("illegal increment request at operation mode")),
                    };
                    buf.set_opmode(next);
                    self.params.decoding_type = next;
                }
                other => {
                    tracing::debug!(control_word = other, "undefined LZW control word");
                    return Err(Error::Lzw("undefined control word"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Java reads each width with a switch whose cases were written out by
    /// hand; these are those cases, checked against the generic bit reader.
    #[test]
    fn bit_reader_matches_transcribed_switch_cases() {
        let words = [0x1234_5678u32, 0x9ABC_DEF0, 0x0F1E_2D3C, 0xC3B2_A190];

        // GetNext17Bits, mode_cnt 0 then 1:
        //   case 0: pinBuf[p] & 0x1FFFF
        //   case 1: (pinBuf[p] & 0xFFFE0000) >>> 17 | (pinBuf[p+1] & 3) << 15
        let mut r = BitReader::new(&words);
        r.set_opmode(opmode::BITS17);
        assert_eq!(r.bits(17).unwrap(), words[0] & 0x1FFFF);
        assert_eq!(
            r.bits(17).unwrap(),
            ((words[0] & 0xFFFE_0000) >> 17) | ((words[1] & 3) << 15)
        );
        // case 2: (pinBuf[p] & 0x7FFFC) >>> 2, now on word 1
        assert_eq!(r.bits(17).unwrap(), (words[1] & 0x7_FFFC) >> 2);

        // GetNext19Bits, cases 0, 1, 2
        let mut r = BitReader::new(&words);
        r.set_opmode(opmode::BITS19);
        assert_eq!(r.bits(19).unwrap(), words[0] & 0x7FFFF);
        assert_eq!(
            r.bits(19).unwrap(),
            ((words[0] & 0xFFF8_0000) >> 19) | ((words[1] & 0x3F) << 13)
        );
        assert_eq!(r.bits(19).unwrap(), (words[1] & 0x1FF_FFC0) >> 6);

        // GetNext20Bits, cases 0, 1
        let mut r = BitReader::new(&words);
        r.set_opmode(opmode::BITS20);
        assert_eq!(r.bits(20).unwrap(), words[0] & 0xFFFFF);
        assert_eq!(
            r.bits(20).unwrap(),
            ((words[0] & 0xFFF0_0000) >> 20) | ((words[1] & 0xFF) << 12)
        );

        // GetNext18Bits, cases 0, 1
        let mut r = BitReader::new(&words);
        r.set_opmode(opmode::BITS18);
        assert_eq!(r.bits(18).unwrap(), words[0] & 0x3FFFF);
        assert_eq!(
            r.bits(18).unwrap(),
            ((words[0] & 0xFFFC_0000) >> 18) | ((words[1] & 0xF) << 14)
        );

        // GetNext16Bits: low half, then high half of the same word.
        let mut r = BitReader::new(&words);
        r.set_opmode(opmode::BITS16);
        assert_eq!(r.bits(16).unwrap(), words[0] & 0xFFFF);
        assert_eq!(r.bits(16).unwrap(), (words[0] & 0xFFFF_0000) >> 16);
        assert_eq!(r.bits(16).unwrap(), words[1] & 0xFFFF);
    }

    #[test]
    fn opmode_change_skips_to_word_boundary() {
        let words = [0u32; 4];
        let mut r = BitReader::new(&words);
        r.set_opmode(opmode::BITS17);
        r.bits(17).unwrap();
        assert_eq!(r.bit_pos, 17);
        // Mid-word: switching width rounds up to the next word.
        r.set_opmode(opmode::BITS18);
        assert_eq!(r.bit_pos, 32);
        // Already aligned: no skip.
        r.set_opmode(opmode::BITS19);
        assert_eq!(r.bit_pos, 32);
    }

    /// Build a 16-bit-mode stream by hand and expand it.
    ///
    /// Literals are emitted as two 16-bit halves, control words as 0xFFFF plus
    /// the word, matching `getNewBufData` in reverse.
    struct Writer {
        bits: Vec<bool>,
    }

    impl Writer {
        fn new() -> Self {
            Writer { bits: Vec::new() }
        }
        fn put(&mut self, v: u32, n: u32) {
            for i in 0..n {
                self.bits.push((v >> i) & 1 == 1);
            }
        }
        fn ctrl(&mut self, cw: i32) {
            self.put(0xFFFF, 16);
            self.put(cw as u32, 16);
        }
        fn literal(&mut self, px: u32) {
            self.put(px & 0xFFFF, 16);
            self.put(px >> 16, 16);
        }
        fn code(&mut self, code: i32) {
            self.put(code as u32, 16);
        }
        /// Emit as the big-endian word stream `expand` expects.
        fn finish(mut self) -> Vec<u8> {
            while self.bits.len() % 32 != 0 {
                self.bits.push(false);
            }
            let mut out = Vec::new();
            for word in self.bits.chunks(32) {
                let mut w = 0u32;
                for (i, b) in word.iter().enumerate() {
                    if *b {
                        w |= 1 << i;
                    }
                }
                out.extend_from_slice(&w.to_be_bytes());
            }
            out
        }
    }

    fn expand_to_pixels(bytes: &[u8]) -> Vec<u32> {
        let mut exp = Expander::new(DEF_LZW_CORDER);
        let mut out = vec![0u8; 4096];
        let n = exp.expand(bytes, bytes.len(), &mut out).unwrap();
        out[..n]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn expands_literals_after_start() {
        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(0x0011_2233);
        w.literal(0x0044_5566);
        w.literal(0x0011_2233);
        w.ctrl(ctrl::COMPLETE);
        assert_eq!(
            expand_to_pixels(&w.finish()),
            vec![0x0011_2233, 0x0044_5566, 0x0011_2233]
        );
    }

    /// The first dictionary code (32768) is defined by the first two symbols,
    /// so referencing it afterwards must replay that pair's tail.
    #[test]
    fn expands_a_dictionary_code() {
        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(0x0000_00AA); // defines nothing yet
        w.literal(0x0000_00BB); // defines 32768 = (AA, BB)
        w.literal(0x0000_00CC); // defines 32769 = (BB, CC)
        w.code(LZW_BASE_CODE); // 32768 -> AA BB
        w.ctrl(ctrl::COMPLETE);
        assert_eq!(
            expand_to_pixels(&w.finish()),
            vec![0xAA, 0xBB, 0xCC, 0xAA, 0xBB]
        );
    }

    /// KwKwK: a code that names the entry being defined right now.
    #[test]
    fn expands_the_self_referential_code() {
        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(0x0000_0007);
        w.literal(0x0000_0008); // defines 32768 = (07, 08)
        w.code(LZW_BASE_CODE + 1); // the entry defined by this very step
        w.ctrl(ctrl::COMPLETE);
        assert_eq!(expand_to_pixels(&w.finish()), vec![7, 8, 8, 8]);
    }

    /// CW_FLUSH switches to raw 32-bit pixels until CW_COMPLETE.
    #[test]
    fn flush_mode_passes_raw_words_through() {
        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(0x1234_5678);
        w.ctrl(ctrl::FLUSH);
        // 32-bit mode is word aligned, so pad to the boundary first.
        while w.bits.len() % 32 != 0 {
            w.bits.push(false);
        }
        w.put(0xDEAD_BEEF, 32);
        w.put(0xFFFF_FFFF, 32); // escape
        w.put(ctrl::COMPLETE as u32, 32);
        assert_eq!(
            expand_to_pixels(&w.finish()),
            vec![0x1234_5678, 0xDEAD_BEEF]
        );
    }

    #[test]
    fn truncated_input_errors_instead_of_panicking() {
        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(0x11);
        // no CW_COMPLETE: the reader runs off the end
        let bytes = w.finish();
        let mut exp = Expander::new(DEF_LZW_CORDER);
        let mut out = vec![0u8; 256];
        assert!(matches!(
            exp.expand(&bytes, bytes.len(), &mut out),
            Err(Error::Lzw(_))
        ));
    }

    #[test]
    fn code_past_next_code_is_rejected() {
        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(0x11);
        w.code(LZW_BASE_CODE + 500);
        w.ctrl(ctrl::COMPLETE);
        let bytes = w.finish();
        let mut exp = Expander::new(DEF_LZW_CORDER);
        let mut out = vec![0u8; 256];
        assert!(matches!(
            exp.expand(&bytes, bytes.len(), &mut out),
            Err(Error::Lzw(_))
        ));
    }

    /// A delta frame continues the dictionary the previous frame built, so a
    /// reset must be observable: the same code means different things.
    #[test]
    fn dictionary_persists_until_reset() {
        let mut exp = Expander::new(DEF_LZW_CORDER);
        let mut out = vec![0u8; 4096];

        let mut w = Writer::new();
        w.ctrl(ctrl::START);
        w.literal(1);
        w.literal(2);
        w.ctrl(ctrl::COMPLETE);
        let first = w.finish();
        exp.expand(&first, first.len(), &mut out).unwrap();

        // Continuation: 32768 = (1, 2) is already defined.
        let mut w = Writer::new();
        w.code(LZW_BASE_CODE);
        w.ctrl(ctrl::COMPLETE);
        let second = w.finish();
        let n = exp.expand(&second, second.len(), &mut out).unwrap();
        assert_eq!(&out[..n], &[1, 0, 0, 0, 2, 0, 0, 0]);

        // After a reset the dictionary is empty, so 32768 is now the
        // yet-to-be-defined code and expands from the zeroed stream state
        // instead of replaying (1, 2).
        exp.reset();
        let n = exp.expand(&second, second.len(), &mut out).unwrap();
        assert_eq!(&out[..n], &[0u8; 8]);
    }
}
