// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Raw key capture, which eframe cannot do for us.
//!
//! eframe reports keys as `egui::Key`, and `egui::Key` cannot name 25 of the
//! 106 keys this device has: the whole numeric keypad, Caps/Num/Scroll Lock,
//! Print Screen, Pause, Context Menu and Meta. A KVM replays *physical*
//! scancodes, and the keyboard channel's modifier word carries the three lock
//! states explicitly, so the losses are not cosmetic.
//!
//! `KeyboardEvent.code` is exactly the vocabulary [`PhysicalKey`] speaks, so
//! these listeners read it directly. They run in the **capture** phase, ahead
//! of eframe's own, and while the target owns the keyboard they call
//! `preventDefault` and `stopPropagation` — which both stops the browser acting
//! on the key and stops eframe seeing it.
//!
//! What cannot be taken: a browser keeps Ctrl+W, Ctrl+T, Ctrl+N and F11 for
//! itself whatever a page asks. Those are what the Keyboard menu's macros are
//! for, exactly as they are on a desktop whose window manager claims them.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use netcmdr_ui::PhysicalKey;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::KeyboardEvent;

/// A listener, held alive for as long as it is attached to the page.
type KeyListener = Closure<dyn FnMut(KeyboardEvent)>;

/// One key event, as the viewer wants it.
pub struct KeyEvent {
    pub key: PhysicalKey,
    pub down: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// Keys captured since the last frame drained them.
#[derive(Clone)]
pub struct Keyboard {
    events: Rc<RefCell<VecDeque<KeyEvent>>>,
    /// Whether the target, rather than the menu, should be getting keys.
    /// The listeners read this at event time, so the app refreshes it each
    /// frame from the viewer.
    to_target: Rc<Cell<bool>>,
    /// Held so the listeners stay attached.
    _listeners: Rc<Vec<KeyListener>>,
}

impl Keyboard {
    /// Attach the listeners to the page.
    pub fn attach() -> Keyboard {
        let events: Rc<RefCell<VecDeque<KeyEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
        let to_target = Rc::new(Cell::new(false));
        let window = web_sys::window().expect("a browser window");

        let mut listeners = Vec::new();
        for (name, down) in [("keydown", true), ("keyup", false)] {
            let queue = events.clone();
            let active = to_target.clone();
            let listener = KeyListener::new(move |event: KeyboardEvent| {
                if !active.get() {
                    return; // the menu is typing; let eframe have it
                }
                let Some(key) = PhysicalKey::from_code(&event.code()) else {
                    return; // a key the device has no code for
                };
                // Do this before queueing: the browser acts on the default the
                // moment we return, and eframe's listener is next in line.
                event.prevent_default();
                event.stop_propagation();
                queue.borrow_mut().push_back(KeyEvent {
                    key,
                    down,
                    shift: event.shift_key(),
                    ctrl: event.ctrl_key(),
                    alt: event.alt_key(),
                });
            });
            window
                .add_event_listener_with_callback_and_bool(
                    name,
                    listener.as_ref().unchecked_ref(),
                    true, // capture phase
                )
                .expect("attaching a key listener");
            listeners.push(listener);
        }

        Keyboard {
            events,
            to_target,
            _listeners: Rc::new(listeners),
        }
    }

    /// Tell the listeners who owns the keyboard right now.
    pub fn set_target_has_focus(&self, yes: bool) {
        self.to_target.set(yes);
    }

    pub fn drain(&self) -> Vec<KeyEvent> {
        self.events.borrow_mut().drain(..).collect()
    }
}
