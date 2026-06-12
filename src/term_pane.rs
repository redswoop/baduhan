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
    /// Shell-reported cwd via OSC 7, already translated to a Windows path
    /// (`C:\…` or `\\wsl$\…`). Beats the PEB fallback when present.
    pub osc7_cwd: Mutex<Option<String>>,
    /// Total newlines ever emitted (the pane's monotonic line clock).
    pub lines_seen: std::sync::atomic::AtomicU64,
    /// OSC 133;A prompt-start marks, as `lines_seen` values (newest last).
    pub marks: Mutex<std::collections::VecDeque<u64>>,
    /// Inline images (OSC 1337), anchored to the line clock.
    pub images: Mutex<Vec<crate::images::InlineImage>>,
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
                // OSC 52 is write-only: anything running in the terminal
                // (including over SSH) could otherwise read the clipboard
                // silently. Answer with empty rather than going mute so
                // querying apps don't stall.
                let s = fmt("");
                self.0.pty_write(&s);
            },
            Event::ChildExit(_) | Event::Exit => self.0.post(WM_APP_PANE_EXITED),
            Event::Bell => {},
        }
    }
}

/// Environment for the `baduhan browse/reload` control CLI: the pane's
/// identity, the exe path, and WSLENV plumbing so WSL shells inherit both
/// (the /p flag translates the exe path to /mnt/c/... form).
fn ctl_env(pane_id: u64) -> Vec<(String, String)> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut wslenv = std::env::var("WSLENV").unwrap_or_default();
    for add in ["BADUHAN_PANE", "BADUHAN_EXE/p"] {
        let base = add.split('/').next().unwrap_or(add);
        if !wslenv.split(':').any(|e| e.split('/').next() == Some(base)) {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(add);
        }
    }
    let mut env = vec![
        ("BADUHAN_PANE".into(), pane_id.to_string()),
        ("BADUHAN_EXE".into(), exe),
        ("WSLENV".into(), wslenv),
    ];
    let cdp = crate::app::config().browser_debug_port;
    if cdp != 0 {
        env.push(("BADUHAN_CDP".into(), format!("http://127.0.0.1:{cdp}")));
        if let Some((_, w)) = env.iter_mut().find(|(k, _)| k == "WSLENV") {
            if !w.is_empty() {
                w.push(':');
            }
            w.push_str("BADUHAN_CDP");
        }
    }
    env
}

/// What kind of shell lives in the pane — drives path quoting for drag &
/// drop and whether the PEB cwd trick is meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellFlavor {
    /// pwsh / powershell / cmd: "C:\path" quoting, PEB cwd valid.
    Windows,
    /// git-bash & friends: 'C:/path' quoting, PEB cwd valid.
    Posix,
    /// WSL: '/mnt/c/path' quoting, cwd lives inside Linux (unknown to us).
    Wsl,
}

impl ShellFlavor {
    pub fn of(command: &[String]) -> ShellFlavor {
        let exe = command
            .first()
            .map(|c| c.rsplit(['\\', '/']).next().unwrap_or(c).to_ascii_lowercase())
            .unwrap_or_default();
        if exe.starts_with("wsl") {
            ShellFlavor::Wsl
        } else if exe.contains("bash") || exe == "sh.exe" || exe.contains("zsh") || exe.contains("fish") {
            ShellFlavor::Posix
        } else {
            ShellFlavor::Windows
        }
    }
}

/// The distro a `wsl.exe -d <name>` command targets.
fn wsl_distro_of(command: &[String]) -> Option<String> {
    let mut it = command.iter();
    while let Some(a) = it.next() {
        if a == "-d" || a == "--distribution" {
            return it.next().cloned();
        }
    }
    None
}

/// Find the last OSC 7 sequence in a chunk: ESC ] 7 ; <url> (BEL | ESC \).
/// Shells emit these atomically per prompt, so chunk-local scanning is fine.
pub fn scan_osc7(bytes: &[u8]) -> Option<String> {
    let mut found = None;
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' && bytes[i + 2] == b'7' && bytes[i + 3] == b';'
        {
            let start = i + 4;
            let mut j = start;
            while j < bytes.len() && bytes[j] != 0x07 && bytes[j] != 0x1b {
                j += 1;
            }
            if j < bytes.len() {
                found = Some(String::from_utf8_lossy(&bytes[start..j]).into_owned());
            }
            i = j;
        }
        i += 1;
    }
    found
}

/// Maintain the pane's line clock and OSC 133;A prompt marks. The clock
/// counts newlines in the output stream; a mark records the clock value at
/// each prompt start, giving scrollback jumps a stable-enough coordinate
/// (drifts after reflow-on-resize, which is acceptable).
fn scan_lines_and_marks(bytes: &[u8], shared: &PaneShared) {
    use std::sync::atomic::Ordering;
    let mut lines = shared.lines_seen.load(Ordering::Relaxed);
    let mut new_marks: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => lines += 1,
            0x1b if bytes[i + 1..].starts_with(b"]133;A") => {
                new_marks.push(lines);
                i += 6;
            },
            _ => {},
        }
        i += 1;
    }
    shared.lines_seen.store(lines, Ordering::Relaxed);
    if !new_marks.is_empty() {
        let mut marks = shared.marks.lock().unwrap();
        marks.extend(new_marks);
        while marks.len() > 500 {
            marks.pop_front();
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Translate an OSC 7 file:// URL into a path Windows file APIs accept.
/// Handles native paths (file:///C:/…), msys/cygwin (/c/… and /cygdrive/c/…),
/// and WSL (Linux paths → \\wsl$\<distro>\… UNC).
pub fn osc7_to_windows_path(
    url: &str,
    flavor: ShellFlavor,
    wsl_distro: Option<&str>,
) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // Strip the authority (hostname) — everything before the first '/'.
    let path = &rest[rest.find('/')?..];
    let path = percent_decode(path);

    // file:///C:/Users/x → C:\Users\x
    let native = path.strip_prefix('/').unwrap_or(&path);
    if native.len() >= 2 && native.as_bytes()[1] == b':' {
        return Some(native.replace('/', "\\"));
    }
    // msys/cygwin: /c/Users/x or /cygdrive/c/Users/x → C:\Users\x
    let drive_form = path.strip_prefix("/cygdrive").unwrap_or(&path);
    let bytes = drive_form.as_bytes();
    if bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes.len() == 2 || bytes[2] == b'/')
    {
        let drive = (bytes[1] as char).to_ascii_uppercase();
        let tail = if bytes.len() > 2 { &drive_form[2..] } else { "" };
        return Some(format!("{drive}:{}", tail.replace('/', "\\")));
    }
    // WSL: a Linux path; reachable from Windows via \\wsl$\<distro>.
    if flavor == ShellFlavor::Wsl
        && let Some(d) = wsl_distro
    {
        return Some(format!("\\\\wsl$\\{d}{}", path.replace('/', "\\")));
    }
    None
}

pub struct TermPane {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub pty: Pty,
    pub shared: Arc<PaneShared>,
    /// Shown as the tab title until the shell sets one via OSC.
    pub profile_name: String,
    pub flavor: ShellFlavor,
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
            osc7_cwd: Mutex::new(None),
            lines_seen: std::sync::atomic::AtomicU64::new(0),
            marks: Mutex::new(std::collections::VecDeque::new()),
            images: Mutex::new(Vec::new()),
        });

        let proxy = EventProxy(shared.clone());
        let term_config = Config {
            scrolling_history: crate::app::config().scrollback_lines,
            // Answer CSI ? u probes so apps (Claude Code, neovim…) enable
            // CSI u key encoding; keys.rs honors the resulting TermMode.
            kitty_keyboard: true,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &TermSize::new(cols as usize, rows as usize),
            proxy.clone(),
        )));

        let flavor = ShellFlavor::of(&profile.command);
        let wsl_distro = wsl_distro_of(&profile.command);
        let pty = {
            let term = term.clone();
            let proxy_out = proxy.clone();
            let proxy_exit = proxy.clone();
            let shared_cwd = shared.clone();
            let mut processor: Processor = Processor::new();
            let mut img_scan = crate::images::ImgScan::default();
            Pty::spawn(
                &profile.command,
                profile.cwd.as_deref(),
                cols,
                rows,
                &ctl_env(pane_id),
                move |bytes| {
                    use std::sync::atomic::Ordering;
                    // Shell-integration cwd reports (OSC 7) — alacritty
                    // ignores them, so sniff before parsing.
                    if let Some(url) = scan_osc7(bytes)
                        && let Some(path) = osc7_to_windows_path(&url, flavor, wsl_distro.as_deref())
                    {
                        *shared_cwd.osc7_cwd.lock().unwrap() = Some(path);
                    }
                    let segs = img_scan.feed(bytes);
                    let mut term = term.lock();
                    for seg in segs {
                        match seg {
                            crate::images::Seg::Plain(b) => {
                                scan_lines_and_marks(&b, &shared_cwd);
                                processor.advance(&mut *term, &b);
                            },
                            crate::images::Seg::Image(img) => {
                                // Reserve grid rows so the prompt continues
                                // below the picture.
                                let (cell_h, screen_rows) = {
                                    let s = shared_cwd.size.lock().unwrap();
                                    (s.cell_height.max(8) as u32, s.num_lines)
                                };
                                let rows = (img.height.div_ceil(cell_h) as u16)
                                    .clamp(1, screen_rows.saturating_sub(2).max(1));
                                let anchor = shared_cwd.lines_seen.load(Ordering::Relaxed);
                                {
                                    let mut imgs = shared_cwd.images.lock().unwrap();
                                    imgs.push(crate::images::InlineImage {
                                        id: crate::app::next_id(),
                                        anchor,
                                        png: std::sync::Arc::new(img.png),
                                        width: img.width,
                                        height: img.height,
                                        rows,
                                    });
                                    let excess = imgs.len().saturating_sub(16);
                                    imgs.drain(..excess);
                                }
                                let nl = "\r\n".repeat(rows as usize);
                                scan_lines_and_marks(nl.as_bytes(), &shared_cwd);
                                processor.advance(&mut *term, nl.as_bytes());
                            },
                        }
                    }
                    drop(term);
                    proxy_out.send_event(Event::Wakeup);
                },
                move || {
                    proxy_exit.send_event(Event::Exit);
                },
            )?
        };

        *shared.writer.lock().unwrap() = Some(pty.writer.clone());

        Ok(TermPane {
            term,
            pty,
            shared,
            profile_name: profile.name.clone(),
            flavor: ShellFlavor::of(&profile.command),
        })
    }

    /// The shell's live working directory as a Windows-usable path.
    /// OSC 7 (shell integration) wins; the PEB read covers pwsh/cmd without
    /// any shell config. msys bash and WSL don't sync the Win32 cwd, so
    /// without OSC 7 they return None.
    pub fn cwd(&self) -> Option<String> {
        if let Some(c) = self.shared.osc7_cwd.lock().unwrap().clone() {
            return Some(c);
        }
        match self.flavor {
            ShellFlavor::Windows => crate::pty::process_cwd(self.pty.child_pid?),
            _ => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_osc7_finds_last_url() {
        let chunk = b"junk\x1b]7;file://host/c/old\x07more\x1b]7;file://host/c/Users/x\x07tail";
        assert_eq!(scan_osc7(chunk).as_deref(), Some("file://host/c/Users/x"));
        assert_eq!(scan_osc7(b"no osc here"), None);
        // ST (ESC \) terminator form.
        assert_eq!(
            scan_osc7(b"\x1b]7;file:///C:/x\x1b\\").as_deref(),
            Some("file:///C:/x")
        );
    }

    #[test]
    fn osc7_native_and_msys_paths() {
        let f = ShellFlavor::Posix;
        assert_eq!(
            osc7_to_windows_path("file:///C:/Users/x", f, None).as_deref(),
            Some("C:\\Users\\x")
        );
        assert_eq!(
            osc7_to_windows_path("file://HOST/c/Users/ana%20k", f, None).as_deref(),
            Some("C:\\Users\\ana k")
        );
        assert_eq!(
            osc7_to_windows_path("file://h/cygdrive/d/work", f, None).as_deref(),
            Some("D:\\work")
        );
    }

    #[test]
    fn osc7_wsl_becomes_unc() {
        assert_eq!(
            osc7_to_windows_path("file://pc/home/armen/src", ShellFlavor::Wsl, Some("Ubuntu"))
                .as_deref(),
            Some("\\\\wsl$\\Ubuntu\\home\\armen\\src")
        );
        // Linux path in a non-WSL pane: unusable.
        assert_eq!(
            osc7_to_windows_path("file://pc/home/armen", ShellFlavor::Posix, None),
            None
        );
    }

    #[test]
    fn wsl_distro_parsing() {
        let cmd: Vec<String> =
            ["wsl.exe", "-d", "Ubuntu", "--cd", "~"].iter().map(|s| s.to_string()).collect();
        assert_eq!(wsl_distro_of(&cmd).as_deref(), Some("Ubuntu"));
        assert_eq!(wsl_distro_of(&["wsl.exe".to_string()]), None);
    }
}
