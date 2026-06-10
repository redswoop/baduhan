//! baduhan (պատուհան, Armenian for "window") — a fast, native terminal for
//! Windows. ConPTY + alacritty_terminal emulation, Direct2D/DirectWrite
//! rendering, iTerm2-style tabs/splits, WebView2 dev browser panes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod browser_pane;
mod command_palette;
mod config;
mod ctl;
mod dragdrop;
mod hints;
mod images;
mod keys;
mod palette;
mod pane_tree;
mod pty;
mod renderer;
mod scripting;
mod tabs;
mod term_pane;
mod vt_tests;
mod window;

use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

fn main() {
    // CLI mode: `baduhan browse <url>` etc. talks to the running instance.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        ctl::run(&args);
    }

    unsafe {
        // OleInitialize (STA + OLE) rather than plain COM init: drag & drop
        // registration requires it.
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }

    // Load config (and activate the color scheme) before any window exists.
    let _ = app::config();

    app::register_class();
    app::create_window(None, None);

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
