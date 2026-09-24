// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The KVM viewer's user interface, shared by the native and web clients.
//!
//! Menu bar, remote screen, dialogs and input routing. Host differences are
//! behind [`platform`]; each shell supplies only a window, GL context and raw
//! input.

pub mod key;
pub mod macros;
pub mod menu;
pub mod platform;
pub mod screen;
pub mod viewer;

pub use key::PhysicalKey;
pub use menu::{Action, CursorMode, Menu, Scaling};
pub use platform::{Host, MgmtLink, MgmtReply, SessionLink};
pub use screen::Screen;
pub use viewer::{Config, Viewer};
