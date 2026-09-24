// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Raw key capture. `egui::Key` lacks 25 of the device's 106 keys (keypad,
//! locks, Print Screen, Pause, Menu, Meta), so we read `KeyboardEvent.code`
//! directly.
//!
//! Listeners run in the capture phase before eframe's and, while the target
//! has the keyboard, call `preventDefault` and `stopPropagation`.
//!
//! Browsers always keep Ctrl+W, Ctrl+T, Ctrl+N and F11; use the Keyboard menu.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use netcmdr_ui::PhysicalKey;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::KeyboardEvent;

type KeyListener = Closure<dyn FnMut(KeyboardEvent)>;

pub struct KeyEvent {
    pub key: PhysicalKey,
    pub down: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Clone)]
pub struct Keyboard {
    events: Rc<RefCell<VecDeque<KeyEvent>>>,
    /// Whether the target gets keys; refreshed each frame.
    to_target: Rc<Cell<bool>>,
    _listeners: Rc<Vec<KeyListener>>,
}

impl Keyboard {
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

    pub fn set_target_has_focus(&self, yes: bool) {
        self.to_target.set(yes);
    }

    pub fn drain(&self) -> Vec<KeyEvent> {
        self.events.borrow_mut().drain(..).collect()
    }
}
