//! baduhan (պատուհան, Armenian for "window") — a fast, native terminal for
//! Windows. ConPTY + alacritty_terminal emulation, Direct2D/DirectWrite
//! rendering, iTerm2-style tabs/splits, WebView2 dev browser panes.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod browser_pane;
mod config;
mod keys;
mod palette;
mod pane_tree;
mod pty;
mod renderer;
mod tabs;
mod term_pane;
mod vt_tests;
mod window;

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
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
