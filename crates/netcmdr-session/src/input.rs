// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Keyboard and mouse messages.
//!
//! Transcribed from `com.minicom.plugin.km_impl.*`.
//!
//! Each event is 16 bytes (class + three params, LE `u32`s) sent as
//! `CmdFlag::RawData`. Mouse moves also set the header command to
//! `MOUSE_MOVE`.
//!
//! Mouse coordinates are **absolute remote pixels**. Button events send (0, 0)
//! and rely on the last move.

use netcmdr_proto::kvmmsg::{Cmd, CmdFlag, Message};

/// Payload size (`KMConsts.CLIENT_RAT_MSG_SIZE`).
const RAT_MSG_SIZE: usize = 16;

/// Message classes (`ClientRatMsgType`).
mod class {
    pub const MOUSE: i32 = 256;
    pub const KEYBOARD: i32 = 257;
    pub const SETTINGS: i32 = 258;
}

/// Mouse message types (`MouseMsgType`).
mod mouse_type {
    pub const MOVE: i32 = 514;
    pub const LEFT_DOWN: i32 = 515;
    pub const LEFT_UP: i32 = 516;
    pub const MIDDLE_DOWN: i32 = 518;
    pub const MIDDLE_UP: i32 = 519;
    pub const RIGHT_DOWN: i32 = 521;
    pub const RIGHT_UP: i32 = 522;
    pub const WHEEL: i32 = 528;
    pub const QUICK_SYNC: i32 = 531;
}

/// Keyboard message types (`KeyboardMsgType`).
mod key_type {
    pub const KEY_UP: i32 = 769;
    pub const KEY_DOWN: i32 = 771;
    pub const CTRL_ALT_DEL: i32 = 773;
}

/// Modifier and lock state sent with every key event (`KMConsts`), so the
/// device can keep the target's PS/2 state in sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}

impl Modifiers {
    fn bits(self) -> i32 {
        let mut f = 0;
        if self.scroll_lock {
            f |= 1;
        }
        if self.num_lock {
            f |= 2;
        }
        if self.caps_lock {
            f |= 4;
        }
        if self.shift {
            f |= 8;
        }
        if self.ctrl {
            f |= 16;
        }
        if self.alt {
            f |= 32;
        }
        f
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Absolute, in remote pixels.
    MouseMove {
        x: i32,
        y: i32,
    },
    /// Uses the last known position.
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Wheel notches; positive is up.
    MouseWheel(i32),
    /// Re-sync the remote pointer.
    MouseQuickSync,
    /// Set 1 scancode with extended-key flag.
    Key {
        scancode: u8,
        extended: bool,
        down: bool,
        modifiers: Modifiers,
    },
    CtrlAltDel,
}

fn rat_packet(class: i32, a: i32, b: i32, c: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(RAT_MSG_SIZE);
    for word in [class, a, b, c] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

pub fn encode(input: Input) -> Message {
    match input {
        Input::MouseMove { x, y } => Message::new(
            CmdFlag::RawData,
            // Only moves carry a command.
            Cmd::MouseMove,
            rat_packet(class::MOUSE, mouse_type::MOVE, x, y),
        ),
        Input::MouseButton { button, down } => {
            let t = match (button, down) {
                (MouseButton::Left, true) => mouse_type::LEFT_DOWN,
                (MouseButton::Left, false) => mouse_type::LEFT_UP,
                (MouseButton::Middle, true) => mouse_type::MIDDLE_DOWN,
                (MouseButton::Middle, false) => mouse_type::MIDDLE_UP,
                (MouseButton::Right, true) => mouse_type::RIGHT_DOWN,
                (MouseButton::Right, false) => mouse_type::RIGHT_UP,
            };
            Message::new(
                CmdFlag::RawData,
                Cmd::Undefined,
                rat_packet(class::MOUSE, t, 0, 0),
            )
        }
        Input::MouseWheel(notches) => Message::new(
            CmdFlag::RawData,
            Cmd::Undefined,
            // The wheel count takes the x slot.
            rat_packet(class::MOUSE, mouse_type::WHEEL, notches, 0),
        ),
        Input::MouseQuickSync => Message::new(
            CmdFlag::RawData,
            Cmd::Undefined,
            rat_packet(class::MOUSE, mouse_type::QUICK_SYNC, 0, 0),
        ),
        Input::Key {
            scancode,
            extended,
            down,
            modifiers,
        } => {
            // Scancode in byte 2, extended flag in byte 3 (`KBMINFO.nKbCodes`).
            let mut codes = (scancode as i32) << 16;
            if extended {
                codes |= 0x0100_0000;
            }
            let t = if down {
                key_type::KEY_DOWN
            } else {
                key_type::KEY_UP
            };
            Message::new(
                CmdFlag::RawData,
                Cmd::Undefined,
                rat_packet(class::KEYBOARD, t, codes, modifiers.bits()),
            )
        }
        Input::CtrlAltDel => Message::new(
            CmdFlag::RawData,
            Cmd::Undefined,
            rat_packet(class::SETTINGS, key_type::CTRL_ALT_DEL, 0, 0),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(m: &Message) -> Vec<i32> {
        m.payload
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn a_move_carries_coordinates_and_the_move_command() {
        let m = encode(Input::MouseMove { x: 640, y: 480 });
        assert_eq!(m.payload.len(), RAT_MSG_SIZE);
        assert_eq!(words(&m), vec![class::MOUSE, mouse_type::MOVE, 640, 480]);
        assert_eq!(m.header.command, Cmd::MouseMove);
        assert_eq!(m.header.cmd_flag, CmdFlag::RawData);
    }

    #[test]
    fn buttons_send_no_coordinates_and_no_command() {
        let m = encode(Input::MouseButton {
            button: MouseButton::Right,
            down: true,
        });
        assert_eq!(words(&m), vec![class::MOUSE, mouse_type::RIGHT_DOWN, 0, 0]);
        assert_eq!(m.header.command, Cmd::Undefined);
    }

    #[test]
    fn the_wheel_count_takes_the_x_slot() {
        let m = encode(Input::MouseWheel(-3));
        assert_eq!(words(&m), vec![class::MOUSE, mouse_type::WHEEL, -3, 0]);
    }

    #[test]
    fn a_key_packs_the_scancode_into_the_third_byte() {
        // 'A' is scancode 30, not extended.
        let m = encode(Input::Key {
            scancode: 30,
            extended: false,
            down: true,
            modifiers: Modifiers::default(),
        });
        assert_eq!(
            words(&m),
            vec![class::KEYBOARD, key_type::KEY_DOWN, 30 << 16, 0]
        );
    }

    #[test]
    fn an_extended_key_sets_the_high_flag() {
        // Right Ctrl is scancode 29, extended.
        let m = encode(Input::Key {
            scancode: 29,
            extended: true,
            down: false,
            modifiers: Modifiers::default(),
        });
        assert_eq!(
            words(&m),
            vec![
                class::KEYBOARD,
                key_type::KEY_UP,
                (29 << 16) | 0x0100_0000,
                0
            ]
        );
    }

    #[test]
    fn modifier_bits_match_the_client() {
        let mods = Modifiers {
            shift: true,
            ctrl: true,
            alt: true,
            num_lock: true,
            caps_lock: true,
            scroll_lock: true,
        };
        assert_eq!(mods.bits(), 1 | 2 | 4 | 8 | 16 | 32);
        assert_eq!(
            Modifiers {
                ctrl: true,
                ..Default::default()
            }
            .bits(),
            16
        );
    }

    #[test]
    fn ctrl_alt_del_is_a_settings_message() {
        let m = encode(Input::CtrlAltDel);
        assert_eq!(
            words(&m),
            vec![class::SETTINGS, key_type::CTRL_ALT_DEL, 0, 0]
        );
    }
}
