// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The KVM viewer's user interface, shared by the native and web clients.
//!
//! Everything a person sees lives here: the menu bar, the remote screen, the
//! dialogs and the input routing between them. None of it knows whether it is
//! in a window or a browser tab — what differs is behind [`platform`], which
//! the two shells implement.
//!
//! The split is deliberate. There is one description of what the viewer does,
//! and each shell supplies only what genuinely cannot be shared: a window and
//! an OpenGL context, and raw key and pointer events.

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
