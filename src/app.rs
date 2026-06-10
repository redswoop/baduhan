//! Process-wide state: window registry, shared factories, window class,
//! and cross-window coordination for tab drags. Single UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::renderer::Gfx;
use crate::tabs::Tab;
use crate::window::TermWindow;

pub const WIN_CLASS: PCWSTR = w!("BaduhanMainWindow");

thread_local! {
    static GFX: RefCell<Option<Rc<Gfx>>> = const { RefCell::new(None) };
    static WINDOWS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    static SHELL: RefCell<Option<Rc<String>>> = const { RefCell::new(None) };
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

pub fn shell() -> Rc<String> {
    SHELL.with(|s| {
        s.borrow_mut()
            .get_or_insert_with(|| {
                // Prefer PowerShell 7 when on PATH, else Windows PowerShell.
                let path = std::env::var("PATH").unwrap_or_default();
                for dir in path.split(';') {
                    let p = std::path::Path::new(dir.trim()).join("pwsh.exe");
                    if p.is_file() {
                        return Rc::new(p.to_string_lossy().into_owned());
                    }
                }
                Rc::new("powershell.exe".to_string())
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

pub fn create_window(initial: Option<Tab>, pos: Option<(i32, i32)>) {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let param = Box::into_raw(Box::new(initial)) as *mut core::ffi::c_void;
        let (x, y) = pos.map(|(x, y)| (x - 80, y - 16)).unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WIN_CLASS,
            w!("baduhan"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            1180,
            760,
            None,
            None,
            Some(hinstance.into()),
            Some(param),
        );
    }
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
            let init = (*cs).lpCreateParams as *mut Option<Tab>;
            let initial = if init.is_null() { None } else { *Box::from_raw(init) };
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
