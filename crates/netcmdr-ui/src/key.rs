// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Physical keys, and the PC set-1 scancodes the device replays.
//!
//! Keys are physical positions; the target applies its own layout.
//!
//! [`PhysicalKey`] uses W3C `KeyboardEvent.code` names, which `winit` shares
//! except `Super` vs `Meta`. W3C name, winit key and scancode live in one
//! table so they can't drift.
//!
//! Checked against the vendor's `JVKEYTOSCAN` (`com.minicom.plugin.km_impl`).

/// Build the key enum and every mapping over it from one table.
macro_rules! physical_keys {
    ($($web:ident $winit:ident $code:literal $ext:literal;)*) => {
        /// A key by its position, independent of layout.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum PhysicalKey {
            $($web,)*
        }

        impl PhysicalKey {
            /// The PC set-1 scancode and whether it is an extended (`0xE0`) code.
            pub fn scancode(self) -> (u8, bool) {
                match self {
                    $(PhysicalKey::$web => ($code, $ext),)*
                }
            }

            /// The W3C `KeyboardEvent.code` name.
            pub fn code(self) -> &'static str {
                match self {
                    $(PhysicalKey::$web => stringify!($web),)*
                }
            }

            /// Parse a `KeyboardEvent.code`.
            pub fn from_code(code: &str) -> Option<PhysicalKey> {
                match code {
                    $(stringify!($web) => Some(PhysicalKey::$web),)*
                    _ => None,
                }
            }

            /// Every key the device has a code for.
            pub const ALL: &'static [PhysicalKey] = &[$(PhysicalKey::$web,)*];
        }

        /// `None` if the device has no code for it.
        #[cfg(feature = "winit")]
        pub fn from_winit(key: winit::keyboard::KeyCode) -> Option<PhysicalKey> {
            match key {
                $(winit::keyboard::KeyCode::$winit => Some(PhysicalKey::$web),)*
                _ => None,
            }
        }
    };
}

// Vendor quirks the device expects: Pause is 0x45 unextended, Num Lock 0x45
// extended, Print Screen 0x37 extended. The keypad shares codes with the
// navigation block, distinguished by not being extended.
//
//   W3C code      winit KeyCode   scancode  extended
physical_keys! {
    Escape          Escape          0x01  false;
    Digit1          Digit1          0x02  false;
    Digit2          Digit2          0x03  false;
    Digit3          Digit3          0x04  false;
    Digit4          Digit4          0x05  false;
    Digit5          Digit5          0x06  false;
    Digit6          Digit6          0x07  false;
    Digit7          Digit7          0x08  false;
    Digit8          Digit8          0x09  false;
    Digit9          Digit9          0x0A  false;
    Digit0          Digit0          0x0B  false;
    Minus           Minus           0x0C  false;
    Equal           Equal           0x0D  false;
    Backspace       Backspace       0x0E  false;
    Tab             Tab             0x0F  false;
    KeyQ            KeyQ            0x10  false;
    KeyW            KeyW            0x11  false;
    KeyE            KeyE            0x12  false;
    KeyR            KeyR            0x13  false;
    KeyT            KeyT            0x14  false;
    KeyY            KeyY            0x15  false;
    KeyU            KeyU            0x16  false;
    KeyI            KeyI            0x17  false;
    KeyO            KeyO            0x18  false;
    KeyP            KeyP            0x19  false;
    BracketLeft     BracketLeft     0x1A  false;
    BracketRight    BracketRight    0x1B  false;
    Enter           Enter           0x1C  false;
    ControlLeft     ControlLeft     0x1D  false;
    KeyA            KeyA            0x1E  false;
    KeyS            KeyS            0x1F  false;
    KeyD            KeyD            0x20  false;
    KeyF            KeyF            0x21  false;
    KeyG            KeyG            0x22  false;
    KeyH            KeyH            0x23  false;
    KeyJ            KeyJ            0x24  false;
    KeyK            KeyK            0x25  false;
    KeyL            KeyL            0x26  false;
    Semicolon       Semicolon       0x27  false;
    Quote           Quote           0x28  false;
    Backquote       Backquote       0x29  false;
    ShiftLeft       ShiftLeft       0x2A  false;
    Backslash       Backslash       0x2B  false;
    KeyZ            KeyZ            0x2C  false;
    KeyX            KeyX            0x2D  false;
    KeyC            KeyC            0x2E  false;
    KeyV            KeyV            0x2F  false;
    KeyB            KeyB            0x30  false;
    KeyN            KeyN            0x31  false;
    KeyM            KeyM            0x32  false;
    Comma           Comma           0x33  false;
    Period          Period          0x34  false;
    Slash           Slash           0x35  false;
    ShiftRight      ShiftRight      0x36  false;
    NumpadMultiply  NumpadMultiply  0x37  false;
    AltLeft         AltLeft         0x38  false;
    Space           Space           0x39  false;
    CapsLock        CapsLock        0x3A  false;
    F1              F1              0x3B  false;
    F2              F2              0x3C  false;
    F3              F3              0x3D  false;
    F4              F4              0x3E  false;
    F5              F5              0x3F  false;
    F6              F6              0x40  false;
    F7              F7              0x41  false;
    F8              F8              0x42  false;
    F9              F9              0x43  false;
    F10             F10             0x44  false;
    Pause           Pause           0x45  false;
    NumLock         NumLock         0x45  true ;
    ScrollLock      ScrollLock      0x46  false;
    Numpad7         Numpad7         0x47  false;
    Numpad8         Numpad8         0x48  false;
    Numpad9         Numpad9         0x49  false;
    NumpadSubtract  NumpadSubtract  0x4A  false;
    Numpad4         Numpad4         0x4B  false;
    Numpad5         Numpad5         0x4C  false;
    Numpad6         Numpad6         0x4D  false;
    NumpadAdd       NumpadAdd       0x4E  false;
    Numpad1         Numpad1         0x4F  false;
    Numpad2         Numpad2         0x50  false;
    Numpad3         Numpad3         0x51  false;
    Numpad0         Numpad0         0x52  false;
    NumpadDecimal   NumpadDecimal   0x53  false;
    IntlBackslash   IntlBackslash   0x56  false;
    F11             F11             0x57  false;
    F12             F12             0x58  false;
    IntlYen         IntlYen         0x7D  false;
    NumpadEnter     NumpadEnter     0x1C  true ;
    ControlRight    ControlRight    0x1D  true ;
    NumpadDivide    NumpadDivide    0x35  true ;
    PrintScreen     PrintScreen     0x37  true ;
    AltRight        AltRight        0x38  true ;
    Home            Home            0x47  true ;
    ArrowUp         ArrowUp         0x48  true ;
    PageUp          PageUp          0x49  true ;
    ArrowLeft       ArrowLeft       0x4B  true ;
    ArrowRight      ArrowRight      0x4D  true ;
    End             End             0x4F  true ;
    ArrowDown       ArrowDown       0x50  true ;
    PageDown        PageDown        0x51  true ;
    Insert          Insert          0x52  true ;
    Delete          Delete          0x53  true ;
    MetaLeft        SuperLeft       0x5B  true ;
    MetaRight       SuperRight      0x5C  true ;
    ContextMenu     ContextMenu     0x5D  true ;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_round_trips_through_its_web_name() {
        for &key in PhysicalKey::ALL {
            assert_eq!(PhysicalKey::from_code(key.code()), Some(key), "{key:?}");
        }
    }

    #[test]
    fn the_device_has_a_code_for_every_key_we_name() {
        for &key in PhysicalKey::ALL {
            assert_ne!(key.scancode().0, 0, "{key:?}");
        }
    }

    #[test]
    fn pause_and_num_lock_share_a_code_and_differ_only_in_the_extended_flag() {
        assert_eq!(PhysicalKey::Pause.scancode(), (0x45, false));
        assert_eq!(PhysicalKey::NumLock.scancode(), (0x45, true));
        assert_eq!(PhysicalKey::PrintScreen.scancode(), (0x37, true));
    }

    #[test]
    fn the_keypad_is_the_unextended_half_of_the_navigation_codes() {
        assert_eq!(PhysicalKey::Numpad7.scancode(), (0x47, false));
        assert_eq!(PhysicalKey::Home.scancode(), (0x47, true));
        assert_eq!(PhysicalKey::Numpad0.scancode(), (0x52, false));
        assert_eq!(PhysicalKey::Insert.scancode(), (0x52, true));
    }

    #[test]
    fn the_windows_key_is_meta_on_the_web() {
        assert_eq!(
            PhysicalKey::from_code("MetaLeft"),
            Some(PhysicalKey::MetaLeft)
        );
        assert_eq!(PhysicalKey::from_code("SuperLeft"), None);
        assert_eq!(PhysicalKey::MetaLeft.code(), "MetaLeft");
    }

    #[test]
    fn a_key_the_device_has_no_code_for_is_refused() {
        assert_eq!(PhysicalKey::from_code("F13"), None);
        assert_eq!(PhysicalKey::from_code(""), None);
        assert_eq!(PhysicalKey::from_code("keya"), None);
    }

    #[cfg(feature = "winit")]
    #[test]
    fn winit_keys_land_on_the_same_scancodes() {
        use winit::keyboard::KeyCode;
        assert_eq!(from_winit(KeyCode::KeyA), Some(PhysicalKey::KeyA));
        assert_eq!(from_winit(KeyCode::SuperLeft), Some(PhysicalKey::MetaLeft));
        assert_eq!(
            from_winit(KeyCode::NumLock).map(PhysicalKey::scancode),
            Some((0x45, true))
        );
        // winit has keys the device does not.
        assert_eq!(from_winit(KeyCode::F20), None);
    }
}
