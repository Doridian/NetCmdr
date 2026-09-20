// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The compressed copies of the static files, made once when the bridge starts.
//!
//! The web client is a WebAssembly module of several megabytes that the browser
//! must have in full before anything appears, and it compresses to under a
//! third of that. Compressing it per request would be absurd; shipping it
//! precompressed from the build script made the bridge depend on whatever
//! happened to be sitting next to the file; and baking it into the binary would
//! tie the executable to one build of the client. So the bridge reads
//! `--static-dir` at startup and keeps the compressed bytes in memory itself.
//!
//! The work happens on a background thread because brotli at quality 11 takes
//! tens of seconds on a module that size. The listener is already accepting by
//! then: a file with nothing cached for it yet is served exactly as it is on
//! disk, so the bridge is usable the moment it starts and merely gets cheaper
//! to download from over the next minute.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// Below this a compressed copy is not worth the memory: the saving is smaller
/// than the two headers that announce it.
const MIN_SIZE: u64 = 512;

/// Above this the file is left alone. Nothing the client is built from comes
/// close, and the point of the ceiling is that an unrelated file dropped into
/// the directory cannot make the bridge hold hundreds of megabytes.
const MAX_SIZE: u64 = 128 * 1024 * 1024;

/// How deep to walk. A symlinked directory can point at its own parent, and
/// this is cheaper than remembering every inode visited.
const MAX_DEPTH: usize = 16;

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

/// What a file looked like when it was compressed.
///
/// A rebuild while the bridge is running replaces the file underneath us, and
/// serving the old compressed bytes under the new file's name would hand the
/// browser a stale client with no sign that anything was wrong.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

struct Entry {
    stamp: Stamp,
    body: Arc<[u8]>,
}

/// The static directory, plus whatever has been compressed of it so far.
pub struct Assets {
    root: PathBuf,
    compressed: Mutex<HashMap<(PathBuf, Encoding), Entry>>,
}

impl Assets {
    pub fn new(root: PathBuf) -> Arc<Assets> {
        Arc::new(Assets {
            root,
            compressed: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compress the directory on a thread of its own, leaving the caller free
    /// to start accepting connections.
    pub fn warm_in_background(self: &Arc<Self>) {
        let assets = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("b070-bridge-compress".into())
            .spawn(move || assets.warm());
        if let Err(e) = spawned {
            // Not fatal: every file is still servable, just uncompressed.
            tracing::warn!(error = %e, "could not spawn the compression thread");
        }
    }

    /// Compress every compressible file under the root.
    ///
    /// gzip first, for all of them, then brotli: gzip is seconds of work where
    /// brotli is tens of them, so this way a client that connects early still
    /// gets a compressed download rather than waiting for the better one.
    pub fn warm(&self) {
        let mut files = Vec::new();
        collect(&self.root, 0, &mut files);
        if files.is_empty() {
            tracing::debug!(root = %self.root.display(), "nothing to compress");
            return;
        }

        for encoding in [Encoding::Gzip, Encoding::Brotli] {
            let started = Instant::now();
            let mut raw = 0u64;
            let mut packed = 0u64;
            for file in &files {
                match self.compress(file, encoding) {
                    Ok(Some((before, after))) => {
                        raw += before;
                        packed += after;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(path = %file.display(), error = %e, "compressing failed")
                    }
                }
            }
            if raw > 0 {
                tracing::info!(
                    encoding = encoding.name(),
                    from = format_args!("{} KiB", raw / 1024),
                    to = format_args!("{} KiB", packed / 1024),
                    took = format_args!("{:.1}s", started.elapsed().as_secs_f32()),
                    "static files compressed"
                );
            }
        }
    }

    /// Compress one file, returning the sizes it went between, or `None` if it
    /// was left alone.
    fn compress(&self, path: &Path, encoding: Encoding) -> io::Result<Option<(u64, u64)>> {
        let meta = std::fs::metadata(path)?;
        if !worth_compressing(path, &meta) {
            return Ok(None);
        }
        let stamp = Stamp::of(&meta);
        let plain = std::fs::read(path)?;
        // The file may have been replaced between the two reads above; a stamp
        // taken from the bytes we actually hold would be a lie either way, so
        // the one recorded is re-checked when the file is served.
        let body = match encoding {
            Encoding::Gzip => gzip(&plain)?,
            Encoding::Brotli => brotli(&plain)?,
        };

        // A file that does not shrink is not worth keeping a second copy of.
        if body.len() >= plain.len() {
            tracing::debug!(path = %path.display(), encoding = encoding.name(), "no smaller");
            return Ok(None);
        }
        let sizes = (plain.len() as u64, body.len() as u64);

        let mut compressed = self.compressed.lock().unwrap_or_else(|e| e.into_inner());
        compressed.insert(
            (path.to_path_buf(), encoding),
            Entry {
                stamp,
                body: body.into(),
            },
        );
        Ok(Some(sizes))
    }

    /// The compressed form of `path`, if there is one and it still matches the
    /// file on disk.
    pub fn get(&self, path: &Path, encoding: Encoding) -> Option<Arc<[u8]>> {
        let compressed = self.compressed.lock().unwrap_or_else(|e| e.into_inner());
        let entry = compressed.get(&(path.to_path_buf(), encoding))?;
        let stamp = std::fs::metadata(path).ok().map(|m| Stamp::of(&m))?;
        if stamp != entry.stamp {
            // The file changed since startup. Serving it plainly is correct and
            // slow, which is the right way round; a restart caches it again.
            tracing::debug!(path = %path.display(), "cached copy is stale");
            return None;
        }
        Some(Arc::clone(&entry.body))
    }
}

/// Every file under `root`, depth-first.
fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        tracing::warn!(path = %dir.display(), "too deep; not descending further");
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "cannot list");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect(&path, depth + 1, out),
            // `file_type` does not follow symlinks; `metadata` does, so a link
            // to a file is followed and a link to a directory is not.
            Ok(t) if t.is_file() || t.is_symlink() => out.push(path),
            _ => {}
        }
    }
}

/// Whether a file is one the bridge should hold a compressed copy of.
///
/// The extension list is the deciding one: a PNG, a woff2 or a `.br` left over
/// from an older build is already compressed, and running it through gzip
/// again spends memory to make it very slightly bigger.
fn worth_compressing(path: &Path, meta: &std::fs::Metadata) -> bool {
    if !meta.is_file() || meta.len() < MIN_SIZE || meta.len() > MAX_SIZE {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("wasm" | "js" | "mjs" | "html" | "css" | "json" | "svg" | "txt" | "map" | "ico")
    )
}

fn gzip(plain: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
    encoder.write_all(plain)?;
    encoder.finish()
}

/// gzip's maximum. Seconds of work for the whole client, and gzip is the
/// fallback for clients that cannot take brotli, so there is no reason to
/// spend less.
const GZIP_LEVEL: u32 = 9;

/// Brotli's maximum quality, and the largest window the format defines without
/// the negotiated extension every browser does not have.
const BROTLI_QUALITY: u32 = 11;
const BROTLI_WINDOW: u32 = 22;

fn brotli(plain: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = brotli::CompressorWriter::new(
        &mut out,
        // Buffer size; the input is already in memory, so this only sets how
        // often the encoder is handed more of it.
        64 * 1024,
        BROTLI_QUALITY,
        BROTLI_WINDOW,
    );
    encoder.write_all(plain)?;
    drop(encoder);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("b070-assets-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Something that compresses well and clears `MIN_SIZE`.
    fn compressible() -> Vec<u8> {
        "export function start() { /* padding */ }\n"
            .repeat(64)
            .into_bytes()
    }

    #[test]
    fn warming_fills_both_encodings() {
        let dir = scratch("both");
        std::fs::write(dir.join("app.js"), compressible()).unwrap();

        let assets = Assets::new(dir.clone());
        assets.warm();

        let file = dir.join("app.js");
        let br = assets.get(&file, Encoding::Brotli).expect("brotli");
        let gz = assets.get(&file, Encoding::Gzip).expect("gzip");
        assert!(br.len() < compressible().len());
        assert!(gz.len() < compressible().len());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Nothing is written beside the files: the compressed copies live in this
    /// process and nowhere else.
    #[test]
    fn nothing_is_written_to_the_directory() {
        let dir = scratch("clean");
        std::fs::write(dir.join("app.js"), compressible()).unwrap();

        Assets::new(dir.clone()).warm();

        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["app.js"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A rebuild under a running bridge must not be served from the old bytes.
    #[test]
    fn a_changed_file_invalidates_its_compressed_copy() {
        let dir = scratch("stale");
        let file = dir.join("app.js");
        std::fs::write(&file, compressible()).unwrap();

        let assets = Assets::new(dir.clone());
        assets.warm();
        assert!(assets.get(&file, Encoding::Gzip).is_some());

        let mut replacement = compressible();
        replacement.extend_from_slice(b"// rebuilt\n");
        std::fs::write(&file, replacement).unwrap();
        assert!(assets.get(&file, Encoding::Gzip).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn already_compressed_and_tiny_files_are_left_alone() {
        let dir = scratch("skipped");
        std::fs::write(dir.join("logo.png"), compressible()).unwrap();
        std::fs::write(dir.join("old.js.br"), compressible()).unwrap();
        std::fs::write(dir.join("tiny.js"), b"let a=1;\n").unwrap();

        let assets = Assets::new(dir.clone());
        assets.warm();

        for name in ["logo.png", "old.js.br", "tiny.js"] {
            assert!(
                assets.get(&dir.join(name), Encoding::Gzip).is_none(),
                "{name} should not have been compressed"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nested_directories_are_walked() {
        let dir = scratch("nested");
        std::fs::create_dir_all(dir.join("assets/js")).unwrap();
        let deep = dir.join("assets/js/app.js");
        std::fs::write(&deep, compressible()).unwrap();

        let assets = Assets::new(dir.clone());
        assets.warm();
        assert!(assets.get(&deep, Encoding::Brotli).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }
}
