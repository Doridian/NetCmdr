// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Keyboard and mouse messages.
//!
//! Transcribed from `com.minicom.plugin.km_impl.*`.
//!
//! Every input event is a 16-byte payload of four little-endian 32-bit words —
//! a message class followed by three parameters — carried on the keyboard/mouse
//! channel as `CmdFlag::RawData`. Mouse *moves* additionally set the header's
//! command to `MOUSE_MOVE`, which is how the device tells them apart from the
//! rest without looking at the body.
//!
//! Mouse coordinates are **absolute, in remote screen pixels**. The vendor's
//! client sends the panel coordinate directly because its panel is 1:1 with the
//! framebuffer, and it sends (0, 0) with button events, relying on the last move
//! to have positioned the pointer.

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

/// Modifier and lock state carried with every key event (`KMConsts`).
///
/// The device needs these because it replays scancodes into a real PS/2
/// keyboard controller and has to keep the target's shift and lock state in
/// step with the local one.
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

/// One input event to send to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Absolute pointer position, in remote screen pixels.
    MouseMove { x: i32, y: i32 },
    /// A button press or release. The device uses the last known position.
    MouseButton { button: MouseButton, down: bool },
    /// Wheel notches; positive scrolls up, as the vendor's client sends it.
    MouseWheel(i32),
    /// Ask the device to re-sync the remote pointer with ours.
    MouseQuickSync,
    /// A PC scancode (set 1) with its extended-key flag.
    Key {
        scancode: u8,
        extended: bool,
        down: bool,
        modifiers: Modifiers,
    },
    /// The one key combination a KVM has to inject out of band.
    CtrlAltDel,
}

fn rat_packet(class: i32, a: i32, b: i32, c: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(RAT_MSG_SIZE);
    for word in [class, a, b, c] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// Encode an input event as the message to put on the keyboard/mouse channel.
pub fn encode(input: Input) -> Message {
    match input {
        Input::MouseMove { x, y } => Message::new(
            CmdFlag::RawData,
            // Only moves carry a command; everything else leaves it undefined.
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
            // The scancode sits in the third byte, with the extended-key flag
            // one byte above it (`KBMINFO.nKbCodes`).
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
