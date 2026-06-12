//! Translate Win32 key events into VT input byte sequences (xterm-compatible).

use alacritty_terminal::term::TermMode;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Right-Alt while Ctrl is down = AltGr on international layouts; the
    /// combination types characters and must reach WM_CHAR untouched.
    pub altgr: bool,
}

impl Mods {
    pub fn current() -> Self {
        let down = |vk: VIRTUAL_KEY| unsafe { (GetKeyState(vk.0 as i32) as u16 & 0x8000) != 0 };
        Mods {
            shift: down(VK_SHIFT),
            ctrl: down(VK_CONTROL),
            alt: down(VK_MENU),
            altgr: down(VK_RMENU) && down(VK_CONTROL),
        }
    }

    fn xterm(&self) -> u8 {
        1 + (self.shift as u8) + ((self.alt as u8) << 1) + ((self.ctrl as u8) << 2)
    }

    fn any(&self) -> bool {
        self.shift || self.ctrl || self.alt
    }
}

/// Encode a non-character key (WM_KEYDOWN). Returns None when the key should
/// flow through to WM_CHAR for normal character translation.
///
/// When the foreground app has enabled the kitty keyboard protocol's
/// "disambiguate escape codes" mode (CSI > 1 u — Claude Code, neovim, fish…),
/// Esc and modified Enter/Tab/Backspace/Space and Ctrl/Alt+character keys are
/// encoded as CSI u so e.g. Shift+Enter (CSI 13;2u) is distinguishable from
/// Enter. alacritty_terminal tracks the mode stack; we only encode.
pub fn encode_key(vk: u16, mods: &Mods, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let kitty = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let m = mods.xterm();

    let csi_u = |code: u32| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[{};{}u", code, m).into_bytes()
        } else {
            format!("\x1b[{}u", code).into_bytes()
        }
    };

    // CSI 1;<m> <ch> when modified, else SS3/CSI plain.
    let cursor = |ch: u8| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[1;{}{}", m, ch as char).into_bytes()
        } else if app_cursor {
            vec![0x1b, b'O', ch]
        } else {
            vec![0x1b, b'[', ch]
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if mods.any() {
            format!("\x1b[{};{}~", n, m).into_bytes()
        } else {
            format!("\x1b[{}~", n).into_bytes()
        }
    };

    let vk = VIRTUAL_KEY(vk);
    Some(match vk {
        VK_UP => cursor(b'A'),
        VK_DOWN => cursor(b'B'),
        VK_RIGHT => cursor(b'C'),
        VK_LEFT => cursor(b'D'),
        VK_HOME => cursor(b'H'),
        VK_END => cursor(b'F'),
        VK_PRIOR => tilde(5),
        VK_NEXT => tilde(6),
        VK_INSERT => tilde(2),
        VK_DELETE => tilde(3),
        VK_RETURN => {
            if kitty && mods.any() {
                csi_u(13)
            } else if mods.alt || mods.shift {
                // Shift+Enter falls back to ESC CR — what Claude Code's
                // /terminal-setup configures elsewhere; zsh inserts a newline.
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        },
        VK_BACK => {
            if kitty && mods.any() {
                csi_u(127)
            } else {
                let b: u8 = if mods.ctrl { 0x08 } else { 0x7f };
                if mods.alt { vec![0x1b, b] } else { vec![b] }
            }
        },
        VK_TAB => {
            if kitty && mods.any() {
                csi_u(9)
            } else if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        },
        // Disambiguation is the point: a bare CSI 27u Esc can't be mistaken
        // for the start of an escape sequence.
        VK_ESCAPE if kitty => csi_u(27),
        VK_ESCAPE => vec![0x1b],
        VK_SPACE if kitty && (mods.ctrl || mods.alt) => csi_u(32),
        VK_SPACE if mods.ctrl => vec![0x00],
        VK_F1 | VK_F2 | VK_F3 | VK_F4 => {
            let ch = b'P' + (vk.0 - VK_F1.0) as u8;
            if mods.any() {
                format!("\x1b[1;{}{}", m, ch as char).into_bytes()
            } else {
                vec![0x1b, b'O', ch]
            }
        },
        VK_F5 => tilde(15),
        VK_F6 => tilde(17),
        VK_F7 => tilde(18),
        VK_F8 => tilde(19),
        VK_F9 => tilde(20),
        VK_F10 => tilde(21),
        VK_F11 => tilde(23),
        VK_F12 => tilde(24),
        // Ctrl/Alt + character key under kitty: CSI <unshifted-codepoint>;<m>u
        // instead of legacy control bytes / ESC prefix. AltGr combos still
        // type characters, so they fall through to WM_CHAR.
        _ if kitty && (mods.ctrl || mods.alt) && !mods.altgr => csi_u(base_char(vk.0)?),
        _ => return None,
    })
}

/// Unshifted codepoint of a character-producing key, lowercased per the kitty
/// spec (`a`, not `A`). None for keys with no base character (modifiers,
/// media keys…), which then flow through to WM_CHAR.
pub fn base_char(vk: u16) -> Option<u32> {
    match vk {
        v @ 0x41..=0x5A => Some(v as u32 + 32), // A-Z -> a-z
        v @ 0x30..=0x39 => Some(v as u32),      // 0-9
        _ => {
            let ch = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_CHAR) } & 0xFFFF;
            let c = char::from_u32(ch)?;
            if c == '\0' || c.is_control() {
                return None;
            }
            Some(c.to_lowercase().next().unwrap_or(c) as u32)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false, altgr: false };
    const SHIFT: Mods = Mods { shift: true, ctrl: false, alt: false, altgr: false };
    const CTRL: Mods = Mods { shift: false, ctrl: true, alt: false, altgr: false };
    const ALT: Mods = Mods { shift: false, ctrl: false, alt: true, altgr: false };
    const ALTGR: Mods = Mods { shift: false, ctrl: true, alt: true, altgr: true };

    const KITTY: TermMode = TermMode::DISAMBIGUATE_ESC_CODES;

    fn enc(vk: VIRTUAL_KEY, mods: &Mods, mode: TermMode) -> Vec<u8> {
        encode_key(vk.0, mods, mode).expect("should encode")
    }

    #[test]
    fn arrows_normal_and_application_mode() {
        assert_eq!(enc(VK_UP, &NONE, TermMode::empty()), b"\x1b[A");
        assert_eq!(enc(VK_UP, &NONE, TermMode::APP_CURSOR), b"\x1bOA");
        assert_eq!(enc(VK_LEFT, &NONE, TermMode::empty()), b"\x1b[D");
    }

    #[test]
    fn modified_arrows_use_xterm_encoding() {
        // Ctrl+Right = CSI 1;5C — used by shells for word-jump.
        assert_eq!(enc(VK_RIGHT, &CTRL, TermMode::empty()), b"\x1b[1;5C");
        assert_eq!(enc(VK_UP, &SHIFT, TermMode::empty()), b"\x1b[1;2A");
        // Modifiers win over application cursor mode.
        assert_eq!(enc(VK_RIGHT, &CTRL, TermMode::APP_CURSOR), b"\x1b[1;5C");
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(VK_F1, &NONE, TermMode::empty()), b"\x1bOP");
        assert_eq!(enc(VK_F5, &NONE, TermMode::empty()), b"\x1b[15~");
        assert_eq!(enc(VK_F12, &NONE, TermMode::empty()), b"\x1b[24~");
        assert_eq!(enc(VK_F5, &CTRL, TermMode::empty()), b"\x1b[15;5~");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(VK_DELETE, &NONE, TermMode::empty()), b"\x1b[3~");
        assert_eq!(enc(VK_PRIOR, &NONE, TermMode::empty()), b"\x1b[5~");
        assert_eq!(enc(VK_BACK, &NONE, TermMode::empty()), [0x7f]);
        assert_eq!(enc(VK_BACK, &CTRL, TermMode::empty()), [0x08]);
        assert_eq!(enc(VK_TAB, &SHIFT, TermMode::empty()), b"\x1b[Z");
        assert_eq!(enc(VK_RETURN, &ALT, TermMode::empty()), b"\x1b\r");
    }

    #[test]
    fn shift_enter_legacy_fallback_is_esc_cr() {
        // Without the kitty protocol, Shift+Enter still has to be
        // distinguishable for Claude Code: ESC CR, same as /terminal-setup.
        assert_eq!(enc(VK_RETURN, &SHIFT, TermMode::empty()), b"\x1b\r");
        // Plain Enter stays plain.
        assert_eq!(enc(VK_RETURN, &NONE, TermMode::empty()), b"\r");
    }

    #[test]
    fn kitty_disambiguates_modified_enter_tab_backspace() {
        assert_eq!(enc(VK_RETURN, &SHIFT, KITTY), b"\x1b[13;2u");
        assert_eq!(enc(VK_RETURN, &CTRL, KITTY), b"\x1b[13;5u");
        assert_eq!(enc(VK_RETURN, &NONE, KITTY), b"\r");
        assert_eq!(enc(VK_TAB, &SHIFT, KITTY), b"\x1b[9;2u");
        assert_eq!(enc(VK_TAB, &NONE, KITTY), b"\t");
        assert_eq!(enc(VK_BACK, &CTRL, KITTY), b"\x1b[127;5u");
        assert_eq!(enc(VK_BACK, &NONE, KITTY), [0x7f]);
    }

    #[test]
    fn kitty_escape_is_csi_27u() {
        assert_eq!(enc(VK_ESCAPE, &NONE, KITTY), b"\x1b[27u");
        assert_eq!(enc(VK_ESCAPE, &NONE, TermMode::empty()), [0x1b]);
    }

    #[test]
    fn kitty_ctrl_and_alt_characters_use_csi_u() {
        assert_eq!(enc(VIRTUAL_KEY(b'A' as u16), &CTRL, KITTY), b"\x1b[97;5u");
        assert_eq!(enc(VIRTUAL_KEY(b'A' as u16), &ALT, KITTY), b"\x1b[97;3u");
        assert_eq!(enc(VIRTUAL_KEY(b'5' as u16), &CTRL, KITTY), b"\x1b[53;5u");
        assert_eq!(enc(VK_SPACE, &CTRL, KITTY), b"\x1b[32;5u");
        // Legacy mode unchanged: ctrl+letter flows to WM_CHAR (control byte).
        assert!(encode_key(b'A' as u16, &CTRL, TermMode::empty()).is_none());
        // Plain and shifted text keys stay text even under kitty.
        assert!(encode_key(b'A' as u16, &NONE, KITTY).is_none());
        assert!(encode_key(b'A' as u16, &SHIFT, KITTY).is_none());
        // AltGr types characters; never steal it.
        assert!(encode_key(b'A' as u16, &ALTGR, KITTY).is_none());
    }

    #[test]
    fn plain_characters_fall_through_to_wm_char() {
        assert!(encode_key(b'A' as u16, &NONE, TermMode::empty()).is_none());
        assert!(encode_key(b'5' as u16, &SHIFT, TermMode::empty()).is_none());
    }
}
