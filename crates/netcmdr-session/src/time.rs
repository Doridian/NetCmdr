// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A monotonic clock for all targets. `std::time::Instant::now()` compiles on
//! `wasm32-unknown-unknown` but panics at runtime, so use this instead;
//! `web-time` uses `performance.now()`.

pub use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;
