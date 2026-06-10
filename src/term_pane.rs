//! A terminal pane: alacritty_terminal emulator + ConPTY, wired to a window
//! via posted messages so PTY reader threads never touch UI state directly.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{test::TermSize, Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::Result;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::palette;
use crate::pty::Pty;

pub const WM_APP_TERM_DIRTY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;
pub const WM_APP_TERM_EVENT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 2;
pub const WM_APP_PANE_EXITED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;

pub type SharedWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

/// Shared, thread-safe pointers from PTY/emulator threads back to the UI.
/// The hwnd is atomic because tabs (and their panes) can move between windows.
pub struct PaneShared {
    pub hwnd: AtomicIsize,
    pub pane_id: u64,
    pub title: Mutex<String>,
    pub dirty: AtomicBool,
    pub size: Mutex<WindowSize>,
    pub writer: Mutex<Option<SharedWriter>>,
}

impl PaneShared {
    fn post(&self, msg: u32) {
        let hwnd = HWND(self.hwnd.load(Ordering::SeqCst) as *mut _);
        if !hwnd.0.is_null() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), msg, WPARAM(self.pane_id as usize), LPARAM(0));
            }
        }
    }

    fn pty_write(&self, s: &str) {
        if let Some(w) = self.writer.lock().unwrap().as_ref() {
            let mut w = w.lock().unwrap();
            let _ = w.write_all(s.as_bytes());
            let _ = w.flush();
        }
    }
}

#[derive(Clone)]
pub struct EventProxy(pub Arc<PaneShared>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                if !self.0.dirty.swap(true, Ordering::SeqCst) {
                    self.0.post(WM_APP_TERM_DIRTY);
                }
            },
            Event::Title(title) => {
                *self.0.title.lock().unwrap() = title;
                self.0.post(WM_APP_TERM_EVENT);
            },
            Event::ResetTitle => {
                self.0.title.lock().unwrap().clear();
                self.0.post(WM_APP_TERM_EVENT);
            },
            Event::PtyWrite(s) => self.0.pty_write(&s),
            Event::TextAreaSizeRequest(fmt) => {
                let size = *self.0.size.lock().unwrap();
                let s = fmt(size);
                self.0.pty_write(&s);
            },
            Event::ColorRequest(idx, fmt) => {
                let sch = palette::scheme();
                let color = match idx {
                    0..256 => palette::indexed(idx as u8),
                    256 => sch.fg,     // NamedColor::Foreground
                    257 => sch.bg,     // NamedColor::Background
                    258 => sch.cursor, // NamedColor::Cursor
                    _ => sch.fg,
                };
                let s = fmt(color);
                self.0.pty_write(&s);
            },
            Event::ClipboardStore(_, text) => {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
                }
            },
            Event::ClipboardLoad(_, fmt) => {
                let text =
                    arboard::Clipboard::new().and_then(|mut cb| cb.get_text()).unwrap_or_default();
                let s = fmt(&text);
                self.0.pty_write(&s);
            },
            Event::ChildExit(_) | Event::Exit => self.0.post(WM_APP_PANE_EXITED),
            Event::Bell => {},
        }
    }
}

pub struct TermPane {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub pty: Pty,
    pub shared: Arc<PaneShared>,
    /// Shown as the tab title until the shell sets one via OSC.
    pub profile_name: String,
}

impl TermPane {
    pub fn spawn(
        hwnd: HWND,
        pane_id: u64,
        profile: &crate::config::Profile,
        cols: u16,
        rows: u16,
    ) -> Result<TermPane> {
        let shared = Arc::new(PaneShared {
            hwnd: AtomicIsize::new(hwnd.0 as isize),
            pane_id,
            title: Mutex::new(String::new()),
            dirty: AtomicBool::new(false),
            size: Mutex::new(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 1,
                cell_height: 1,
            }),
            writer: Mutex::new(None),
        });

        let proxy = EventProxy(shared.clone());
        let term_config = Config {
            scrolling_history: crate::app::config().scrollback_lines,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &TermSize::new(cols as usize, rows as usize),
            proxy.clone(),
        )));

        let pty = {
            let term = term.clone();
            let proxy_out = proxy.clone();
            let proxy_exit = proxy.clone();
            let mut processor: Processor = Processor::new();
            Pty::spawn(
                &profile.command,
                profile.cwd.as_deref(),
                cols,
                rows,
                move |bytes| {
                    let mut term = term.lock();
                    processor.advance(&mut *term, bytes);
                    drop(term);
                    proxy_out.send_event(Event::Wakeup);
                },
                move || {
                    proxy_exit.send_event(Event::Exit);
                },
            )?
        };

        *shared.writer.lock().unwrap() = Some(pty.writer.clone());

        Ok(TermPane { term, pty, shared, profile_name: profile.name.clone() })
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        {
            let mut size = self.shared.size.lock().unwrap();
            size.num_cols = cols;
            size.num_lines = rows;
            size.cell_width = cell_w;
            size.cell_height = cell_h;
        }
        self.pty.resize(cols, rows);
        self.term.lock().resize(TermSize::new(cols as usize, rows as usize));
    }

    pub fn title(&self) -> String {
        let t = self.shared.title.lock().unwrap().clone();
        if t.is_empty() { self.profile_name.clone() } else { t }
    }

    /// Re-target dirty/event notifications when the pane moves to another window.
    pub fn set_hwnd(&self, hwnd: HWND) {
        self.shared.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
    }
}
