// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Key combinations the local OS would intercept (Alt+Tab, Super+L, …), sent
//! to the target from the keyboard menu.
//!
//! Keys are pressed in order and released in reverse, so modifiers are held.
//!
//! Ctrl+Alt+Delete isn't here; it's out of band
//! ([`netcmdr_session::Input::CtrlAltDel`]).

use crate::key::PhysicalKey;

pub struct KeyMacro {
    pub label: &'static str,
    /// Physical keys, pressed in order and released in reverse.
    pub keys: &'static [PhysicalKey],
}

/// A titled group of macros, separated in the menu.
pub struct Group {
    pub title: &'static str,
    pub macros: &'static [KeyMacro],
}

pub const GROUPS: &[Group] = &[
    Group {
        title: "Task switching",
        macros: &[
            KeyMacro {
                label: "Alt+Tab",
                keys: &[PhysicalKey::AltLeft, PhysicalKey::Tab],
            },
            KeyMacro {
                label: "Alt+Shift+Tab",
                keys: &[
                    PhysicalKey::AltLeft,
                    PhysicalKey::ShiftLeft,
                    PhysicalKey::Tab,
                ],
            },
            KeyMacro {
                label: "Ctrl+Shift+Esc  (Task Manager)",
                keys: &[
                    PhysicalKey::ControlLeft,
                    PhysicalKey::ShiftLeft,
                    PhysicalKey::Escape,
                ],
            },
            KeyMacro {
                label: "Ctrl+Esc  (Start menu)",
                keys: &[PhysicalKey::ControlLeft, PhysicalKey::Escape],
            },
        ],
    },
    Group {
        title: "Windows",
        macros: &[
            KeyMacro {
                label: "Super",
                keys: &[PhysicalKey::MetaLeft],
            },
            KeyMacro {
                label: "Super+D  (show desktop)",
                keys: &[PhysicalKey::MetaLeft, PhysicalKey::KeyD],
            },
            KeyMacro {
                label: "Super+E  (file manager)",
                keys: &[PhysicalKey::MetaLeft, PhysicalKey::KeyE],
            },
            KeyMacro {
                label: "Super+L  (lock)",
                keys: &[PhysicalKey::MetaLeft, PhysicalKey::KeyL],
            },
            KeyMacro {
                label: "Super+R  (run)",
                keys: &[PhysicalKey::MetaLeft, PhysicalKey::KeyR],
            },
        ],
    },
    Group {
        title: "Window control",
        macros: &[
            KeyMacro {
                label: "Alt+F4  (close)",
                keys: &[PhysicalKey::AltLeft, PhysicalKey::F4],
            },
            KeyMacro {
                label: "Alt+Space  (window menu)",
                keys: &[PhysicalKey::AltLeft, PhysicalKey::Space],
            },
            KeyMacro {
                label: "Alt+Enter",
                keys: &[PhysicalKey::AltLeft, PhysicalKey::Enter],
            },
        ],
    },
    Group {
        title: "Capture",
        macros: &[
            KeyMacro {
                label: "Print Screen",
                keys: &[PhysicalKey::PrintScreen],
            },
            KeyMacro {
                label: "Alt+Print Screen  (active window)",
                keys: &[PhysicalKey::AltLeft, PhysicalKey::PrintScreen],
            },
        ],
    },
    Group {
        // Linux VT switch; X grabs it locally.
        title: "Linux consoles",
        macros: &[
            KeyMacro {
                label: "Ctrl+Alt+F1",
                keys: &[
                    PhysicalKey::ControlLeft,
                    PhysicalKey::AltLeft,
                    PhysicalKey::F1,
                ],
            },
            KeyMacro {
                label: "Ctrl+Alt+F2",
                keys: &[
                    PhysicalKey::ControlLeft,
                    PhysicalKey::AltLeft,
                    PhysicalKey::F2,
                ],
            },
            KeyMacro {
                label: "Ctrl+Alt+F7",
                keys: &[
                    PhysicalKey::ControlLeft,
                    PhysicalKey::AltLeft,
                    PhysicalKey::F7,
                ],
            },
        ],
    },
];

/// Flat index over [`GROUPS`].
pub fn get(index: usize) -> Option<&'static KeyMacro> {
    GROUPS.iter().flat_map(|g| g.macros.iter()).nth(index)
}

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

    #[test]
    fn every_key_has_a_scancode() {
        for group in GROUPS {
            for m in group.macros {
                for key in m.keys {
                    assert_ne!(key.scancode().0, 0, "{}: no scancode for {key:?}", m.label);
                }
            }
        }
    }

    #[test]
    fn modifiers_come_before_the_key_they_modify() {
        // Modifiers must come first.
        for group in GROUPS {
            for m in group.macros {
                let modifier = |k: &PhysicalKey| {
                    matches!(
                        k,
                        PhysicalKey::ControlLeft
                            | PhysicalKey::ControlRight
                            | PhysicalKey::AltLeft
                            | PhysicalKey::AltRight
                            | PhysicalKey::ShiftLeft
                            | PhysicalKey::ShiftRight
                            | PhysicalKey::MetaLeft
                            | PhysicalKey::MetaRight
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
