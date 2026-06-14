//! VT emulation conformance battery: feeds escape sequences through the same
//! Processor + Term pipeline the live panes use and asserts on grid state,
//! modes, and emitted events. Pins our integration against alacritty_terminal
//! upgrades and documents exactly which VT features baduhan supports.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{test::TermSize, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<Event>>>);

impl EventListener for Capture {
    fn send_event(&self, e: Event) {
        self.0.lock().unwrap().push(e);
    }
}

struct Vt {
    term: Term<Capture>,
    proc: Processor,
    events: Arc<Mutex<Vec<Event>>>,
}

impl Vt {
    fn new(cols: usize, rows: usize) -> Vt {
        let events = Arc::new(Mutex::new(Vec::new()));
        let term = Term::new(
            // Mirror the live pane config (term_pane.rs): kitty keyboard
            // protocol negotiation on.
            Config { kitty_keyboard: true, ..Config::default() },
            &TermSize::new(cols, rows),
            Capture(events.clone()),
        );
        Vt { term, proc: Processor::new(), events }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.proc.advance(&mut self.term, bytes);
    }

    fn row(&self, y: usize) -> String {
        let grid = self.term.grid();
        let mut s = String::new();
        for x in 0..grid.columns() {
            let cell = &grid[Line(y as i32)][Column(x)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            s.push(cell.c);
        }
        s.trim_end().to_string()
    }

    fn cell(&self, y: usize, x: usize) -> &alacritty_terminal::term::cell::Cell {
        &self.term.grid()[Line(y as i32)][Column(x)]
    }

    fn cursor(&self) -> (usize, usize) {
        let p: Point = self.term.grid().cursor.point;
        (p.line.0 as usize, p.column.0)
    }

    fn pty_output(&self) -> String {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::PtyWrite(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }
}

// ----- text, cursor, wrapping -----------------------------------------------

#[test]
fn plain_text_and_autowrap() {
    let mut vt = Vt::new(10, 4);
    vt.feed(b"hello");
    assert_eq!(vt.row(0), "hello");
    vt.feed(b" worlds!!"); // 14 chars total: wraps onto line 1
    assert_eq!(vt.row(0), "hello worl");
    assert_eq!(vt.row(1), "ds!!");
}

#[test]
fn autowrap_disabled_clamps_to_last_column() {
    let mut vt = Vt::new(10, 4);
    vt.feed(b"\x1b[?7l0123456789ABC\x1b[?7h");
    assert_eq!(vt.row(0), "012345678C");
    assert_eq!(vt.row(1), "");
}

#[test]
fn cursor_movement_cup_cuu_cud_cuf_cub() {
    let mut vt = Vt::new(20, 10);
    vt.feed(b"\x1b[5;9H"); // 1-based row 5, col 9
    assert_eq!(vt.cursor(), (4, 8));
    vt.feed(b"\x1b[2A\x1b[3C\x1b[1B\x1b[4D");
    assert_eq!(vt.cursor(), (3, 7));
    vt.feed(b"\x1b[H");
    assert_eq!(vt.cursor(), (0, 0));
}

#[test]
fn save_restore_cursor_decsc_decrc() {
    let mut vt = Vt::new(20, 5);
    vt.feed(b"\x1b[3;4H\x1b7\x1b[H");
    assert_eq!(vt.cursor(), (0, 0));
    vt.feed(b"\x1b8");
    assert_eq!(vt.cursor(), (2, 3));
}

#[test]
fn horizontal_tabs_default_stops() {
    let mut vt = Vt::new(40, 4);
    vt.feed(b"\tX");
    assert_eq!(vt.cell(0, 8).c, 'X');
    vt.feed(b"\tY"); // next stop at 16
    assert_eq!(vt.cell(0, 16).c, 'Y');
}

// ----- erase / edit ----------------------------------------------------------

#[test]
fn erase_display_and_line() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"aaaaa\r\nbbbbb\r\nccccc");
    vt.feed(b"\x1b[H\x1b[K"); // EL to end of line 0
    assert_eq!(vt.row(0), "");
    assert_eq!(vt.row(1), "bbbbb");
    vt.feed(b"\x1b[2J"); // ED all
    assert_eq!(vt.row(1), "");
    assert_eq!(vt.row(2), "");
}

#[test]
fn insert_delete_lines_and_chars() {
    let mut vt = Vt::new(10, 4);
    vt.feed(b"one\r\ntwo\r\nthree");
    vt.feed(b"\x1b[1;1H\x1b[1L"); // IL: blank line pushed in at top
    assert_eq!(vt.row(0), "");
    assert_eq!(vt.row(1), "one");
    vt.feed(b"\x1b[1M"); // DL: and gone again
    assert_eq!(vt.row(0), "one");
    vt.feed(b"\x1b[1;1H\x1b[2@"); // ICH: shift right 2
    assert_eq!(vt.row(0), "  one");
    vt.feed(b"\x1b[2P"); // DCH: shift back
    assert_eq!(vt.row(0), "one");
}

#[test]
fn erase_and_repeat_characters() {
    let mut vt = Vt::new(10, 2);
    vt.feed(b"abcdef\x1b[1;2H\x1b[3X"); // ECH: blank b,c,d in place
    assert_eq!(vt.row(0), "a   ef");
    let mut vt = Vt::new(10, 2);
    vt.feed(b"x\x1b[4b"); // REP: repeat previous char 4 times
    assert_eq!(vt.row(0), "xxxxx");
}

#[test]
fn full_reset_ris() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"junk\x1b[?1049h\x1b[?25l");
    vt.feed(b"\x1bc");
    assert_eq!(vt.row(0), "");
    assert!(vt.term.mode().contains(TermMode::SHOW_CURSOR));
    assert!(!vt.term.mode().contains(TermMode::ALT_SCREEN));
}

// ----- scroll regions --------------------------------------------------------

#[test]
fn scroll_region_decstbm() {
    let mut vt = Vt::new(10, 5);
    vt.feed(b"AA\r\nBB\r\nCC\r\nDD\r\nEE");
    // Region rows 2..4 (1-based); cursor to region bottom, then LF scrolls
    // only the region.
    vt.feed(b"\x1b[2;4r\x1b[4;1H\n");
    assert_eq!(vt.row(0), "AA"); // outside: untouched
    assert_eq!(vt.row(1), "CC"); // scrolled up
    assert_eq!(vt.row(2), "DD");
    assert_eq!(vt.row(3), ""); // fresh line
    assert_eq!(vt.row(4), "EE"); // outside: untouched
}

#[test]
fn origin_mode_homes_to_region() {
    let mut vt = Vt::new(10, 6);
    vt.feed(b"\x1b[3;5r\x1b[?6h\x1b[H");
    assert_eq!(vt.cursor(), (2, 0)); // home = region top in DECOM
    vt.feed(b"\x1b[?6l\x1b[r");
}

// ----- SGR attributes --------------------------------------------------------

#[test]
fn sgr_basic_attributes() {
    let mut vt = Vt::new(20, 2);
    vt.feed(b"\x1b[1;3;4;9;7;2mZ\x1b[0my");
    let f = vt.cell(0, 0).flags;
    assert!(f.contains(Flags::BOLD));
    assert!(f.contains(Flags::ITALIC));
    assert!(f.contains(Flags::UNDERLINE));
    assert!(f.contains(Flags::STRIKEOUT));
    assert!(f.contains(Flags::INVERSE));
    assert!(f.contains(Flags::DIM));
    assert!(vt.cell(0, 1).flags.is_empty());
}

#[test]
fn sgr_16_and_256_color() {
    let mut vt = Vt::new(20, 2);
    vt.feed(b"\x1b[31mr\x1b[94mb\x1b[38;5;196mx\x1b[48;5;46my\x1b[m");
    assert_eq!(vt.cell(0, 0).fg, Color::Named(NamedColor::Red));
    assert_eq!(vt.cell(0, 1).fg, Color::Named(NamedColor::BrightBlue));
    assert_eq!(vt.cell(0, 2).fg, Color::Indexed(196));
    assert_eq!(vt.cell(0, 3).bg, Color::Indexed(46));
}

#[test]
fn sgr_truecolor() {
    let mut vt = Vt::new(20, 2);
    vt.feed(b"\x1b[38;2;12;34;56m\x1b[48;2;200;100;50mQ\x1b[m");
    assert_eq!(vt.cell(0, 0).fg, Color::Spec(Rgb { r: 12, g: 34, b: 56 }));
    assert_eq!(vt.cell(0, 0).bg, Color::Spec(Rgb { r: 200, g: 100, b: 50 }));
}

#[test]
fn sgr_underline_styles_and_color() {
    let mut vt = Vt::new(20, 2);
    vt.feed(b"\x1b[4:2mD\x1b[4:3mC\x1b[4:4md\x1b[4:5mo\x1b[0m");
    assert!(vt.cell(0, 0).flags.contains(Flags::DOUBLE_UNDERLINE));
    assert!(vt.cell(0, 1).flags.contains(Flags::UNDERCURL));
    assert!(vt.cell(0, 2).flags.contains(Flags::DOTTED_UNDERLINE));
    assert!(vt.cell(0, 3).flags.contains(Flags::DASHED_UNDERLINE));

    // SGR 58: colored underline (undercurl spell-check style).
    let mut vt = Vt::new(20, 2);
    vt.feed(b"\x1b[4:3m\x1b[58;2;255;0;0mE\x1b[59;4:0m");
    assert_eq!(
        vt.cell(0, 0).underline_color(),
        Some(Color::Spec(Rgb { r: 255, g: 0, b: 0 }))
    );
}

// ----- alt screen ------------------------------------------------------------

#[test]
fn alt_screen_1049_preserves_primary() {
    let mut vt = Vt::new(20, 5);
    vt.feed(b"primary content");
    vt.feed(b"\x1b[?1049h");
    assert!(vt.term.mode().contains(TermMode::ALT_SCREEN));
    assert_eq!(vt.row(0), ""); // alt starts clean
    vt.feed(b"alt stuff");
    vt.feed(b"\x1b[?1049l");
    assert!(!vt.term.mode().contains(TermMode::ALT_SCREEN));
    assert_eq!(vt.row(0), "primary content");
}

#[test]
fn reset_terminal_unsticks_alt_screen_and_mouse() {
    use alacritty_terminal::grid::Scroll;
    let mut vt = Vt::new(20, 5);
    for i in 0..40 {
        vt.feed(format!("line{i}\r\n").as_bytes());
    }
    // A full-screen app wedged the terminal: alt screen + mouse reporting on,
    // never reset (killed / Ctrl+Z-suspended). Scroll is now unreachable.
    vt.feed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");
    assert!(vt.term.mode().contains(TermMode::ALT_SCREEN));
    assert!(vt.term.mode().intersects(TermMode::MOUSE_MODE));
    vt.term.scroll_display(Scroll::Delta(5));
    assert_eq!(vt.term.grid().display_offset(), 0, "alt screen has no scrollback");

    // "Reset Terminal" replays the exact bytes window.rs sends.
    vt.feed(crate::window::TERM_RESET_SEQ);
    assert!(!vt.term.mode().contains(TermMode::ALT_SCREEN));
    assert!(!vt.term.mode().intersects(TermMode::MOUSE_MODE));
    // Scrollback is reachable again.
    vt.term.scroll_display(Scroll::Delta(5));
    assert!(vt.term.grid().display_offset() > 0, "scrollback restored after reset");
}

// ----- modes -----------------------------------------------------------------

#[test]
fn private_modes_set_term_mode_flags() {
    let mut vt = Vt::new(10, 3);
    let cases: &[(&[u8], TermMode)] = &[
        (b"\x1b[?1h", TermMode::APP_CURSOR),
        (b"\x1b[?1000h", TermMode::MOUSE_REPORT_CLICK),
        (b"\x1b[?1002h", TermMode::MOUSE_DRAG),
        (b"\x1b[?1003h", TermMode::MOUSE_MOTION),
        (b"\x1b[?1004h", TermMode::FOCUS_IN_OUT),
        (b"\x1b[?1006h", TermMode::SGR_MOUSE),
        (b"\x1b[?2004h", TermMode::BRACKETED_PASTE),
    ];
    for (seq, flag) in cases {
        vt.feed(seq);
        assert!(vt.term.mode().contains(*flag), "set {flag:?}");
    }
    // And back off.
    vt.feed(b"\x1b[?1l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2004l");
    for (_, flag) in cases {
        assert!(!vt.term.mode().contains(*flag), "clear {flag:?}");
    }
}

#[test]
fn cursor_visibility_and_shape_decscusr() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"\x1b[?25l");
    assert!(!vt.term.mode().contains(TermMode::SHOW_CURSOR));
    vt.feed(b"\x1b[?25h");
    assert!(vt.term.mode().contains(TermMode::SHOW_CURSOR));

    vt.feed(b"\x1b[5 q"); // blinking bar
    assert_eq!(vt.term.cursor_style().shape, CursorShape::Beam);
    vt.feed(b"\x1b[3 q"); // blinking underline
    assert_eq!(vt.term.cursor_style().shape, CursorShape::Underline);
    vt.feed(b"\x1b[2 q"); // steady block
    assert_eq!(vt.term.cursor_style().shape, CursorShape::Block);
}

// ----- reports ---------------------------------------------------------------

#[test]
fn device_status_report_cursor_position() {
    let mut vt = Vt::new(20, 5);
    vt.feed(b"\x1b[3;7H\x1b[6n");
    assert_eq!(vt.pty_output(), "\x1b[3;7R");
}

#[test]
fn primary_device_attributes() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"\x1b[c");
    assert!(vt.pty_output().starts_with("\x1b[?"), "DA1 reply: {:?}", vt.pty_output());
}

// ----- OSC -------------------------------------------------------------------

#[test]
fn osc_title_set_and_reset() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"\x1b]2;my title\x07");
    vt.feed(b"\x1b]0;both title\x1b\\"); // ST terminator form
    let events = vt.events.lock().unwrap();
    let titles: Vec<&String> = events
        .iter()
        .filter_map(|e| match e {
            Event::Title(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["my title", "both title"]);
}

#[test]
fn osc_52_clipboard_store_decodes_base64() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"\x1b]52;c;aGVsbG8=\x07"); // "hello"
    let events = vt.events.lock().unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ClipboardStore(_, s) if s == "hello"
    )));
}

#[test]
fn osc_8_hyperlinks_attach_to_cells() {
    let mut vt = Vt::new(30, 3);
    vt.feed(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\plain");
    let link = vt.cell(0, 0).hyperlink();
    assert_eq!(link.map(|h| h.uri().to_string()), Some("https://example.com".into()));
    assert!(vt.cell(0, 4).hyperlink().is_none());
}

#[test]
fn osc_4_palette_and_104_reset() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"\x1b]4;1;#ff8800\x07");
    assert_eq!(vt.term.colors()[1], Some(Rgb { r: 0xff, g: 0x88, b: 0x00 }));
    vt.feed(b"\x1b]104;1\x07");
    assert_eq!(vt.term.colors()[1], None);
}

// ----- wide and combining characters ------------------------------------------

#[test]
fn wide_chars_take_two_cells() {
    let mut vt = Vt::new(10, 3);
    vt.feed("漢字".as_bytes());
    assert_eq!(vt.cell(0, 0).c, '漢');
    assert!(vt.cell(0, 0).flags.contains(Flags::WIDE_CHAR));
    assert!(vt.cell(0, 1).flags.contains(Flags::WIDE_CHAR_SPACER));
    assert_eq!(vt.cell(0, 2).c, '字');
    assert_eq!(vt.cursor(), (0, 4));
}

#[test]
fn combining_marks_join_previous_cell() {
    let mut vt = Vt::new(10, 3);
    vt.feed("e\u{0301}x".as_bytes());
    assert_eq!(vt.cell(0, 0).c, 'e');
    assert_eq!(vt.cell(0, 0).zerowidth(), Some(&['\u{0301}'][..]));
    assert_eq!(vt.cell(0, 1).c, 'x');
}

#[test]
fn nerd_font_pua_glyphs_are_single_width() {
    // Powerline / Nerd Font glyphs live in the PUA and must stay one cell.
    let mut vt = Vt::new(10, 3);
    vt.feed("\u{e0b0}\u{f015}x".as_bytes());
    assert_eq!(vt.cell(0, 0).c, '\u{e0b0}');
    assert!(!vt.cell(0, 0).flags.contains(Flags::WIDE_CHAR));
    assert_eq!(vt.cell(0, 2).c, 'x');
}

// ----- charsets ---------------------------------------------------------------

#[test]
fn dec_special_graphics_line_drawing() {
    let mut vt = Vt::new(10, 3);
    vt.feed(b"\x1b(0lqk\x1b(B");
    assert_eq!(vt.row(0), "┌─┐");
}

// ----- synchronized updates (mode 2026) ---------------------------------------

#[test]
fn synchronized_update_buffers_until_end() {
    let mut vt = Vt::new(20, 3);
    vt.feed(b"\x1b[?2026h");
    vt.feed(b"buffered");
    assert_eq!(vt.row(0), "", "output must be held during sync update");
    vt.feed(b"\x1b[?2026l");
    assert_eq!(vt.row(0), "buffered");
}

// ----- kitty keyboard protocol (CSI u) -----------------------------------------

#[test]
fn kitty_keyboard_protocol_push_query_pop() {
    let mut vt = Vt::new(20, 3);
    assert!(!vt.term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));

    // Apps probe support with CSI ? u and expect a flags report back —
    // this is how Claude Code decides Shift+Enter works without setup.
    vt.feed(b"\x1b[?u");
    assert_eq!(vt.pty_output(), "\x1b[?0u");

    // CSI > 1 u pushes "disambiguate escape codes" onto the mode stack.
    vt.feed(b"\x1b[>1u");
    assert!(vt.term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
    vt.feed(b"\x1b[?u");
    assert!(vt.pty_output().ends_with("\x1b[?1u"));

    // CSI < u pops back to legacy encoding.
    vt.feed(b"\x1b[<u");
    assert!(!vt.term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
}
