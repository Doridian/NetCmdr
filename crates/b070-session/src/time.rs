// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A monotonic clock that exists on every target this builds for.
//!
//! `std::time::Instant::now()` compiles on `wasm32-unknown-unknown` and then
//! **panics at run time** — there is no clock behind it. The failure is a
//! runtime abort in the middle of a working session, which is exactly the kind
//! of thing a compile of the wasm target does not catch, so the import is
//! centralised here rather than repeated at each use.
//!
//! `web-time` answers from `performance.now()`, which is monotonic and has the
//! resolution this needs (the noise meter reports once a second, the heartbeat
//! every thirty).

pub use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;
