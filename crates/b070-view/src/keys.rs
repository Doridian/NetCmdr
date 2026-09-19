//! Physical key to PC scancode (set 1) mapping.
//!
//! The device replays scancodes into the target's keyboard controller, so what
//! it wants is the *physical* key, not the character it produces — the target
//! applies its own layout. `winit`'s `KeyCode` is already physical and named
//! after the US layout position, which is exactly the set-1 code space.
//!
//! Checked against the vendor client's own table (`JVKEYTOSCAN` in
//! `com.minicom.plugin.km_impl`), including its two oddities: Pause sends 0x45 unextended while Num Lock sends 0x45
//! extended, and Print Screen sends 0x37 extended.

use winit::keyboard::KeyCode;

/// Map a physical key to `(scancode, extended)`, or `None` if the device has no
/// code for it.
pub fn scancode(code: KeyCode) -> Option<(u8, bool)> {
    use KeyCode::*;
    let plain = |c: u8| Some((c, false));
    let ext = |c: u8| Some((c, true));
    match code {
        Escape => plain(0x01),
        Digit1 => plain(0x02),
        Digit2 => plain(0x03),
        Digit3 => plain(0x04),
        Digit4 => plain(0x05),
        Digit5 => plain(0x06),
        Digit6 => plain(0x07),
        Digit7 => plain(0x08),
        Digit8 => plain(0x09),
        Digit9 => plain(0x0A),
        Digit0 => plain(0x0B),
        Minus => plain(0x0C),
        Equal => plain(0x0D),
        Backspace => plain(0x0E),
        Tab => plain(0x0F),
        KeyQ => plain(0x10),
        KeyW => plain(0x11),
        KeyE => plain(0x12),
        KeyR => plain(0x13),
        KeyT => plain(0x14),
        KeyY => plain(0x15),
        KeyU => plain(0x16),
        KeyI => plain(0x17),
        KeyO => plain(0x18),
        KeyP => plain(0x19),
        BracketLeft => plain(0x1A),
        BracketRight => plain(0x1B),
        Enter => plain(0x1C),
        ControlLeft => plain(0x1D),
        KeyA => plain(0x1E),
        KeyS => plain(0x1F),
        KeyD => plain(0x20),
        KeyF => plain(0x21),
        KeyG => plain(0x22),
        KeyH => plain(0x23),
        KeyJ => plain(0x24),
        KeyK => plain(0x25),
        KeyL => plain(0x26),
        Semicolon => plain(0x27),
        Quote => plain(0x28),
        Backquote => plain(0x29),
        ShiftLeft => plain(0x2A),
        Backslash => plain(0x2B),
        KeyZ => plain(0x2C),
        KeyX => plain(0x2D),
        KeyC => plain(0x2E),
        KeyV => plain(0x2F),
        KeyB => plain(0x30),
        KeyN => plain(0x31),
        KeyM => plain(0x32),
        Comma => plain(0x33),
        Period => plain(0x34),
        Slash => plain(0x35),
        ShiftRight => plain(0x36),
        NumpadMultiply => plain(0x37),
        AltLeft => plain(0x38),
        Space => plain(0x39),
        CapsLock => plain(0x3A),
        F1 => plain(0x3B),
        F2 => plain(0x3C),
        F3 => plain(0x3D),
        F4 => plain(0x3E),
        F5 => plain(0x3F),
        F6 => plain(0x40),
        F7 => plain(0x41),
        F8 => plain(0x42),
        F9 => plain(0x43),
        F10 => plain(0x44),
        // The vendor sends Pause unextended and Num Lock extended, both 0x45.
        Pause => plain(0x45),
        NumLock => ext(0x45),
        ScrollLock => plain(0x46),
        Numpad7 => plain(0x47),
        Numpad8 => plain(0x48),
        Numpad9 => plain(0x49),
        NumpadSubtract => plain(0x4A),
        Numpad4 => plain(0x4B),
        Numpad5 => plain(0x4C),
        Numpad6 => plain(0x4D),
        NumpadAdd => plain(0x4E),
        Numpad1 => plain(0x4F),
        Numpad2 => plain(0x50),
        Numpad3 => plain(0x51),
        Numpad0 => plain(0x52),
        NumpadDecimal => plain(0x53),
        IntlBackslash => plain(0x56),
        F11 => plain(0x57),
        F12 => plain(0x58),
        IntlYen => plain(0x7D),

        NumpadEnter => ext(0x1C),
        ControlRight => ext(0x1D),
        NumpadDivide => ext(0x35),
        PrintScreen => ext(0x37),
        AltRight => ext(0x38),
        Home => ext(0x47),
        ArrowUp => ext(0x48),
        PageUp => ext(0x49),
        ArrowLeft => ext(0x4B),
        ArrowRight => ext(0x4D),
        End => ext(0x4F),
        ArrowDown => ext(0x50),
        PageDown => ext(0x51),
        Insert => ext(0x52),
        Delete => ext(0x53),
        SuperLeft => ext(0x5B),
        SuperRight => ext(0x5C),
        ContextMenu => ext(0x5D),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_match_set_one() {
        assert_eq!(scancode(KeyCode::KeyA), Some((0x1E, false)));
        assert_eq!(scancode(KeyCode::KeyZ), Some((0x2C, false)));
    }

    #[test]
    fn navigation_keys_are_extended() {
        for key in [
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Insert,
            KeyCode::Delete,
            KeyCode::ArrowUp,
            KeyCode::PageDown,
        ] {
            assert_eq!(scancode(key).map(|(_, e)| e), Some(true), "{key:?}");
        }
    }

    /// The numeric keypad shares codes with the navigation block, but is not
    /// extended — that flag is the only thing telling them apart.
    #[test]
    fn the_keypad_shares_codes_without_the_extended_flag() {
        assert_eq!(scancode(KeyCode::Numpad7), Some((0x47, false)));
        assert_eq!(scancode(KeyCode::Home), Some((0x47, true)));
        assert_eq!(scancode(KeyCode::Numpad0), Some((0x52, false)));
        assert_eq!(scancode(KeyCode::Insert), Some((0x52, true)));
    }

    #[test]
    fn pause_and_num_lock_differ_only_by_the_extended_flag() {
        assert_eq!(scancode(KeyCode::Pause), Some((0x45, false)));
        assert_eq!(scancode(KeyCode::NumLock), Some((0x45, true)));
    }

    #[test]
    fn left_and_right_modifiers_are_distinct() {
        assert_eq!(scancode(KeyCode::ControlLeft), Some((0x1D, false)));
        assert_eq!(scancode(KeyCode::ControlRight), Some((0x1D, true)));
        assert_eq!(scancode(KeyCode::ShiftLeft), Some((0x2A, false)));
        assert_eq!(scancode(KeyCode::ShiftRight), Some((0x36, false)));
    }

    #[test]
    fn unmapped_keys_are_dropped_rather_than_guessed() {
        assert_eq!(scancode(KeyCode::F20), None);
        assert_eq!(scancode(KeyCode::AudioVolumeUp), None);
    }
}
