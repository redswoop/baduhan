//! Translate Win32 key events into VT input byte sequences (xterm-compatible).

use alacritty_terminal::term::TermMode;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Mods {
    pub fn current() -> Self {
        let down = |vk: VIRTUAL_KEY| unsafe { (GetKeyState(vk.0 as i32) as u16 & 0x8000) != 0 };
        Mods { shift: down(VK_SHIFT), ctrl: down(VK_CONTROL), alt: down(VK_MENU) }
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
pub fn encode_key(vk: u16, mods: &Mods, mode: TermMode) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let m = mods.xterm();

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
            if mods.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        },
        VK_BACK => {
            let b: u8 = if mods.ctrl { 0x08 } else { 0x7f };
            if mods.alt { vec![0x1b, b] } else { vec![b] }
        },
        VK_TAB => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        },
        VK_ESCAPE => vec![0x1b],
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false };
    const SHIFT: Mods = Mods { shift: true, ctrl: false, alt: false };
    const CTRL: Mods = Mods { shift: false, ctrl: true, alt: false };
    const ALT: Mods = Mods { shift: false, ctrl: false, alt: true };

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
    fn plain_characters_fall_through_to_wm_char() {
        assert!(encode_key(b'A' as u16, &NONE, TermMode::empty()).is_none());
        assert!(encode_key(b'5' as u16, &SHIFT, TermMode::empty()).is_none());
    }
}
