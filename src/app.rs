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
            if unregister(hwnd) {
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
