//! Embedded browser pane backed by WebView2. The WebView is a child HWND of
//! the top-level window; moving a tab between windows reparents it without a
//! reload via ICoreWebView2Controller::SetParentWindow.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DocumentTitleChangedEventHandler, NewWindowRequestedEventHandler, SourceChangedEventHandler,
};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_WEBVIEW_READY: u32 = WM_APP + 4;
pub const WM_APP_URL_ENTER: u32 = WM_APP + 5;

pub struct BrowserShared {
    pub hwnd: AtomicIsize,
    pub pane_id: u64,
    pub title: Mutex<String>,
    pub url: Mutex<String>,
}

impl BrowserShared {
    fn post(&self, msg: u32) {
        let hwnd = HWND(self.hwnd.load(Ordering::SeqCst) as *mut _);
        if !hwnd.0.is_null() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), msg, WPARAM(self.pane_id as usize), LPARAM(0));
            }
        }
    }
}

enum EnvState {
    NotStarted,
    Creating(Vec<(isize, u64)>),
    Ready(ICoreWebView2Environment),
}

thread_local! {
    static ENV: RefCell<EnvState> = const { RefCell::new(EnvState::NotStarted) };
    static READY: RefCell<HashMap<u64, ICoreWebView2Controller>> = RefCell::new(HashMap::new());
}

/// Kick off (or queue behind) async environment + controller creation.
/// Completion is delivered to `hwnd` as WM_APP_WEBVIEW_READY w/ pane id.
fn request_controller(hwnd: HWND, pane_id: u64) {
    ENV.with(|env| {
        let mut env = env.borrow_mut();
        match &mut *env {
            EnvState::Ready(e) => {
                let e = e.clone();
                drop(env);
                create_controller(&e, hwnd, pane_id);
            },
            EnvState::Creating(queue) => queue.push((hwnd.0 as isize, pane_id)),
            EnvState::NotStarted => {
                *env = EnvState::Creating(vec![(hwnd.0 as isize, pane_id)]);
                drop(env);
                let data_dir = std::env::var("LOCALAPPDATA")
                    .map(|d| format!("{d}\\baduhan-webview2"))
                    .unwrap_or_else(|_| ".baduhan-webview2".into());
                // Expose CDP so Playwright/claude can drive the embedded
                // browser (read by the WebView2 loader at env creation).
                let cdp_port = crate::app::config().browser_debug_port;
                if cdp_port != 0 {
                    unsafe {
                        std::env::set_var(
                            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                            format!("--remote-debugging-port={cdp_port}"),
                        );
                    }
                }
                let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
                    move |result, environment| {
                        if result.is_ok()
                            && let Some(environment) = environment {
                                let queue = ENV.with(|env| {
                                    let mut env = env.borrow_mut();
                                    let queue = match &mut *env {
                                        EnvState::Creating(q) => std::mem::take(q),
                                        _ => Vec::new(),
                                    };
                                    *env = EnvState::Ready(environment.clone());
                                    queue
                                });
                                for (hwnd, pane_id) in queue {
                                    create_controller(
                                        &environment,
                                        HWND(hwnd as *mut _),
                                        pane_id,
                                    );
                                }
                            }
                        Ok(())
                    },
                ));
                unsafe {
                    let _ = CreateCoreWebView2EnvironmentWithOptions(
                        PCWSTR::null(),
                        &HSTRING::from(data_dir),
                        None,
                        &handler,
                    );
                }
            },
        }
    });
}

fn create_controller(env: &ICoreWebView2Environment, hwnd: HWND, pane_id: u64) {
    let handler =
        CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
            if result.is_ok()
                && let Some(controller) = controller {
                    READY.with(|r| r.borrow_mut().insert(pane_id, controller));
                    unsafe {
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_APP_WEBVIEW_READY,
                            WPARAM(pane_id as usize),
                            LPARAM(0),
                        );
                    }
                }
            Ok(())
        }));
    unsafe {
        let _ = env.CreateCoreWebView2Controller(hwnd, &handler);
    }
}

/// Fetch a finished controller (from WM_APP_WEBVIEW_READY).
pub fn take_ready_controller(pane_id: u64) -> Option<ICoreWebView2Controller> {
    READY.with(|r| r.borrow_mut().remove(&pane_id))
}

pub struct BrowserPane {
    pub shared: Arc<BrowserShared>,
    pub controller: Option<ICoreWebView2Controller>,
    pub webview: Option<ICoreWebView2>,
    pub edit: HWND,
    pending_url: String,
    visible: bool,
}

impl BrowserPane {
    pub fn new(parent: HWND, pane_id: u64, url: &str, edit_font: windows::Win32::Graphics::Gdi::HFONT) -> BrowserPane {
        let shared = Arc::new(BrowserShared {
            hwnd: AtomicIsize::new(parent.0 as isize),
            pane_id,
            title: Mutex::new("Browser".into()),
            url: Mutex::new(url.to_string()),
        });

        // Win32 EDIT control as the URL bar.
        let edit = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("EDIT"),
                &HSTRING::from(url),
                WINDOW_STYLE(
                    (WS_CHILD.0) | (WS_VISIBLE.0) | (ES_AUTOHSCROLL as u32) | (WS_CLIPSIBLINGS.0),
                ),
                0,
                0,
                10,
                10,
                Some(parent),
                None,
                None,
                None,
            )
            .unwrap_or_else(|e| {
                eprintln!("URL bar EDIT creation failed: {e:?}");
                Default::default()
            })
        };
        unsafe {
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(edit_font.0 as usize)),
                Some(LPARAM(1)),
            );
            // Let the main wndproc find the pane when Enter is pressed.
            crate::browser_pane::subclass_edit(edit, pane_id);
        }

        request_controller(parent, pane_id);

        BrowserPane {
            shared,
            controller: None,
            webview: None,
            edit,
            pending_url: url.to_string(),
            visible: true,
        }
    }

    pub fn title(&self) -> String {
        self.shared.title.lock().unwrap().clone()
    }

    /// Install the async-created controller and wire events.
    pub fn install(&mut self, controller: ICoreWebView2Controller) {
        unsafe {
            let _ = controller.SetIsVisible(self.visible);
            if let Ok(webview) = controller.CoreWebView2() {
                let shared = self.shared.clone();
                let mut token = 0i64;
                let _ = webview.add_DocumentTitleChanged(
                    &DocumentTitleChangedEventHandler::create(Box::new(move |webview, _| {
                        if let Some(webview) = webview {
                            let mut buf = windows::core::PWSTR::null();
                            if webview.DocumentTitle(&mut buf).is_ok() && !buf.is_null() {
                                let s = String::from_utf16_lossy(buf.as_wide());
                                *shared.title.lock().unwrap() = s;
                                shared.post(crate::term_pane::WM_APP_TERM_EVENT);
                            }
                        }
                        Ok(())
                    })),
                    &mut token,
                );

                let shared = self.shared.clone();
                let edit = self.edit;
                let mut token2 = 0i64;
                let _ = webview.add_SourceChanged(
                    &SourceChangedEventHandler::create(Box::new(move |webview, _| {
                        if let Some(webview) = webview {
                            let mut buf = windows::core::PWSTR::null();
                            if webview.Source(&mut buf).is_ok() && !buf.is_null() {
                                let s = String::from_utf16_lossy(buf.as_wide());
                                let _ = SetWindowTextW(edit, &HSTRING::from(s.as_str()));
                                *shared.url.lock().unwrap() = s;
                            }
                        }
                        Ok(())
                    })),
                    &mut token2,
                );

                // Open in-place instead of spawning OS browser windows.
                let mut token3 = 0i64;
                let wv = webview.clone();
                let _ = webview.add_NewWindowRequested(
                    &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                        if let Some(args) = args {
                            let mut uri = windows::core::PWSTR::null();
                            if args.Uri(&mut uri).is_ok() && !uri.is_null() {
                                let s = String::from_utf16_lossy(uri.as_wide());
                                let _ = args.SetHandled(true);
                                let _ = wv.Navigate(&HSTRING::from(s.as_str()));
                            }
                        }
                        Ok(())
                    })),
                    &mut token3,
                );

                let url = self.pending_url.clone();
                let _ = webview.Navigate(&HSTRING::from(url.as_str()));
                self.webview = Some(webview);
            }
            self.controller = Some(controller);
        }
    }

    pub fn navigate(&mut self, url: &str) {
        let url = normalize_url(url);
        *self.shared.url.lock().unwrap() = url.clone();
        unsafe {
            let _ = SetWindowTextW(self.edit, &HSTRING::from(url.as_str()));
        }
        if let Some(webview) = &self.webview {
            unsafe {
                let _ = webview.Navigate(&HSTRING::from(url.as_str()));
            }
        } else {
            self.pending_url = url;
        }
    }

    pub fn back(&self) {
        if let Some(w) = &self.webview {
            unsafe {
                let _ = w.GoBack();
            }
        }
    }

    pub fn forward(&self) {
        if let Some(w) = &self.webview {
            unsafe {
                let _ = w.GoForward();
            }
        }
    }

    pub fn reload(&self) {
        if let Some(w) = &self.webview {
            unsafe {
                let _ = w.Reload();
            }
        }
    }

    pub fn devtools(&self) {
        if let Some(w) = &self.webview {
            unsafe {
                let _ = w.OpenDevToolsWindow();
            }
        }
    }

    /// Position webview + URL bar. Rects are physical pixels in parent space.
    pub fn set_bounds(&self, webview_px: RECT, edit_px: RECT) {
        unsafe {
            if let Some(c) = &self.controller {
                let _ = c.SetBounds(webview_px);
            }
            let _ = SetWindowPos(
                self.edit,
                None,
                edit_px.left,
                edit_px.top,
                edit_px.right - edit_px.left,
                edit_px.bottom - edit_px.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn show(&mut self, visible: bool) {
        self.visible = visible;
        unsafe {
            if let Some(c) = &self.controller {
                let _ = c.SetIsVisible(visible);
            }
            let _ = ShowWindow(self.edit, if visible { SW_SHOWNA } else { SW_HIDE });
        }
    }

    /// Move this pane's HWND-backed pieces to another top-level window.
    pub fn reparent(&self, new_parent: HWND) {
        self.shared.hwnd.store(new_parent.0 as isize, Ordering::SeqCst);
        unsafe {
            if let Some(c) = &self.controller {
                let _ = c.SetParentWindow(new_parent);
            }
            let _ = windows::Win32::UI::WindowsAndMessaging::SetParent(self.edit, Some(new_parent));
        }
    }

    /// Hand keyboard focus to the page content.
    pub fn focus_webview(&self) {
        if let Some(c) = &self.controller {
            unsafe {
                let _ = c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
            }
        }
    }

    /// Focus the URL bar with the text selected, ready to type over.
    pub fn focus_url_bar(&self) {
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(self.edit));
            SendMessageW(
                self.edit,
                windows::Win32::UI::Controls::EM_SETSEL,
                Some(WPARAM(0)),
                Some(LPARAM(-1)),
            );
        }
    }

    pub fn edit_text(&self) -> String {
        unsafe {
            let len = GetWindowTextLengthW(self.edit);
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(self.edit, &mut buf);
            String::from_utf16_lossy(&buf[..n.max(0) as usize])
        }
    }
}

impl Drop for BrowserPane {
    fn drop(&mut self) {
        unsafe {
            if let Some(c) = &self.controller {
                let _ = c.Close();
            }
            if !self.edit.is_invalid() {
                let _ = DestroyWindow(self.edit);
            }
        }
    }
}

pub fn normalize_url(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    if s.contains("://") || s.starts_with("about:") || s.starts_with("edge:") {
        return s.into();
    }
    if s.starts_with("localhost") || s.starts_with("127.") || s.starts_with("0.0.0.0") {
        return format!("http://{s}");
    }
    // Bare word without dots â†’ search.
    if !s.contains('.') && !s.contains(':') {
        return format!("https://www.google.com/search?q={}", s.replace(' ', "+"));
    }
    format!("https://{s}")
}

/// Subclass the EDIT so Enter triggers navigation (posted to the parent).
unsafe fn subclass_edit(edit: HWND, pane_id: u64) {
    use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

    unsafe extern "system" fn edit_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        pane_id: usize,
    ) -> windows::Win32::Foundation::LRESULT {
        match msg {
            WM_KEYDOWN if wparam.0 == VK_RETURN_USIZE => {
                let parent = unsafe { GetParent(hwnd) }.unwrap_or_default();
                unsafe {
                    let _ = PostMessageW(
                        Some(parent),
                        WM_APP_URL_ENTER,
                        WPARAM(pane_id),
                        LPARAM(0),
                    );
                }
                windows::Win32::Foundation::LRESULT(0)
            },
            WM_CHAR if wparam.0 == 0x0D => windows::Win32::Foundation::LRESULT(0),
            WM_KEYDOWN if wparam.0 == VK_ESCAPE_USIZE => {
                let parent = unsafe { GetParent(hwnd) }.unwrap_or_default();
                unsafe {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(parent));
                }
                windows::Win32::Foundation::LRESULT(0)
            },
            _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
        }
    }

    const VK_RETURN_USIZE: usize = 0x0D;
    const VK_ESCAPE_USIZE: usize = 0x1B;

    unsafe {
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, pane_id as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn full_urls_pass_through() {
        assert_eq!(normalize_url("https://example.com/x"), "https://example.com/x");
        assert_eq!(normalize_url("about:blank"), "about:blank");
    }

    #[test]
    fn localhost_gets_http() {
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("127.0.0.1:8080"), "http://127.0.0.1:8080");
    }

    #[test]
    fn domains_get_https() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
    }

    #[test]
    fn bare_words_become_searches() {
        assert!(normalize_url("rust lifetimes").contains("google.com/search"));
    }

    #[test]
    fn empty_is_blank() {
        assert_eq!(normalize_url("  "), "about:blank");
    }
}
