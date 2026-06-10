//! Color scheme and ANSI palette resolution.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

// Campbell (Windows Terminal default scheme), tweaked background.
pub const ANSI: [Rgb; 16] = [
    rgb(0x0C, 0x0C, 0x0C), // black
    rgb(0xC5, 0x0F, 0x1F), // red
    rgb(0x13, 0xA1, 0x0E), // green
    rgb(0xC1, 0x9C, 0x00), // yellow
    rgb(0x00, 0x37, 0xDA), // blue
    rgb(0x88, 0x17, 0x98), // magenta
    rgb(0x3A, 0x96, 0xDD), // cyan
    rgb(0xCC, 0xCC, 0xCC), // white
    rgb(0x76, 0x76, 0x76), // bright black
    rgb(0xE7, 0x48, 0x56), // bright red
    rgb(0x16, 0xC6, 0x0C), // bright green
    rgb(0xF9, 0xF1, 0xA5), // bright yellow
    rgb(0x3B, 0x78, 0xFF), // bright blue
    rgb(0xB4, 0x00, 0x9E), // bright magenta
    rgb(0x61, 0xD6, 0xD6), // bright cyan
    rgb(0xF2, 0xF2, 0xF2), // bright white
];

pub const DEFAULT_FG: Rgb = rgb(0xCC, 0xCC, 0xCC);
pub const DEFAULT_BG: Rgb = rgb(0x0C, 0x0C, 0x14);

// Chrome colors (tab bar, dividers, etc.).
pub const CHROME_BG: Rgb = rgb(0x16, 0x16, 0x20);
pub const TAB_ACTIVE: Rgb = rgb(0x0C, 0x0C, 0x14);
pub const TAB_INACTIVE: Rgb = rgb(0x1E, 0x1E, 0x2A);
pub const TAB_TEXT: Rgb = rgb(0xB8, 0xBC, 0xC8);
pub const TAB_TEXT_ACTIVE: Rgb = rgb(0xF0, 0xF0, 0xF4);
pub const ACCENT: Rgb = rgb(0x4F, 0x8F, 0xF7);
pub const DIVIDER: Rgb = rgb(0x2A, 0x2A, 0x38);
pub const TOOLBAR_BG: Rgb = rgb(0x1A, 0x1A, 0x26);

/// 256-color xterm palette entry for indices 16..=255.
pub fn indexed(idx: u8) -> Rgb {
    if idx < 16 {
        return ANSI[idx as usize];
    }
    if idx < 232 {
        let i = idx as u32 - 16;
        let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
        let c = |v: u32| if v == 0 { 0u8 } else { (55 + v * 40) as u8 };
        rgb(c(r), c(g), c(b))
    } else {
        let v = (8 + (idx as u32 - 232) * 10) as u8;
        rgb(v, v, v)
    }
}

/// Resolve a terminal color against runtime overrides (OSC 4/10/11) then defaults.
pub fn resolve(color: Color, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(c) => c,
        Color::Indexed(idx) => colors[idx as usize].unwrap_or_else(|| indexed(idx)),
        Color::Named(name) => named(name, colors),
    }
}

fn named(name: NamedColor, colors: &Colors) -> Rgb {
    if let Some(c) = colors[name as usize] {
        return c;
    }
    match name {
        NamedColor::Foreground => DEFAULT_FG,
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Cursor => DEFAULT_FG,
        NamedColor::BrightForeground => rgb(0xF2, 0xF2, 0xF2),
        NamedColor::DimForeground => rgb(0x80, 0x80, 0x80),
        NamedColor::Black => ANSI[0],
        NamedColor::Red => ANSI[1],
        NamedColor::Green => ANSI[2],
        NamedColor::Yellow => ANSI[3],
        NamedColor::Blue => ANSI[4],
        NamedColor::Magenta => ANSI[5],
        NamedColor::Cyan => ANSI[6],
        NamedColor::White => ANSI[7],
        NamedColor::BrightBlack => ANSI[8],
        NamedColor::BrightRed => ANSI[9],
        NamedColor::BrightGreen => ANSI[10],
        NamedColor::BrightYellow => ANSI[11],
        NamedColor::BrightBlue => ANSI[12],
        NamedColor::BrightMagenta => ANSI[13],
        NamedColor::BrightCyan => ANSI[14],
        NamedColor::BrightWhite => ANSI[15],
        NamedColor::DimBlack => dim(ANSI[0]),
        NamedColor::DimRed => dim(ANSI[1]),
        NamedColor::DimGreen => dim(ANSI[2]),
        NamedColor::DimYellow => dim(ANSI[3]),
        NamedColor::DimBlue => dim(ANSI[4]),
        NamedColor::DimMagenta => dim(ANSI[5]),
        NamedColor::DimCyan => dim(ANSI[6]),
        NamedColor::DimWhite => dim(ANSI[7]),
    }
}

pub fn dim(c: Rgb) -> Rgb {
    rgb((c.r as u32 * 2 / 3) as u8, (c.g as u32 * 2 / 3) as u8, (c.b as u32 * 2 / 3) as u8)
}

pub fn d2d(c: Rgb) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r as f32 / 255.0, g: c.g as f32 / 255.0, b: c.b as f32 / 255.0, a: 1.0 }
}

pub fn d2d_a(c: Rgb, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r as f32 / 255.0, g: c.g as f32 / 255.0, b: c.b as f32 / 255.0, a }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_low_16_match_ansi_table() {
        assert_eq!(indexed(0), ANSI[0]);
        assert_eq!(indexed(15), ANSI[15]);
    }

    #[test]
    fn indexed_cube_corners() {
        // 16 = cube (0,0,0); 231 = cube (5,5,5) = white.
        assert_eq!(indexed(16), rgb(0, 0, 0));
        assert_eq!(indexed(231), rgb(0xFF, 0xFF, 0xFF));
        // 196 = pure red (5,0,0).
        assert_eq!(indexed(196), rgb(0xFF, 0, 0));
    }

    #[test]
    fn indexed_grayscale_ramp() {
        assert_eq!(indexed(232), rgb(8, 8, 8));
        assert_eq!(indexed(255), rgb(238, 238, 238));
    }

    #[test]
    fn resolve_spec_passthrough_and_named_defaults() {
        let colors = Colors::default();
        let c = rgb(1, 2, 3);
        assert_eq!(resolve(Color::Spec(c), &colors), c);
        assert_eq!(resolve(Color::Named(NamedColor::Foreground), &colors), DEFAULT_FG);
        assert_eq!(resolve(Color::Named(NamedColor::Red), &colors), ANSI[1]);
        assert_eq!(resolve(Color::Indexed(196), &colors), rgb(0xFF, 0, 0));
    }
}
