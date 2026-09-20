// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The web client, carried inside the binary and unpacked when the bridge
//! starts.
//!
//! The bridge is one file. There is no directory to point it at, nothing to
//! deploy beside it and nothing it can be served a stale or hostile copy of:
//! the client it hands out is the one it was built with. `build.rs` puts it
//! there — see that file for how the client gets in.
//!
//! # Why brotli is the stored form
//!
//! The client is a WebAssembly module of several megabytes that the browser
//! must have in full before anything appears, and brotli takes it to under a
//! third of that. Storing that third is what keeps the executable small, and it
//! is also the copy most browsers are served, so the brotli work happens once
//! at build time and never again.
//!
//! What [`unpack`](Assets::unpack) does at startup is therefore the cheap half:
//! decompress each file, which is milliseconds, and gzip the result for the
//! clients that do not take brotli — together under half a second for the whole
//! client, against the half minute the brotli would have cost. Both copies are then
//! held in memory for the life of the process, immutable — no file can change
//! underneath the bridge any more, so nothing needs revalidating per request.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::OnceLock;
use std::time::Instant;

/// One file as `build.rs` stored it.
pub(crate) struct Embedded {
    /// URL path relative to the root of the client, e.g. `netcmdr_web_bg.wasm`.
    path: &'static str,
    /// Length of the original file, so the decompressed copy is allocated once.
    plain_len: usize,
    /// Whether [`Self::body`] is brotli-compressed. It is, unless brotli made
    /// this particular file bigger.
    brotli: bool,
    body: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// gzip's maximum. Under a second for the whole client, and gzip is the
/// fallback for clients that cannot take brotli, so there is no reason to
/// spend less.
const GZIP_LEVEL: u32 = 9;

/// A content coding the bridge can serve a file in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Encoding {
    /// Preferred: every browser that speaks it also speaks gzip.
    Brotli,
    Gzip,
}

impl Encoding {
    /// The token as `Content-Encoding` and `Accept-Encoding` spell it.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
        }
    }
}

/// One file of the client, in every coding the bridge can send it in.
pub struct File {
    plain: Vec<u8>,
    /// The bytes in the binary, when they are the brotli ones.
    brotli: Option<&'static [u8]>,
    /// `None` where gzip did not make the file smaller.
    gzip: Option<Vec<u8>>,
}

impl File {
    /// The file itself.
    pub fn plain(&self) -> &[u8] {
        &self.plain
    }

    /// The file in `encoding`, if a copy in it is worth sending.
    pub fn encoded(&self, encoding: Encoding) -> Option<&[u8]> {
        match encoding {
            Encoding::Brotli => self.brotli,
            Encoding::Gzip => self.gzip.as_deref(),
        }
    }
}

/// The whole client, ready to serve.
pub struct Assets {
    files: HashMap<&'static str, File>,
    /// What the files came to in memory, for the line the bridge logs.
    unpacked: usize,
}

impl Assets {
    /// Decompress an embedded table and gzip each file beside it.
    ///
    /// # Panics
    ///
    /// If a stored file will not decompress. These bytes were written by this
    /// crate's own build script and read from the executable's own image, so
    /// that means a corrupt binary; serving a client with a hole in it would
    /// reach the browser as an unexplained failure to start, which is a worse
    /// way to find out.
    pub(crate) fn unpack(embedded: &'static [Embedded]) -> Assets {
        let mut files = HashMap::with_capacity(embedded.len());
        let mut unpacked = 0;
        for entry in embedded {
            let plain = if entry.brotli {
                unbrotli(entry.body, entry.plain_len)
                    .unwrap_or_else(|e| panic!("embedded {} will not decompress: {e}", entry.path))
            } else {
                entry.body.to_vec()
            };
            let gzip = match gzip(&plain) {
                // A copy that did not shrink is not worth the memory: the
                // saving is smaller than the two headers announcing it.
                Ok(packed) if packed.len() < plain.len() => Some(packed),
                Ok(_) => None,
                Err(e) => {
                    // Not fatal: brotli and the plain bytes still serve every
                    // browser this decade.
                    tracing::warn!(path = entry.path, error = %e, "gzipping failed");
                    None
                }
            };
            unpacked += plain.len() + gzip.as_ref().map_or(0, Vec::len);
            files.insert(
                entry.path,
                File {
                    plain,
                    brotli: entry.brotli.then_some(entry.body),
                    gzip,
                },
            );
        }
        Assets { files, unpacked }
    }

    /// The file at a path relative to the root of the client.
    pub fn get(&self, path: &str) -> Option<&File> {
        self.files.get(path)
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// How much memory the unpacked copies take.
    pub fn unpacked_len(&self) -> usize {
        self.unpacked
    }
}

/// The embedded client, unpacked on first use.
///
/// The bridge calls this before it binds, so that the second of gzip is spent
/// while nothing is waiting on it and a failure is a startup failure.
pub fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let started = Instant::now();
        let assets = Assets::unpack(EMBEDDED);
        if !assets.is_empty() {
            tracing::info!(
                files = assets.len(),
                size = format_args!("{} KiB", assets.unpacked_len() / 1024),
                took = format_args!("{:.2}s", started.elapsed().as_secs_f32()),
                "web client unpacked"
            );
        }
        assets
    })
}

fn unbrotli(packed: &[u8], plain_len: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(plain_len);
    brotli_decompressor::Decompressor::new(packed, 64 * 1024).read_to_end(&mut out)?;
    Ok(out)
}

fn gzip(plain: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
    encoder.write_all(plain)?;
    encoder.finish()
}

/// A table as `build.rs` would have written it, from bytes made here.
///
/// Leaked because the real one is a `static` in the binary; a test that built a
/// borrowed one would be testing a different lifetime than the code has to work
/// with. Shared with the tests in [`crate::http`], which need assets to serve.
#[cfg(test)]
pub(crate) fn table_for_test(entries: Vec<(&'static str, Vec<u8>)>) -> &'static [Embedded] {
    let built: Vec<Embedded> = entries
        .into_iter()
        .map(|(path, plain)| {
            let packed = brotli_for_test(&plain);
            let compressed = packed.len() < plain.len();
            Embedded {
                path,
                plain_len: plain.len(),
                brotli: compressed,
                body: Box::leak(if compressed { packed } else { plain }.into_boxed_slice()),
            }
        })
        .collect();
    Box::leak(built.into_boxed_slice())
}

/// The build script's compressor, so a test exercises the same bytes it would
/// have embedded rather than a hand-made approximation.
#[cfg(test)]
fn brotli_for_test(plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = brotli::CompressorWriter::new(&mut out, 64 * 1024, 11, 22);
    encoder.write_all(plain).unwrap();
    drop(encoder);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Something that compresses well, as the real client does.
    fn compressible() -> Vec<u8> {
        "export function start() { /* padding */ }\n"
            .repeat(64)
            .into_bytes()
    }

    #[test]
    fn a_stored_file_comes_back_in_three_codings() {
        let assets = Assets::unpack(table_for_test(vec![("app.wasm", compressible())]));
        let file = assets.get("app.wasm").expect("app.wasm");

        // The plain copy is the file as the build saw it, made back out of
        // the stored brotli rather than kept beside it.
        assert_eq!(file.plain(), compressible());
        let br = file.encoded(Encoding::Brotli).expect("brotli");
        let gz = file.encoded(Encoding::Gzip).expect("gzip");
        assert!(br.len() < file.plain().len());
        assert!(gz.len() < file.plain().len());
        // Brotli is the stored form precisely because it is the smaller one.
        assert!(br.len() < gz.len());
    }

    #[test]
    fn an_unknown_path_is_simply_absent() {
        let assets = Assets::unpack(table_for_test(vec![("index.html", compressible())]));
        assert!(assets.get("nope.html").is_none());
        assert!(assets.get("../etc/passwd").is_none());
    }

    /// Four kilobytes nothing can compress, standing in for the PNGs and
    /// woff2s a client can carry. xorshift64 rather than a hash of a counter:
    /// brotli finds the structure in the latter and the test then proves the
    /// opposite of what it means to.
    fn noise() -> Vec<u8> {
        let mut x: u64 = 0x243F_6A88_85A3_08D3;
        (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 33) as u8
            })
            .collect()
    }

    /// A coding that would not shrink the file is not offered at all — and a
    /// file brotli does not shrink is stored as it is, so the bytes in the
    /// binary are never larger than the file they came from.
    #[test]
    fn an_incompressible_file_is_served_as_it_is() {
        let noise = noise();
        let assets = Assets::unpack(table_for_test(vec![("noise.bin", noise.clone())]));
        let file = assets.get("noise.bin").expect("noise.bin");

        assert_eq!(file.plain(), noise);
        assert!(file.encoded(Encoding::Brotli).is_none());
        assert!(file.encoded(Encoding::Gzip).is_none());
    }

    #[test]
    fn the_coding_names_are_the_ones_the_headers_use() {
        assert_eq!(Encoding::Brotli.name(), "br");
        assert_eq!(Encoding::Gzip.name(), "gzip");
    }
}
