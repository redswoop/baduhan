//! Process-wide state: window registry, shared factories, window class,
//! and cross-window coordination for tab drags. Single UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Config;
use crate::renderer::Gfx;
use crate::tabs::Tab;
use crate::window::TermWindow;

pub const WIN_CLASS: PCWSTR = w!("BaduhanMainWindow");

thread_local! {
    static GFX: RefCell<Option<Rc<Gfx>>> = const { RefCell::new(None) };
    static WINDOWS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    static CONFIG: RefCell<Option<Rc<Config>>> = const { RefCell::new(None) };
    /// hwnd hosting the global quake RegisterHotKey (0 = none).
    static HOTKEY_HOST: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// The quake dropdown window, if one has been created (0 = none).
    static QUAKE: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// (settings.json, init.lua) mtimes at last config load.
    static CONFIG_MTIMES: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

const QUAKE_HOTKEY_ID: i32 = 0xBA1;
/// SetTimer id for the once-a-second config mtime poll.
pub const CONFIG_TIMER_ID: usize = 0xBA2;

fn config_mtimes() -> (u64, u64) {
    let mt = |p: &std::path::Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    let settings = Config::path();
    let lua = settings.with_file_name("init.lua");
    (mt(&settings), mt(&lua))
}

/// Timer tick: if settings.json or init.lua changed on disk, reload the
/// config and apply it to every live window.
pub fn poll_config_change() {
    let now = config_mtimes();
    if CONFIG_MTIMES.with(|c| c.get()) == now {
        return;
    }
    CONFIG_MTIMES.with(|c| c.set(now));
    let mut cfg = Config::load_or_create();
    crate::scripting::init(&mut cfg);
    crate::palette::set_scheme(cfg.scheme());
    CONFIG.with(|c| *c.borrow_mut() = Some(Rc::new(cfg)));
    let handles: Vec<isize> = WINDOWS.with(|w| w.borrow().clone());
    for h in handles {
        with_window(HWND(h as *mut _), |w| w.apply_config());
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn gfx() -> Rc<Gfx> {
    GFX.with(|g| {
        g.borrow_mut()
            .get_or_insert_with(|| Rc::new(Gfx::new().expect("Direct2D/DirectWrite init failed")))
            .clone()
    })
}

/// Load (once) and return the user config; also activates the color scheme.
pub fn config() -> Rc<Config> {
    CONFIG.with(|c| {
        c.borrow_mut()
            .get_or_insert_with(|| {
                let mut cfg = Config::load_or_create();
                crate::scripting::init(&mut cfg);
                crate::palette::set_scheme(cfg.scheme());
                CONFIG_MTIMES.with(|c| c.set(config_mtimes()));
                Rc::new(cfg)
            })
            .clone()
    })
}

pub fn register_class() {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: WIN_CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// What a new window starts with.
pub enum WindowInit {
    Fresh,
    Adopt(Tab),
    Restore(crate::session::WindowState),
}

pub fn create_window(initial: Option<Tab>, pos: Option<(i32, i32)>) {
    let init = match initial {
        Some(tab) => WindowInit::Adopt(tab),
        None => WindowInit::Fresh,
    };
    let pos = pos.map(|(x, y)| (x - 80, y - 16));
    create_window_init(init, pos, None);
}

pub fn create_window_restored(state: crate::session::WindowState) {
    let pos = Some((state.x, state.y));
    let size = Some((state.w, state.h));
    create_window_init(WindowInit::Restore(state), pos, size);
}

fn create_window_init(init: WindowInit, pos: Option<(i32, i32)>, size: Option<(i32, i32)>) {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let param = Box::into_raw(Box::new(init)) as *mut core::ffi::c_void;
        let (x, y) = pos.unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
        let (w, h) = size.unwrap_or((1180, 760));
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WIN_CLASS,
            w!("baduhan"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            w.max(200),
            h.max(120),
            None,
            None,
            Some(hinstance.into()),
            Some(param),
        );
    }
}

/// Host the global quake hotkey on `hwnd` if nobody holds it yet.
pub fn ensure_quake_hotkey(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    if HOTKEY_HOST.with(|h| h.get()) != 0 {
        return;
    }
    let spec = config().quake_hotkey.clone();
    if spec.is_empty() {
        return;
    }
    let Some(k) = crate::scripting::parse_keyspec(&spec) else {
        eprintln!("bad quake_hotkey '{spec}'");
        return;
    };
    let mut mods = HOT_KEY_MODIFIERS(MOD_NOREPEAT.0);
    if k.ctrl {
        mods |= MOD_CONTROL;
    }
    if k.shift {
        mods |= MOD_SHIFT;
    }
    if k.alt {
        mods |= MOD_ALT;
    }
    unsafe {
        if RegisterHotKey(Some(hwnd), QUAKE_HOTKEY_ID, mods, k.vk as u32).is_ok() {
            HOTKEY_HOST.with(|h| h.set(hwnd.0 as isize));
        } else {
            eprintln!("quake hotkey '{spec}' is taken by another app");
        }
    }
}

/// Toggle the quake dropdown: create on first use, then show/hide.
pub fn toggle_quake() {
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    unsafe {
        let q = QUAKE.with(|c| c.get());
        if q != 0 && is_registered(HWND(q as *mut _)) {
            let h = HWND(q as *mut _);
            if IsWindowVisible(h).as_bool() && GetForegroundWindow() == h {
                let _ = ShowWindow(h, SW_HIDE);
            } else {
                let _ = ShowWindow(h, SW_SHOW);
                let _ = SetForegroundWindow(h);
                let _ = SetFocus(Some(h));
            }
            return;
        }
        // Top 45% of the primary work area, borderless-ish, topmost.
        let mut work = windows::Win32::Foundation::RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let w = work.right - work.left;
        let h = ((work.bottom - work.top) as f32 * 0.45) as i32;
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let param = Box::into_raw(Box::new(WindowInit::Fresh)) as *mut core::ffi::c_void;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            WIN_CLASS,
            w!("baduhan — quake"),
            WS_POPUP | WS_THICKFRAME | WS_VISIBLE,
            work.left,
            work.top,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            Some(param),
        );
        if let Ok(hwnd) = hwnd {
            QUAKE.with(|c| c.set(hwnd.0 as isize));
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Bookkeeping when a window dies: free the quake slot and re-home the
/// global hotkey onto a surviving window.
fn window_destroyed(hwnd: HWND) {
    if QUAKE.with(|c| c.get()) == hwnd.0 as isize {
        QUAKE.with(|c| c.set(0));
    }
    if HOTKEY_HOST.with(|h| h.get()) == hwnd.0 as isize {
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey(
                Some(hwnd),
                QUAKE_HOTKEY_ID,
            );
        }
        HOTKEY_HOST.with(|h| h.set(0));
        let survivor = WINDOWS.with(|w| w.borrow().first().copied());
        if let Some(s) = survivor {
            ensure_quake_hotkey(HWND(s as *mut _));
        }
    }
}

/// Snapshot every live window into a session and persist it.
pub fn save_session() {
    if !config().restore_session {
        return;
    }
    let mut s = crate::session::Session::default();
    let handles: Vec<isize> = WINDOWS.with(|w| w.borrow().clone());
    for h in handles {
        if let Some(Some(ws)) = with_window(HWND(h as *mut _), |w| w.snapshot()) {
            s.windows.push(ws);
        }
    }
    crate::session::save(&s);
}

fn register(hwnd: HWND) {
    WINDOWS.with(|w| w.borrow_mut().push(hwnd.0 as isize));
}

fn unregister(hwnd: HWND) -> bool {
    WINDOWS.with(|w| {
        let mut w = w.borrow_mut();
        w.retain(|&h| h != hwnd.0 as isize);
        w.is_empty()
    })
}

pub fn is_registered(hwnd: HWND) -> bool {
    WINDOWS.with(|w| w.borrow().contains(&(hwnd.0 as isize)))
}

pub fn with_window<R>(hwnd: HWND, f: impl FnOnce(&mut TermWindow) -> R) -> Option<R> {
    if !is_registered(hwnd) {
        return None;
    }
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TermWindow;
        if ptr.is_null() {
            None
        } else {
            Some(f(&mut *ptr))
        }
    }
}

/// Which term window's tab bar is under this screen point (excluding `not`)?
/// Returns the window plus the insertion index for a dropped tab.
pub fn tabbar_hit(screen: POINT, not: HWND) -> Option<(HWND, usize)> {
    unsafe {
        let hit = WindowFromPoint(screen);
        if hit.0.is_null() {
            return None;
        }
        let root = GetAncestor(hit, GA_ROOT);
        if root == not || !is_registered(root) {
            return None;
        }
        let mut pt = screen;
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(root, &mut pt);
        with_window(root, |w| w.tabbar_insert_index(pt)).flatten().map(|i| (root, i))
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const CREATESTRUCTW;
            let init = (*cs).lpCreateParams as *mut WindowInit;
            let initial =
                if init.is_null() { WindowInit::Fresh } else { *Box::from_raw(init) };
            let win = Box::new(TermWindow::new(hwnd, initial));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(win) as isize);
            register(hwnd);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TermWindow;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
            let last = unregister(hwnd);
            window_destroyed(hwnd);
            if last {
                PostQuitMessage(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        match (*ptr).message(msg, wparam, lparam) {
            Some(result) => result,
            None => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
