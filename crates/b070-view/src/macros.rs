// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Key combinations the local machine eats before we ever see them.
//!
//! A KVM viewer's keyboard menu exists because a window manager claims
//! Ctrl+Alt+Del, Alt+Tab, Super+L and friends for itself: pressing them affects
//! the machine you are sitting at, never the target. Picking one here
//! synthesises the same scancode sequence and sends it down the session's
//! keyboard channel instead.
//!
//! Each macro is a list of *physical* keys. They are pressed in order and
//! released in reverse, which is what a real keyboard does and what the target's
//! keyboard controller expects — modifiers must still be held when the final key
//! arrives.
//!
//! Ctrl+Alt+Delete is deliberately *not* in this table: on a PS/2 target it has
//! to be injected out of band, so it has its own message
//! ([`b070_session::Input::CtrlAltDel`]) and its own menu entry.

use winit::keyboard::KeyCode;

/// One named key sequence.
pub struct KeyMacro {
    /// What the menu shows.
    pub label: &'static str,
    /// Physical keys, pressed in order and released in reverse.
    pub keys: &'static [KeyCode],
}

/// A titled run of macros; the menu draws a separator between groups.
pub struct Group {
    pub title: &'static str,
    pub macros: &'static [KeyMacro],
}

/// Everything the keyboard menu offers, grouped as it is drawn.
pub const GROUPS: &[Group] = &[
    Group {
        title: "Task switching",
        macros: &[
            KeyMacro {
                label: "Alt+Tab",
                keys: &[KeyCode::AltLeft, KeyCode::Tab],
            },
            KeyMacro {
                label: "Alt+Shift+Tab",
                keys: &[KeyCode::AltLeft, KeyCode::ShiftLeft, KeyCode::Tab],
            },
            KeyMacro {
                label: "Ctrl+Shift+Esc  (Task Manager)",
                keys: &[KeyCode::ControlLeft, KeyCode::ShiftLeft, KeyCode::Escape],
            },
            KeyMacro {
                label: "Ctrl+Esc  (Start menu)",
                keys: &[KeyCode::ControlLeft, KeyCode::Escape],
            },
        ],
    },
    Group {
        title: "Windows",
        macros: &[
            KeyMacro {
                label: "Super",
                keys: &[KeyCode::SuperLeft],
            },
            KeyMacro {
                label: "Super+D  (show desktop)",
                keys: &[KeyCode::SuperLeft, KeyCode::KeyD],
            },
            KeyMacro {
                label: "Super+E  (file manager)",
                keys: &[KeyCode::SuperLeft, KeyCode::KeyE],
            },
            KeyMacro {
                label: "Super+L  (lock)",
                keys: &[KeyCode::SuperLeft, KeyCode::KeyL],
            },
            KeyMacro {
                label: "Super+R  (run)",
                keys: &[KeyCode::SuperLeft, KeyCode::KeyR],
            },
        ],
    },
    Group {
        title: "Window control",
        macros: &[
            KeyMacro {
                label: "Alt+F4  (close)",
                keys: &[KeyCode::AltLeft, KeyCode::F4],
            },
            KeyMacro {
                label: "Alt+Space  (window menu)",
                keys: &[KeyCode::AltLeft, KeyCode::Space],
            },
            KeyMacro {
                label: "Alt+Enter",
                keys: &[KeyCode::AltLeft, KeyCode::Enter],
            },
        ],
    },
    Group {
        title: "Capture",
        macros: &[
            KeyMacro {
                label: "Print Screen",
                keys: &[KeyCode::PrintScreen],
            },
            KeyMacro {
                label: "Alt+Print Screen  (active window)",
                keys: &[KeyCode::AltLeft, KeyCode::PrintScreen],
            },
        ],
    },
    Group {
        // The console switch a Linux target expects; X grabs it locally.
        title: "Linux consoles",
        macros: &[
            KeyMacro {
                label: "Ctrl+Alt+F1",
                keys: &[KeyCode::ControlLeft, KeyCode::AltLeft, KeyCode::F1],
            },
            KeyMacro {
                label: "Ctrl+Alt+F2",
                keys: &[KeyCode::ControlLeft, KeyCode::AltLeft, KeyCode::F2],
            },
            KeyMacro {
                label: "Ctrl+Alt+F7",
                keys: &[KeyCode::ControlLeft, KeyCode::AltLeft, KeyCode::F7],
            },
        ],
    },
];

/// Flat index over [`GROUPS`], which is how the menu identifies a pick.
pub fn get(index: usize) -> Option<&'static KeyMacro> {
    GROUPS.iter().flat_map(|g| g.macros.iter()).nth(index)
}

/// The flat index of the first macro in group `group`.
pub fn group_offset(group: usize) -> usize {
    GROUPS[..group].iter().map(|g| g.macros.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_indices_address_every_macro_exactly_once() {
        let total: usize = GROUPS.iter().map(|g| g.macros.len()).sum();
        let labels: Vec<_> = (0..total).map(|i| get(i).unwrap().label).collect();
        assert_eq!(labels.len(), total);
        assert_eq!(labels[0], GROUPS[0].macros[0].label);
        assert_eq!(*labels.last().unwrap(), {
            let g = GROUPS.last().unwrap();
            g.macros.last().unwrap().label
        });
        assert!(get(total).is_none());
    }

    #[test]
    fn group_offsets_line_up_with_the_flat_index() {
        for (i, group) in GROUPS.iter().enumerate() {
            let base = group_offset(i);
            for (j, m) in group.macros.iter().enumerate() {
                assert_eq!(get(base + j).unwrap().label, m.label);
            }
        }
    }

    /// Every key has to survive the scancode table, or the macro sends nothing.
    #[test]
    fn every_key_has_a_scancode() {
        for group in GROUPS {
            for m in group.macros {
                for key in m.keys {
                    assert!(
                        crate::keys::scancode(*key).is_some(),
                        "{}: no scancode for {key:?}",
                        m.label
                    );
                }
            }
        }
    }

    #[test]
    fn modifiers_come_before_the_key_they_modify() {
        // Releasing in reverse only works if the modifier is listed first.
        for group in GROUPS {
            for m in group.macros {
                let modifier = |k: &KeyCode| {
                    matches!(
                        k,
                        KeyCode::ControlLeft
                            | KeyCode::ControlRight
                            | KeyCode::AltLeft
                            | KeyCode::AltRight
                            | KeyCode::ShiftLeft
                            | KeyCode::ShiftRight
                            | KeyCode::SuperLeft
                            | KeyCode::SuperRight
                    )
                };
                let first_plain = m.keys.iter().position(|k| !modifier(k));
                if let Some(i) = first_plain {
                    assert!(
                        m.keys[i..].iter().all(|k| !modifier(k)),
                        "{}: modifier after a normal key",
                        m.label
                    );
                }
            }
        }
    }
}
