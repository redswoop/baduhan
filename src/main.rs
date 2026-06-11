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
mod session;
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
        // Debug builds are console-subsystem so logs are visible when run
        // from a shell. Launched from Explorer, though, Windows creates a
        // console just for us — detach so no empty console window lingers.
        // (A shared console — launched from a shell — has >1 process.)
        let mut pids = [0u32; 2];
        if windows::Win32::System::Console::GetConsoleProcessList(&mut pids) == 1 {
            let _ = windows::Win32::System::Console::FreeConsole();
        }

        // OleInitialize (STA + OLE) rather than plain COM init: drag & drop
        // registration requires it.
        let _ = windows::Win32::System::Ole::OleInitialize(None);
    }

    // Load config (and activate the color scheme) before any window exists.
    let cfg = app::config();

    app::register_class();
    let restored = cfg.restore_session
        && session::load()
            .filter(|s| !s.windows.is_empty())
            .map(|s| {
                for ws in s.windows {
                    app::create_window_restored(ws);
                }
            })
            .is_some();
    if !restored {
        app::create_window(None, None);
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
