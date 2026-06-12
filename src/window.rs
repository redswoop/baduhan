//! Top-level terminal window: tab bar, pane area, input routing, painting.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use windows::core::HSTRING;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app;
use crate::browser_pane::{self, BrowserPane, WM_APP_URL_ENTER, WM_APP_WEBVIEW_READY};
use crate::keys::{self, Mods};
use crate::palette;
use crate::pane_tree::{self, Dir, PaneId, RectF};
use crate::renderer::{self, rect as rf, FontSet, WindowGfx};
use crate::tabs::{Pane, PaneKind, Tab};
use crate::term_pane::{
    TermPane, WM_APP_PANE_EXITED, WM_APP_TERM_DIRTY, WM_APP_TERM_EVENT,
};

pub const WM_APP_DUMP_FRAME: u32 = WM_APP + 9;
/// wparam = Box<(Vec<String>, DropOp, POINT)> from the OLE drop handler.
pub const WM_APP_DROP_FILES: u32 = WM_APP + 11;
#[cfg(debug_assertions)]
pub const WM_APP_DEBUG_ACTION: u32 = WM_APP + 10;

pub const TABBAR_H: f32 = 38.0;
pub const TOOLBAR_H: f32 = 34.0;
pub const PANE_PAD: f32 = 2.0;
/// Per-pane title bar height, shown only when a tab has multiple panes.
pub const PANE_TITLE_H: f32 = 22.0;
const TAB_MAX_W: f32 = 220.0;
const TAB_MIN_W: f32 = 90.0;
const PLUS_W: f32 = 28.0;
const BTN_W: f32 = 30.0;

const D2DERR_RECREATE_TARGET: i32 = 0x8899000Cu32 as i32;
/// SetTimer id for clearing the announcement toast.
const TOAST_TIMER_ID: usize = 0xBA3;

enum Drag {
    None,
    Divider { path: Vec<usize>, index: usize, dir: Dir, last: (f32, f32) },
    Tab { idx: usize, press: (f32, f32), cur: (f32, f32), live: bool },
    /// Dragging a pane by its title bar (iTerm2-style rearrange).
    Pane { id: PaneId, press: (f32, f32), cur: (f32, f32), live: bool },
    Select,
}

/// Where a dragged pane would land, plus the preview rect to highlight.
enum DropTarget {
    /// Split `target` in `dir`, dragged pane goes before/after.
    Zone { target: PaneId, dir: Dir, before: bool, preview: RectF },
    /// Swap places with `target` (drop in its center).
    Swap { target: PaneId, preview: RectF },
    /// Drop on the tab bar: pane becomes its own tab.
    NewTab,
    Nothing,
}

/// The pane's content area: below the title bar when title bars are shown.
fn content_rect(r: RectF, multi: bool) -> RectF {
    if multi {
        RectF { x: r.x, y: r.y + PANE_TITLE_H, w: r.w, h: (r.h - PANE_TITLE_H).max(0.0) }
    } else {
        r
    }
}

fn title_close_rect(r: RectF) -> RectF {
    RectF { x: r.x + r.w - 22.0, y: r.y, w: 20.0, h: PANE_TITLE_H }
}

/// Scrollback search state (Ctrl+Shift+F overlay).
struct SearchUi {
    query: String,
    dfas: Option<RegexSearch>,
    found: Option<Match>,
}

/// Quick-select hints state (Ctrl+Shift+Space overlay).
struct HintsUi {
    matches: Vec<crate::hints::HintMatch>,
    typed: String,
}

/// Command palette state (Ctrl+Shift+P overlay).
struct PaletteUi {
    items: Vec<crate::command_palette::Item>,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
}

pub struct TermWindow {
    pub hwnd: HWND,
    gfx_win: Option<WindowGfx>,
    /// Chrome fonts + the default-size cell fonts. Tabs zoomed away from the
    /// default carry their own FontSet (Tab::fonts).
    pub fonts: FontSet,
    dpi: f32,
    pub tabs: Vec<Tab>,
    pub active: usize,
    focused: bool,
    /// Pane currently holding "terminal focus" (got \x1b[I last).
    focus_pane: Option<PaneId>,
    search: Option<SearchUi>,
    hints: Option<HintsUi>,
    palette: Option<PaletteUi>,
    /// Keyboard-shortcut overlay (Ctrl+Shift+/); any key dismisses.
    cheatsheet: bool,
    /// Hovered caption button (HTMINBUTTON/HTMAXBUTTON/HTCLOSE) for hot paint.
    hot_caption: Option<u32>,
    /// Transient on-screen announcement ("theme: Dracula").
    toast: Option<String>,
    drag: Drag,
    edit_font: HFONT,
    edit_brush: HBRUSH,
    suppress_char: bool,
    pending_surrogate: Option<u16>,
    pending_init: Option<app::WindowInit>,
}

impl TermWindow {
    pub fn new(hwnd: HWND, pending_init: app::WindowInit) -> TermWindow {
        let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
        let dpi = if dpi <= 0.0 { 96.0 } else { dpi };
        let cfg = app::config();
        let fonts =
            FontSet::new(&app::gfx(), &cfg.font_family, cfg.font_size, dpi).expect("fonts");
        let edit_font = make_edit_font(dpi);
        let edit_brush = unsafe { CreateSolidBrush(COLORREF(0x002A1E1E)) };
        TermWindow {
            hwnd,
            gfx_win: None,
            fonts,
            dpi,
            tabs: Vec::new(),
            active: 0,
            focused: true,
            focus_pane: None,
            search: None,
            hints: None,
            palette: None,
            cheatsheet: false,
            hot_caption: None,
            toast: None,
            drag: Drag::None,
            edit_font,
            edit_brush,
            suppress_char: false,
            pending_surrogate: None,
            pending_init: Some(pending_init),
        }
    }

    fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    /// Cell fonts for the active tab (its own zoomed set, or the default).
    fn cell_fonts(&self) -> &FontSet {
        self.tabs
            .get(self.active)
            .and_then(|t| t.fonts.as_ref())
            .unwrap_or(&self.fonts)
    }

    fn active_font_size(&self) -> f32 {
        self.tabs.get(self.active).map(|t| t.font_size).unwrap_or_else(|| app::config().font_size)
    }

    fn client_px(&self) -> (u32, u32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        ((rc.right - rc.left).max(0) as u32, (rc.bottom - rc.top).max(0) as u32)
    }

    fn client_dips(&self) -> (f32, f32) {
        let (w, h) = self.client_px();
        (w as f32 / self.scale(), h as f32 / self.scale())
    }

    fn pane_area(&self) -> RectF {
        let (w, h) = self.client_dips();
        RectF { x: 0.0, y: TABBAR_H, w, h: (h - TABBAR_H).max(0.0) }
    }

    // ----- custom frame (tabs in the title bar) ------------------------------

    /// Normal windows get the custom frame; the quake popup keeps its own.
    fn custom_frame(&self) -> bool {
        let style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) } as u32;
        style & WS_CAPTION.0 == WS_CAPTION.0
    }

    /// Resize border thickness in physical px (incl. padded border), per DPI.
    fn frame_metrics(&self) -> (i32, i32) {
        unsafe {
            let dpi = self.dpi as u32;
            let pad = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            (
                GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + pad,
                GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + pad,
            )
        }
    }

    /// Min / max / close caption button rects (DIPs, right-aligned).
    fn caption_buttons(&self) -> (RectF, RectF, RectF) {
        let (w, _) = self.client_dips();
        let bw = 46.0;
        let h = TABBAR_H - 6.0;
        let close = RectF { x: w - bw, y: 0.0, w: bw, h };
        let max = RectF { x: w - 2.0 * bw, y: 0.0, w: bw, h };
        let min = RectF { x: w - 3.0 * bw, y: 0.0, w: bw, h };
        (min, max, close)
    }

    // ----- tabs ------------------------------------------------------------

    fn tab_width(&self) -> f32 {
        let (w, _) = self.client_dips();
        let caption = if self.custom_frame() { 3.0 * 46.0 } else { 0.0 };
        let avail = (w - PLUS_W - 16.0 - caption).max(50.0);
        (avail / self.tabs.len().max(1) as f32).clamp(TAB_MIN_W, TAB_MAX_W)
    }

    fn tab_rect(&self, i: usize) -> RectF {
        let tw = self.tab_width();
        RectF { x: 8.0 + i as f32 * tw, y: 6.0, w: tw - 4.0, h: TABBAR_H - 10.0 }
    }

    fn plus_rect(&self) -> RectF {
        let tw = self.tab_width();
        RectF { x: 8.0 + self.tabs.len() as f32 * tw + 2.0, y: 8.0, w: PLUS_W - 6.0, h: TABBAR_H - 14.0 }
    }

    pub fn new_tab(&mut self) {
        let cfg = app::config();
        self.new_tab_with(cfg.default_profile());
    }

    pub fn new_tab_with_profile(&mut self, idx: usize) {
        let cfg = app::config();
        if let Some(profile) = cfg.profiles.get(idx) {
            self.new_tab_with(profile);
        }
    }

    fn new_tab_with(&mut self, profile: &crate::config::Profile) {
        let pane_id = app::next_id();
        match TermPane::spawn(self.hwnd, pane_id, profile, 100, 30) {
            Ok(tp) => {
                let tab = Tab::single(
                    Pane { id: pane_id, kind: PaneKind::Term(tp) },
                    app::config().font_size,
                );
                self.tabs.push(tab);
                self.switch_tab(self.tabs.len() - 1);
            },
            Err(e) => {
                let msg =
                    HSTRING::from(format!("Failed to start {}: {e}", profile.name));
                unsafe {
                    MessageBoxW(Some(self.hwnd), &msg, windows::core::w!("baduhan"), MB_ICONERROR);
                }
            },
        }
    }

    /// Native popup listing the configured profiles; `(x, y)` in DIPs.
    fn show_profile_menu(&mut self, x: f32, y: f32) {
        let cfg = app::config();
        let picked = unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            for (i, p) in cfg.profiles.iter().enumerate() {
                let label = if i < 9 {
                    format!("{}\tCtrl+Shift+{}", p.name, i + 1)
                } else {
                    p.name.clone()
                };
                let _ = AppendMenuW(menu, MF_STRING, i + 1, &HSTRING::from(label));
            }
            let s = self.scale();
            let mut pt = POINT { x: (x * s) as i32, y: (y * s) as i32 };
            let _ = ClientToScreen(self.hwnd, &mut pt);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            cmd.0 as usize
        };
        if picked > 0 {
            self.new_tab_with_profile(picked - 1);
        }
    }

    pub fn adopt_tab(&mut self, mut tab: Tab, index: Option<usize>) {
        for pane in tab.panes.values_mut() {
            match &mut pane.kind {
                PaneKind::Term(t) => t.set_hwnd(self.hwnd),
                PaneKind::Browser(b) => b.reparent(self.hwnd),
            }
        }
        let idx = index.unwrap_or(self.tabs.len()).min(self.tabs.len());
        self.tabs.insert(idx, tab);
        self.switch_tab(idx);
    }

    /// Remove a tab intact (for moving to another window).
    fn take_tab(&mut self, idx: usize) -> Tab {
        let mut tab = self.tabs.remove(idx);
        for pane in tab.panes.values_mut() {
            if let PaneKind::Browser(b) = &mut pane.kind {
                b.show(false);
            }
        }
        if self.tabs.is_empty() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        } else {
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
            self.switch_tab(self.active);
        }
        tab
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let show = i == idx;
            for pane in tab.panes.values_mut() {
                if let PaneKind::Browser(b) = &mut pane.kind {
                    b.show(show);
                }
            }
        }
        self.active = idx;
        self.relayout();
        self.update_title();
        self.focus_active_pane();
        self.invalidate();
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        // Persist the session before any close mutates it; on app exit the
        // last save before the final close wins.
        app::save_session();
        drop(self.tabs.remove(idx));
        if self.tabs.is_empty() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            return;
        }
        let new_active = if self.active >= self.tabs.len() { self.tabs.len() - 1 } else { self.active };
        self.switch_tab(new_active);
    }

    /// Tab title with the init.lua tab_title hook applied.
    fn styled_tab_title(&self, tab: &Tab) -> String {
        let t = tab.title();
        crate::scripting::format_title(&t).unwrap_or(t)
    }

    fn update_title(&self) {
        if let Some(tab) = self.tabs.get(self.active) {
            unsafe {
                let _ = SetWindowTextW(
                    self.hwnd,
                    &HSTRING::from(format!("{} — baduhan", self.styled_tab_title(tab))),
                );
            }
        }
    }

    fn focus_active_pane(&mut self) {
        if let Some(tab) = self.tabs.get(self.active)
            && matches!(tab.panes.get(&tab.active).map(|p| &p.kind), Some(PaneKind::Term(_))) {
                unsafe {
                    let _ = SetFocus(Some(self.hwnd));
                }
            }
        self.sync_pane_focus();
    }

    fn term_by_id(&self, id: PaneId) -> Option<&TermPane> {
        self.tabs.iter().find_map(|t| match t.pane(id).map(|p| &p.kind) {
            Some(PaneKind::Term(tp)) => Some(tp),
            _ => None,
        })
    }

    /// Send focus-in/out (CSI I / CSI O) to terminals as pane focus moves —
    /// pane switches and window activation both funnel through here.
    fn sync_pane_focus(&mut self) {
        let cur = if self.focused {
            self.tabs
                .get(self.active)
                .map(|t| t.active)
                .filter(|id| self.term_by_id(*id).is_some())
        } else {
            None
        };
        if cur == self.focus_pane {
            return;
        }
        let send = |t: Option<&TermPane>, seq: &[u8]| {
            if let Some(t) = t
                && t.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
                    t.pty.write(seq);
                }
        };
        if let Some(old) = self.focus_pane {
            send(self.term_by_id(old), b"\x1b[O");
        }
        if let Some(new) = cur {
            send(self.term_by_id(new), b"\x1b[I");
        }
        self.focus_pane = cur;
    }

    // ----- layout ----------------------------------------------------------

    pub fn relayout(&mut self) {
        let area = self.pane_area();
        let scale = self.scale();
        let (cw, ch) = {
            let f = self.cell_fonts();
            (f.cell_w, f.cell_h)
        };
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let lay = tab.layout(area);
        let multi = lay.panes.len() > 1;
        for (id, r) in &lay.panes {
            let c = content_rect(*r, multi);
            match &mut tab.panes.get_mut(id).map(|p| &mut p.kind) {
                Some(PaneKind::Term(t)) => {
                    let cols = (((c.w - 2.0 * PANE_PAD) / cw) as u16).max(2);
                    let rows = (((c.h - 2.0 * PANE_PAD) / ch) as u16).max(1);
                    let cur = *t.shared.size.lock().unwrap();
                    if cur.num_cols != cols || cur.num_lines != rows {
                        t.resize(cols, rows, cw as u16, ch as u16);
                    }
                },
                Some(PaneKind::Browser(b)) => {
                    let chrome = browser_chrome(c);
                    let px = |v: f32| (v * scale).round() as i32;
                    let webview_px = RECT {
                        left: px(c.x),
                        top: px(chrome.toolbar.y + chrome.toolbar.h),
                        right: px(c.x + c.w),
                        bottom: px(c.y + c.h),
                    };
                    let edit_px = RECT {
                        left: px(chrome.edit.x),
                        top: px(chrome.edit.y),
                        right: px(chrome.edit.x + chrome.edit.w),
                        bottom: px(chrome.edit.y + chrome.edit.h),
                    };
                    b.set_bounds(webview_px, edit_px);
                },
                None => {},
            }
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    // ----- painting --------------------------------------------------------

    fn ensure_gfx(&mut self) {
        if self.gfx_win.is_none() {
            let (w, h) = self.client_px();
            match WindowGfx::new(&app::gfx(), self.hwnd, w.max(1), h.max(1), self.dpi) {
                Ok(g) => self.gfx_win = Some(g),
                Err(e) => eprintln!("WindowGfx::new failed: {e:?}"),
            }
        }
    }

    fn paint(&mut self) {
        self.ensure_gfx();
        let Some(win) = self.gfx_win.take() else { return };
        self.draw_frame(&win);
        let result: windows::core::Result<()> = unsafe { win.rt.EndDraw(None, None) };
        match result {
            Err(e) if e.code().0 == D2DERR_RECREATE_TARGET => {
                self.gfx_win = None;
                self.invalidate();
            },
            Err(e) => {
                eprintln!("EndDraw failed: {e:?}");
                self.gfx_win = Some(win);
            },
            Ok(()) => self.gfx_win = Some(win),
        }
    }

    /// Write the current frame to a PNG (debug aid, WM_APP_DUMP_FRAME).
    fn dump_frame(&self, path: &str) -> anyhow::Result<()> {
        use windows::Win32::Graphics::Imaging::*;
        use windows::Win32::System::Com::*;
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let (w, h) = self.client_px();
            let (wg, bmp) =
                WindowGfx::new_wic(&app::gfx(), &wic, w.max(1), h.max(1), self.dpi)?;
            self.draw_frame(&wg);
            wg.rt.EndDraw(None, None)?;

            let stream = wic.CreateStream()?;
            stream.InitializeFromFilename(
                &HSTRING::from(path),
                windows::Win32::Foundation::GENERIC_WRITE.0,
            )?;
            let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or_else(|| anyhow::anyhow!("no frame"))?;
            frame.Initialize(None)?;
            frame.WriteSource(&bmp, std::ptr::null())?;
            frame.Commit()?;
            enc.Commit()?;
        }
        Ok(())
    }

    fn draw_frame(&self, win: &WindowGfx) {
        let gfx = app::gfx();
        unsafe {
            win.rt.BeginDraw();
        }
        unsafe {
            win.rt.Clear(Some(&palette::d2d(palette::CHROME_BG)));
        }

        self.draw_tab_bar(win);

        if let Some(tab) = self.tabs.get(self.active) {
            let cell_fonts = tab.fonts.as_ref().unwrap_or(&self.fonts);
            let area = self.pane_area();
            let lay = tab.layout(area);
            let multi = lay.panes.len() > 1;

            for d in &lay.dividers {
                win.fill(
                    rf(d.rect.x, d.rect.y, d.rect.x + d.rect.w, d.rect.y + d.rect.h),
                    palette::d2d(palette::DIVIDER),
                );
            }

            let dim = app::config().dim_inactive_panes.clamp(0.0, 0.8);
            for (id, r) in &lay.panes {
                let Some(pane) = tab.pane(*id) else { continue };
                let active = *id == tab.active;
                let prect = rf(r.x, r.y, r.x + r.w, r.y + r.h);
                let c = content_rect(*r, multi);
                if multi {
                    self.draw_pane_title(win, &gfx, pane, *r, active);
                }
                match &pane.kind {
                    PaneKind::Term(t) => {
                        t.shared.dirty.store(false, std::sync::atomic::Ordering::SeqCst);
                        let term = t.term.lock();
                        renderer::draw_term(
                            win,
                            &gfx,
                            cell_fonts,
                            &term,
                            rf(c.x, c.y, c.x + c.w, c.y + c.h),
                            self.focused && active,
                        );
                        // Inline images, anchored to the pane's line clock.
                        let drawable = visible_images(t, &term, c, cell_fonts);
                        drop(term);
                        if !drawable.is_empty() {
                            let clip = rf(c.x, c.y, c.x + c.w, c.y + c.h);
                            unsafe {
                                win.rt.PushAxisAlignedClip(
                                    &clip,
                                    windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_ALIASED,
                                );
                            }
                            for (id, png, dest) in &drawable {
                                win.draw_image(*id, png, *dest);
                            }
                            unsafe {
                                win.rt.PopAxisAlignedClip();
                            }
                        }
                        // iTerm2-style dimming of unfocused splits. (Browser
                        // panes are live HWNDs above us — can't be veiled.)
                        if multi && !active && dim > 0.0 {
                            win.fill(prect, palette::d2d_a(palette::rgb(0, 0, 0), dim));
                        }
                    },
                    PaneKind::Browser(_) => {
                        self.draw_browser_chrome(win, c);
                    },
                }
                if multi && active {
                    win.frame(prect, palette::d2d_a(palette::ACCENT, 0.9), 1.5);
                }
            }

            // Drop-zone preview while dragging a pane by its title bar.
            if let Drag::Pane { id, cur, live: true, .. } = &self.drag {
                match self.drop_target(cur.0, cur.1, *id) {
                    DropTarget::Zone { preview, .. } => {
                        let p = rf(preview.x, preview.y, preview.x + preview.w, preview.y + preview.h);
                        win.fill(p, palette::d2d_a(palette::ACCENT, 0.25));
                        win.frame(p, palette::d2d_a(palette::ACCENT, 0.9), 2.0);
                    },
                    DropTarget::Swap { preview, .. } => {
                        let p = rf(preview.x, preview.y, preview.x + preview.w, preview.y + preview.h);
                        win.frame(p, palette::d2d_a(palette::ACCENT, 0.9), 3.0);
                    },
                    DropTarget::NewTab => {
                        let (w, _) = self.client_dips();
                        win.fill(
                            rf(0.0, 0.0, w, TABBAR_H),
                            palette::d2d_a(palette::ACCENT, 0.18),
                        );
                    },
                    DropTarget::Nothing => {},
                }
                // Floating chip with the dragged pane's title.
                if let Some(pane) =
                    self.tabs.get(self.active).and_then(|t| t.pane(*id))
                {
                    let label = pane.title();
                    let chip = rf(cur.0 + 10.0, cur.1 + 8.0, cur.0 + 150.0, cur.1 + 30.0);
                    win.rounded(chip, 4.0, palette::d2d_a(palette::TAB_INACTIVE, 0.95));
                    win.text(
                        &gfx,
                        &label,
                        &self.fonts.ui,
                        rf(chip.left + 8.0, chip.top, chip.right - 4.0, chip.bottom),
                        palette::d2d(palette::TAB_TEXT_ACTIVE),
                    );
                }
            }

            self.draw_search_bar(win, &gfx);
            self.draw_hints(win, &gfx);
            self.draw_palette(win, &gfx);
            self.draw_cheatsheet(win, &gfx);
            self.draw_toast(win, &gfx);
        }
    }

    /// Classify where a pane dragged to (x, y) would land.
    fn drop_target(&self, x: f32, y: f32, dragged: PaneId) -> DropTarget {
        if y < TABBAR_H {
            return DropTarget::NewTab;
        }
        let Some((target, r)) = self.pane_at(x, y) else { return DropTarget::Nothing };
        if target == dragged {
            return DropTarget::Nothing;
        }
        // Relative distance to each edge; nearest edge under 30% wins,
        // otherwise the center zone swaps.
        let fx = ((x - r.x) / r.w.max(1.0)).clamp(0.0, 1.0);
        let fy = ((y - r.y) / r.h.max(1.0)).clamp(0.0, 1.0);
        let edges = [
            (fx, Dir::Row, true),         // left
            (1.0 - fx, Dir::Row, false),  // right
            (fy, Dir::Col, true),         // top
            (1.0 - fy, Dir::Col, false),  // bottom
        ];
        let (d, dir, before) = edges
            .iter()
            .copied()
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();
        if d > 0.3 {
            return DropTarget::Swap { target, preview: r };
        }
        let preview = match (dir, before) {
            (Dir::Row, true) => RectF { x: r.x, y: r.y, w: r.w / 2.0, h: r.h },
            (Dir::Row, false) => RectF { x: r.x + r.w / 2.0, y: r.y, w: r.w / 2.0, h: r.h },
            (Dir::Col, true) => RectF { x: r.x, y: r.y, w: r.w, h: r.h / 2.0 },
            (Dir::Col, false) => RectF { x: r.x, y: r.y + r.h / 2.0, w: r.w, h: r.h / 2.0 },
        };
        DropTarget::Zone { target, dir, before, preview }
    }

    /// Drop-effect feedback for OLE drag-over: what would happen here?
    pub fn drop_effect_at(
        &self,
        px: i32,
        py: i32,
        op: crate::dragdrop::DropOp,
    ) -> windows::Win32::System::Ole::DROPEFFECT {
        use windows::Win32::System::Ole::*;
        let s = self.scale();
        let (x, y) = (px as f32 / s, py as f32 / s);
        let Some((pane_id, _)) = self.pane_at(x, y) else { return DROPEFFECT_NONE };
        match self.tabs.get(self.active).and_then(|t| t.pane(pane_id)).map(|p| &p.kind) {
            Some(PaneKind::Browser(_)) => DROPEFFECT_COPY, // open the file
            Some(PaneKind::Term(t)) => {
                // Copy/move need a knowable cwd; WSL shells get path-paste.
                if op != crate::dragdrop::DropOp::PastePath
                    && t.flavor == crate::term_pane::ShellFlavor::Wsl
                {
                    DROPEFFECT_LINK
                } else {
                    match op {
                        crate::dragdrop::DropOp::PastePath => DROPEFFECT_LINK,
                        crate::dragdrop::DropOp::Copy => DROPEFFECT_COPY,
                        crate::dragdrop::DropOp::Move => DROPEFFECT_MOVE,
                    }
                }
            },
            None => DROPEFFECT_NONE,
        }
    }

    /// Execute a completed file drop (posted from the OLE handler).
    fn do_drop(&mut self, paths: Vec<String>, op: crate::dragdrop::DropOp, p: POINT) {
        use crate::dragdrop::DropOp;
        let s = self.scale();
        let (x, y) = (p.x as f32 / s, p.y as f32 / s);
        let Some((pane_id, _)) = self.pane_at(x, y) else { return };
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab.active != pane_id {
                tab.active = pane_id;
                self.sync_pane_focus();
                self.update_title();
            }
        let hwnd = self.hwnd;
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        match tab.pane_mut(pane_id).map(|p| &mut p.kind) {
            Some(PaneKind::Browser(b)) => {
                let url = format!("file:///{}", paths[0].replace('\\', "/"));
                b.navigate(&url);
            },
            Some(PaneKind::Term(t)) => {
                let mut op = op;
                let cwd = if op == DropOp::PastePath { None } else { t.cwd() };
                if op != DropOp::PastePath && cwd.is_none() {
                    op = DropOp::PastePath; // WSL / unreadable cwd: degrade
                }
                match op {
                    DropOp::PastePath => {
                        let mut text = String::new();
                        for p in &paths {
                            text.push_str(&quote_path(p, t.flavor));
                            text.push(' ');
                        }
                        t.pty.write(text.as_bytes());
                    },
                    DropOp::Copy | DropOp::Move => {
                        shell_file_op(hwnd, &paths, &cwd.unwrap(), op == DropOp::Move);
                    },
                }
            },
            None => {},
        }
        self.invalidate();
    }

    // ----- session save/restore ----------------------------------------------

    /// Serialize this window (None when it has nothing worth saving).
    pub fn snapshot(&self) -> Option<crate::session::WindowState> {
        use crate::session::{LeafState, PaneType, TabState};
        if self.tabs.is_empty() {
            return None;
        }
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rc);
        }
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                let leaves = pane_tree::collect_leaves(&tab.root);
                let active_leaf =
                    leaves.iter().position(|l| *l == tab.active).unwrap_or(0);
                let tree = crate::session::snapshot_node(&tab.root, &|id| {
                    match tab.pane(id).map(|p| &p.kind) {
                        Some(PaneKind::Term(t)) => LeafState {
                            kind: PaneType::Term,
                            profile: Some(t.profile_name.clone()),
                            cwd: t.cwd(),
                            url: None,
                        },
                        Some(PaneKind::Browser(b)) => LeafState {
                            kind: PaneType::Browser,
                            profile: None,
                            cwd: None,
                            url: Some(b.shared.url.lock().unwrap().clone()),
                        },
                        None => LeafState {
                            kind: PaneType::Term,
                            profile: None,
                            cwd: None,
                            url: None,
                        },
                    }
                });
                TabState { font_size: tab.font_size, active_leaf, tree }
            })
            .collect();
        Some(crate::session::WindowState {
            x: rc.left,
            y: rc.top,
            w: rc.right - rc.left,
            h: rc.bottom - rc.top,
            active: self.active,
            tabs,
        })
    }

    /// Rebuild tabs from a saved session state.
    fn restore_tabs(&mut self, state: crate::session::WindowState) {
        use crate::session::PaneType;
        let cfg = app::config();
        let hwnd = self.hwnd;
        let edit_font = self.edit_font;
        for ts in &state.tabs {
            let mut panes: std::collections::HashMap<PaneId, Pane> =
                std::collections::HashMap::new();
            let root = crate::session::rebuild_node(&ts.tree, &mut |leaf| {
                let id = app::next_id();
                let kind = match leaf.kind {
                    PaneType::Term => {
                        let mut profile = cfg
                            .profiles
                            .iter()
                            .find(|p| Some(&p.name) == leaf.profile.as_ref())
                            .cloned()
                            .unwrap_or_else(|| cfg.default_profile().clone());
                        if let Some(cwd) = &leaf.cwd {
                            profile.cwd = Some(cwd.clone());
                            // `wsl --cd ~` would override the restored cwd.
                            if let Some(i) =
                                profile.command.iter().position(|a| a == "--cd")
                            {
                                let end = (i + 2).min(profile.command.len());
                                profile.command.drain(i..end);
                            }
                        }
                        PaneKind::Term(TermPane::spawn(hwnd, id, &profile, 80, 24).ok()?)
                    },
                    PaneType::Browser => PaneKind::Browser(BrowserPane::new(
                        hwnd,
                        id,
                        leaf.url.as_deref().unwrap_or("about:blank"),
                        edit_font,
                    )),
                };
                panes.insert(id, Pane { id, kind });
                Some(id)
            });
            let Some(root) = root else { continue };
            let leaves = pane_tree::collect_leaves(&root);
            let active = leaves.get(ts.active_leaf).copied().unwrap_or(leaves[0]);
            let mut tab = Tab {
                root,
                panes,
                active,
                zoomed: None,
                font_size: ts.font_size,
                fonts: None,
            };
            if (ts.font_size - cfg.font_size).abs() > 0.01 {
                tab.fonts =
                    FontSet::new(&app::gfx(), &self.fonts.family, ts.font_size, self.dpi).ok();
            }
            self.tabs.push(tab);
        }
        if self.tabs.is_empty() {
            self.new_tab();
            return;
        }
        self.switch_tab(state.active.min(self.tabs.len() - 1));
    }

    pub fn repaint(&self) {
        self.invalidate();
    }

    /// Flash a transient announcement chip (≈1.6 s).
    fn show_toast(&mut self, text: String) {
        self.toast = Some(text);
        unsafe {
            SetTimer(Some(self.hwnd), TOAST_TIMER_ID, 1600, None);
        }
        self.invalidate();
    }

    /// Cycle to the next theme in %APPDATA%\baduhan\themes (Ctrl+Shift+S).
    fn cycle_theme(&mut self) {
        let themes = crate::config::list_themes();
        if themes.is_empty() {
            self.show_toast(format!(
                "no themes in {}",
                crate::config::themes_dir().display()
            ));
            return;
        }
        let current = app::config().theme.clone();
        let idx = current
            .and_then(|c| themes.iter().position(|(n, _)| *n == c))
            .map(|i| (i + 1) % themes.len())
            .unwrap_or(0);
        let (name, path) = &themes[idx];
        match crate::config::load_theme_file(path) {
            Some(scheme) => {
                app::apply_theme(name, scheme);
                self.show_toast(format!("\u{1F3A8} {name}  ({}/{})", idx + 1, themes.len()));
            },
            None => self.show_toast(format!("couldn't parse theme '{name}'")),
        }
    }

    fn draw_toast(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx) {
        let Some(text) = &self.toast else { return };
        let (w, _) = self.client_dips();
        let tw = (text.chars().count() as f32 * 8.0 + 40.0).min(w - 20.0);
        let bar = rf((w - tw) / 2.0, TABBAR_H + 14.0, (w + tw) / 2.0, TABBAR_H + 48.0);
        win.rounded(bar, 8.0, palette::d2d_a(palette::CHROME_BG, 0.97));
        win.frame(bar, palette::d2d_a(palette::ACCENT, 0.9), 1.0);
        win.text(
            gfx,
            text,
            &self.fonts.ui,
            rf(bar.left + 16.0, bar.top, bar.right - 8.0, bar.bottom),
            palette::d2d(palette::TAB_TEXT_ACTIVE),
        );
    }

    /// Re-apply a freshly reloaded config: fonts, scheme, dim. Per-tab zoom
    /// sizes are preserved; families/metrics rebuild.
    pub fn apply_config(&mut self) {
        let cfg = app::config();
        if let Ok(f) = FontSet::new(&app::gfx(), &cfg.font_family, cfg.font_size, self.dpi) {
            self.fonts = f;
        }
        for tab in &mut self.tabs {
            if tab.fonts.is_some() {
                tab.fonts =
                    FontSet::new(&app::gfx(), &cfg.font_family, tab.font_size, self.dpi).ok();
            }
        }
        self.relayout();
        self.invalidate();
        self.update_title();
    }

    /// Ctrl+, — open a config file in its associated editor (notepad if none).
    fn open_settings_file(&self, lua: bool) {
        let path = if lua {
            crate::config::Config::path().with_file_name("init.lua")
        } else {
            crate::config::Config::path()
        };
        if lua && !path.exists() {
            let _ = std::fs::write(&path, "-- baduhan init.lua — see README \"Scripting\"\n");
        }
        unsafe {
            let r = windows::Win32::UI::Shell::ShellExecuteW(
                None,
                windows::core::w!("open"),
                &HSTRING::from(path.to_string_lossy().as_ref()),
                None,
                None,
                SW_SHOWNORMAL,
            );
            if r.0 as isize <= 32 {
                // No association for .json/.lua: fall back to notepad.
                let _ = windows::Win32::UI::Shell::ShellExecuteW(
                    None,
                    windows::core::w!("open"),
                    windows::core::w!("notepad.exe"),
                    &HSTRING::from(path.to_string_lossy().as_ref()),
                    None,
                    SW_SHOWNORMAL,
                );
            }
        }
    }

    /// Handle a `baduhan browse/reload/devtools` request from a shell pane.
    /// Returns false when the pane isn't ours (caller tries other windows).
    fn handle_ctl(&mut self, req: &crate::ctl::CtlReq) -> bool {
        let Some(tab_idx) = self.tabs.iter().position(|t| t.find_pane_by_id(req.pane)) else {
            return false;
        };
        let is_active_tab = tab_idx == self.active;
        let tab = &mut self.tabs[tab_idx];
        let browser = tab.panes.values_mut().find_map(|p| match &mut p.kind {
            PaneKind::Browser(b) => Some(b),
            _ => None,
        });
        match (req.verb.as_str(), browser) {
            ("browse", Some(b)) => b.navigate(&req.arg),
            ("browse", None) => {
                // No browser in this tab yet: split one off the requesting
                // pane, without stealing its focus.
                let pane_id = app::next_id();
                let url = browser_pane::normalize_url(&req.arg);
                let mut b = BrowserPane::new(self.hwnd, pane_id, &url, self.edit_font);
                if !is_active_tab {
                    b.show(false);
                }
                if !pane_tree::split(&mut tab.root, req.pane, Dir::Row, pane_id) {
                    return false;
                }
                tab.zoomed = None;
                tab.panes.insert(pane_id, Pane { id: pane_id, kind: PaneKind::Browser(b) });
            },
            ("reload", Some(b)) => b.reload(),
            ("devtools", Some(b)) => b.devtools(),
            _ => return false,
        }
        if is_active_tab {
            self.relayout();
        }
        self.invalidate();
        true
    }

    /// Move the active tab's pane `id` into its own new tab.
    fn pane_to_new_tab(&mut self, id: PaneId) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let (font_size, fonts) = (tab.font_size, tab.fonts.clone());
        let Some(pane) = tab.remove(id) else { return }; // last pane: already its own tab
        let mut new_tab = Tab::single(pane, font_size);
        new_tab.fonts = fonts;
        self.tabs.push(new_tab);
        self.switch_tab(self.tabs.len() - 1);
    }

    fn draw_tab_bar(&self, win: &WindowGfx) {
        let gfx = app::gfx();
        let (w, _) = self.client_dips();
        win.fill(rf(0.0, 0.0, w, TABBAR_H), palette::d2d(palette::CHROME_BG));

        let drag_info = match &self.drag {
            Drag::Tab { idx, press, cur, live: true } => Some((*idx, cur.0 - press.0)),
            _ => None,
        };

        for i in 0..self.tabs.len() {
            // The dragged tab is drawn last (on top, offset).
            if matches!(drag_info, Some((di, _)) if di == i) {
                continue;
            }
            self.draw_tab(win, &gfx, i, 0.0);
        }
        if let Some((i, dx)) = drag_info {
            self.draw_tab(win, &gfx, i, dx);
        }

        // "+" button.
        let pr = self.plus_rect();
        win.rounded(rf(pr.x, pr.y, pr.x + pr.w, pr.y + pr.h), 4.0, palette::d2d(palette::TAB_INACTIVE));
        win.text(
            &gfx,
            "\u{E710}",
            &self.fonts.icons,
            rf(pr.x, pr.y, pr.x + pr.w, pr.y + pr.h),
            palette::d2d(palette::TAB_TEXT),
        );

        // Caption buttons (tabs live in the title bar).
        if self.custom_frame() {
            let (minr, maxr, closer) = self.caption_buttons();
            let zoomed = unsafe { IsZoomed(self.hwnd) }.as_bool();
            let draw_btn = |r: &RectF, glyph: &str, code: u32, red: bool| {
                if self.hot_caption == Some(code) {
                    let bg = if red {
                        palette::rgb(0xC4, 0x2B, 0x1C)
                    } else {
                        palette::TAB_INACTIVE
                    };
                    win.fill(rf(r.x, r.y, r.x + r.w, r.y + r.h), palette::d2d(bg));
                }
                win.text(
                    &gfx,
                    glyph,
                    &self.fonts.icons,
                    rf(r.x, r.y, r.x + r.w, r.y + r.h),
                    palette::d2d(palette::TAB_TEXT_ACTIVE),
                );
            };
            draw_btn(&minr, "\u{E921}", HTMINBUTTON, false);
            draw_btn(&maxr, if zoomed { "\u{E923}" } else { "\u{E922}" }, HTMAXBUTTON, false);
            draw_btn(&closer, "\u{E8BB}", HTCLOSE, true);
        }
    }

    fn draw_tab(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx, i: usize, dx: f32) {
        let r = self.tab_rect(i);
        let r = RectF { x: r.x + dx, ..r };
        let active = i == self.active;
        let bg = if active { palette::TAB_ACTIVE } else { palette::TAB_INACTIVE };
        win.rounded(rf(r.x, r.y, r.x + r.w, r.y + r.h), 6.0, palette::d2d(bg));
        if active {
            win.fill(rf(r.x + 6.0, r.y, r.x + r.w - 6.0, r.y + 2.0), palette::d2d(palette::ACCENT));
        }
        let tab = &self.tabs[i];
        let mut title = self.styled_tab_title(tab);
        if tab.zoomed.is_some() {
            title = format!("\u{2922} {title}");
        }
        let text_color = if active { palette::TAB_TEXT_ACTIVE } else { palette::TAB_TEXT };
        win.text(
            gfx,
            &title,
            &self.fonts.ui,
            rf(r.x + 10.0, r.y, r.x + r.w - 24.0, r.y + r.h),
            palette::d2d(text_color),
        );
        // Close glyph.
        win.text(
            gfx,
            "\u{E711}",
            &self.fonts.icons,
            rf(r.x + r.w - 22.0, r.y, r.x + r.w - 4.0, r.y + r.h),
            palette::d2d_a(text_color, 0.7),
        );
    }

    fn draw_browser_chrome(&self, win: &WindowGfx, r: RectF) {
        let gfx = app::gfx();
        let chrome = browser_chrome(r);
        let toolbar = chrome.toolbar;
        win.fill(
            rf(toolbar.x, toolbar.y, toolbar.x + toolbar.w, toolbar.y + toolbar.h),
            palette::d2d(palette::TOOLBAR_BG),
        );
        let btn = |b: &RectF, glyph: &str| {
            win.text(
                &gfx,
                glyph,
                &self.fonts.icons,
                rf(b.x, b.y, b.x + b.w, b.y + b.h),
                palette::d2d(palette::TAB_TEXT),
            );
        };
        btn(&chrome.back, "\u{E72B}");
        btn(&chrome.fwd, "\u{E72A}");
        btn(&chrome.reload, "\u{E72C}");
        btn(&chrome.dev, "\u{EC7A}");
        btn(&chrome.close, "\u{E711}");
        // Body placeholder until WebView2 arrives.
        win.fill(
            rf(r.x, toolbar.y + toolbar.h, r.x + r.w, r.y + r.h),
            palette::d2d(palette::scheme().bg),
        );
    }

    /// Per-pane title bar: drag handle + title + close glyph.
    fn draw_pane_title(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx, pane: &Pane, r: RectF, active: bool) {
        let bar = rf(r.x, r.y, r.x + r.w, r.y + PANE_TITLE_H);
        win.fill(bar, palette::d2d(palette::TOOLBAR_BG));
        let text_color = if active { palette::TAB_TEXT_ACTIVE } else { palette::TAB_TEXT };
        // Grip dots hint at draggability.
        win.text(
            gfx,
            "\u{E76F}",
            &self.fonts.icons,
            rf(r.x + 4.0, r.y, r.x + 20.0, r.y + PANE_TITLE_H),
            palette::d2d_a(text_color, 0.6),
        );
        win.text(
            gfx,
            &pane.title(),
            &self.fonts.ui,
            rf(r.x + 24.0, r.y, r.x + r.w - 26.0, r.y + PANE_TITLE_H),
            palette::d2d(text_color),
        );
        let cl = title_close_rect(r);
        win.text(
            gfx,
            "\u{E711}",
            &self.fonts.icons,
            rf(cl.x, cl.y, cl.x + cl.w, cl.y + cl.h),
            palette::d2d_a(text_color, 0.7),
        );
    }

    // ----- hit testing -----------------------------------------------------

    fn pane_at(&self, x: f32, y: f32) -> Option<(PaneId, RectF)> {
        let tab = self.tabs.get(self.active)?;
        let lay = tab.layout(self.pane_area());
        lay.panes.iter().find(|(_, r)| r.contains(x, y)).copied()
    }

    fn is_multi(&self) -> bool {
        self.tabs
            .get(self.active)
            .map(|t| t.zoomed.is_none() && t.panes.len() > 1)
            .unwrap_or(false)
    }

    /// Pane under a point plus its *content* rect (sans title bar).
    fn pane_content_at(&self, x: f32, y: f32) -> Option<(PaneId, RectF)> {
        let multi = self.is_multi();
        self.pane_at(x, y).map(|(id, r)| (id, content_rect(r, multi)))
    }

    fn divider_at(&self, x: f32, y: f32) -> Option<pane_tree::Divider> {
        let tab = self.tabs.get(self.active)?;
        let lay = tab.layout(self.pane_area());
        // Generous 2px halo for easier grabbing.
        lay.dividers
            .iter()
            .find(|d| {
                let r = d.rect;
                x >= r.x - 2.0 && x < r.x + r.w + 2.0 && y >= r.y - 2.0 && y < r.y + r.h + 2.0
            })
            .cloned()
    }

    fn tab_at(&self, x: f32, y: f32) -> Option<usize> {
        if y >= TABBAR_H {
            return None;
        }
        (0..self.tabs.len()).find(|&i| self.tab_rect(i).contains(x, y))
    }

    /// Cell under a point inside a terminal pane rect, viewport coords.
    fn cell_at(&self, pane: RectF, x: f32, y: f32) -> (usize, usize, Side) {
        let f = self.cell_fonts();
        let cx = ((x - pane.x - PANE_PAD) / f.cell_w).max(0.0);
        let cy = ((y - pane.y - PANE_PAD) / f.cell_h).max(0.0);
        let side = if cx.fract() < 0.5 { Side::Left } else { Side::Right };
        (cx as usize, cy as usize, side)
    }

    // ----- actions ---------------------------------------------------------

    fn with_active_term<R>(
        &mut self,
        f: impl FnOnce(&TermPane) -> R,
    ) -> Option<R> {
        let tab = self.tabs.get(self.active)?;
        tab.active_term().map(f)
    }

    fn split(&mut self, dir: Dir, browser: bool) {
        let pane_id = app::next_id();
        let kind = if browser {
            PaneKind::Browser(BrowserPane::new(self.hwnd, pane_id, "about:blank", self.edit_font))
        } else {
            let cfg = app::config();
            match TermPane::spawn(self.hwnd, pane_id, cfg.default_profile(), 80, 24) {
                Ok(t) => PaneKind::Term(t),
                Err(_) => return,
            }
        };
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.split(dir, Pane { id: pane_id, kind });
        }
        self.relayout();
        if browser {
            // Land in the URL bar, ready to type a destination.
            if let Some(tab) = self.tabs.get_mut(self.active)
                && let Some(b) = tab.active_browser_mut() {
                    b.focus_url_bar();
                }
        } else {
            self.focus_active_pane();
        }
        self.invalidate();
        self.update_title();
    }

    fn close_active_pane(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let active_pane = tab.active;
        match tab.remove(active_pane) {
            Some(pane) => {
                drop(pane);
                self.relayout();
                self.focus_active_pane();
                self.invalidate();
                self.update_title();
            },
            None => {
                self.close_tab(self.active);
            },
        }
    }

    fn close_pane_by_id(&mut self, pane_id: PaneId) {
        let Some(tab_idx) = self.tabs.iter().position(|t| t.find_pane_by_id(pane_id)) else {
            return;
        };
        let tab = &mut self.tabs[tab_idx];
        match tab.remove(pane_id) {
            Some(pane) => {
                drop(pane);
                if self.focus_pane == Some(pane_id) {
                    self.focus_pane = None;
                }
                if tab_idx == self.active {
                    self.relayout();
                    self.sync_pane_focus();
                }
                self.invalidate();
                self.update_title();
            },
            None => self.close_tab(tab_idx),
        }
    }

    fn focus_dir(&mut self, dx: i32, dy: i32) {
        let area = self.pane_area();
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let lay = tab.layout(area);
        if let Some(next) = pane_tree::neighbor(&lay, tab.active, dx, dy) {
            tab.active = next;
            self.focus_active_pane();
            self.invalidate();
            self.update_title();
        }
    }

    fn zoom_toggle(&mut self) {
        let area = self.pane_area();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.zoomed = if tab.zoomed.is_some() { None } else { Some(tab.active) };
            let _ = area;
        }
        self.relayout();
        self.invalidate();
    }

    fn copy_selection(&mut self) {
        let text = self
            .with_active_term(|t| t.term.lock().selection_to_string())
            .flatten();
        if let Some(text) = text
            && !text.is_empty()
                && let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
                }
    }

    fn paste(&mut self) {
        let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) else {
            return;
        };
        self.with_active_term(|t| {
            let text = text.replace("\r\n", "\r").replace('\n', "\r");
            let bracketed = t.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
            if bracketed {
                t.pty.write(b"\x1b[200~");
                t.pty.write(text.as_bytes());
                t.pty.write(b"\x1b[201~");
            } else {
                t.pty.write(text.as_bytes());
            }
        });
    }

    /// Zoom the *active tab* to `size`; other tabs keep their own zoom.
    fn set_font_size(&mut self, size: f32) {
        let size = size.clamp(7.0, 32.0);
        if (size - self.active_font_size()).abs() < 0.01 {
            return;
        }
        let family = self.fonts.family.clone();
        let default_size = app::config().font_size;
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        tab.font_size = size;
        if (size - default_size).abs() < 0.01 {
            tab.fonts = None; // back on the shared default set
        } else if let Ok(f) = FontSet::new(&app::gfx(), &family, size, self.dpi) {
            tab.fonts = Some(f);
        }
        self.relayout();
        self.invalidate();
    }

    fn move_tab(&mut self, delta: i32) {
        let n = self.tabs.len() as i32;
        if n < 2 {
            return;
        }
        let from = self.active as i32;
        let to = (from + delta).rem_euclid(n);
        let tab = self.tabs.remove(from as usize);
        self.tabs.insert(to as usize, tab);
        self.active = to as usize;
        self.invalidate();
    }

    fn detach_tab(&mut self, idx: usize, pos: Option<(i32, i32)>) {
        if idx >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            // Detaching the only tab is just moving the window.
            if let Some((x, y)) = pos {
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        x - 60,
                        y - 14,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER,
                    );
                }
            }
            return;
        }
        let tab = self.take_tab(idx);
        app::create_window(Some(tab), pos);
    }

    // ----- input: keyboard ---------------------------------------------------

    fn on_key_down(&mut self, vk: u16) -> bool {
        self.suppress_char = false;
        let mods = Mods::current();

        // Shortcut overlay is modal: any non-modifier key dismisses it.
        if self.cheatsheet {
            if !matches!(VIRTUAL_KEY(vk), VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN)
            {
                self.cheatsheet = false;
                self.suppress_char = true;
                self.invalidate();
            }
            return true;
        }

        if self.hotkey(vk, &mods) {
            return true;
        }

        // Modal overlays swallow input while open.
        if self.palette.is_some() {
            return self.palette_key(vk);
        }
        if self.hints.is_some() {
            return self.hints_key(vk);
        }
        if self.search.is_some() {
            return self.search_key(vk, &mods);
        }

        // Scrollback paging.
        if mods.shift && !mods.ctrl && !mods.alt {
            let vkk = VIRTUAL_KEY(vk);
            if vkk == VK_PRIOR || vkk == VK_NEXT {
                let handled = self
                    .with_active_term(|t| {
                        let mut term = t.term.lock();
                        if term.mode().contains(TermMode::ALT_SCREEN) {
                            false
                        } else {
                            term.scroll_display(if vkk == VK_PRIOR {
                                Scroll::PageUp
                            } else {
                                Scroll::PageDown
                            });
                            true
                        }
                    })
                    .unwrap_or(false);
                if handled {
                    self.invalidate();
                    return true;
                }
            }
        }

        // VT encoding for the active terminal.
        let mode = self
            .with_active_term(|t| *t.term.lock().mode())
            .unwrap_or(TermMode::empty());
        if let Some(bytes) = keys::encode_key(vk, &mods, mode) {
            self.with_active_term(|t| {
                {
                    let mut term = t.term.lock();
                    if term.grid().display_offset() != 0 {
                        term.scroll_display(Scroll::Bottom);
                    }
                }
                t.pty.write(&bytes);
            });
            self.invalidate();
            return true;
        }
        false
    }

    // ----- hyperlinks --------------------------------------------------------

    /// URL under a viewport cell: an OSC 8 hyperlink on the cell, or a plain
    /// URL token detected in the row text around it.
    fn link_at(&self, pane_id: PaneId, prect: RectF, x: f32, y: f32) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.pane(pane_id)?;
        let PaneKind::Term(t) = &pane.kind else { return None };
        let (col, row, _) = self.cell_at(prect, x, y);
        let term = t.term.lock();
        let cols = term.grid().columns();
        let lines = term.grid().screen_lines();
        if col >= cols || row >= lines {
            return None;
        }
        let display_offset = term.grid().display_offset() as i32;
        let line = Line(row as i32 - display_offset);
        let cell = &term.grid()[line][Column(col)];
        if let Some(h) = cell.hyperlink() {
            return Some(h.uri().to_string());
        }
        // Plain-text URL: walk the row, find the whitespace-delimited token
        // covering the clicked column.
        let mut text = String::new();
        let mut cell_to_char: Vec<usize> = Vec::with_capacity(cols);
        for cx in 0..cols {
            let c = &term.grid()[line][Column(cx)];
            cell_to_char.push(text.chars().count());
            if c.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(c.c);
        }
        drop(term);
        let chars: Vec<char> = text.chars().collect();
        let pos = *cell_to_char.get(col)?;
        if pos >= chars.len() || chars[pos].is_whitespace() {
            return None;
        }
        let mut start = pos;
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let mut end = pos;
        while end + 1 < chars.len() && !chars[end + 1].is_whitespace() {
            end += 1;
        }
        let token: String = chars[start..=end].iter().collect();
        let token = token.trim_end_matches(['.', ',', ';', ')', ']', '>', '"', '\'']);
        for scheme in ["https://", "http://", "file://"] {
            if let Some(i) = token.find(scheme) {
                return Some(token[i..].to_string());
            }
        }
        if token.starts_with("www.") && token.contains('.') {
            return Some(format!("https://{token}"));
        }
        None
    }

    fn open_url(&self, url: &str) {
        unsafe {
            windows::Win32::UI::Shell::ShellExecuteW(
                None,
                windows::core::w!("open"),
                &HSTRING::from(url),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }

    // ----- prompt marks (OSC 133) -------------------------------------------

    /// Jump between shell prompts in scrollback. dir < 0 = older.
    fn prompt_jump(&mut self, dir: i32) {
        use std::sync::atomic::Ordering;
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(t) = tab.active_term() else { return };
        let lines_now = t.shared.lines_seen.load(Ordering::Relaxed);
        let marks: Vec<u64> = t.shared.marks.lock().unwrap().iter().copied().collect();
        if marks.is_empty() {
            return;
        }
        let mut term = t.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let cur = term.grid().display_offset() as i64;
        let cursor_row = term.grid().cursor.point.line.0 as i64;
        let history = term.grid().history_size() as i64;
        // A mark with clock M sits (lines_now - M) wrapped lines above the
        // cursor; this display_offset puts it at the viewport top.
        let offset_of = |m: u64| (lines_now.saturating_sub(m)) as i64 - cursor_row;
        let target = if dir < 0 {
            // Older: smallest reachable offset strictly above the current one.
            marks
                .iter()
                .rev()
                .map(|m| offset_of(*m))
                .find(|o| *o > cur && *o <= history)
        } else {
            // Newer: largest offset strictly below the current one (offsets
            // descend as marks get newer), else snap to the live bottom.
            marks
                .iter()
                .map(|m| offset_of(*m))
                .find(|o| *o < cur && *o >= 0)
                .or(if cur > 0 { Some(0) } else { None })
        };
        if let Some(o) = target {
            term.scroll_display(Scroll::Delta((o - cur) as i32));
            drop(term);
            self.invalidate();
        }
    }

    // ----- quick-select hints -----------------------------------------------

    fn open_hints(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(t) = tab.active_term() else { return };
        let term = t.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let cols = term.grid().columns();
        let lines = term.grid().screen_lines();
        let mut rows: Vec<(String, Vec<usize>)> = Vec::with_capacity(lines);
        for r in 0..lines {
            let line = Line(r as i32 - display_offset);
            let mut text = String::new();
            let mut col_map = Vec::new();
            for c in 0..cols {
                let cell = &term.grid()[line][Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                col_map.push(c);
                text.push(cell.c);
            }
            rows.push((text, col_map));
        }
        drop(term);
        let matches = crate::hints::scan(&rows);
        if matches.is_empty() {
            return;
        }
        self.hints = Some(HintsUi { matches, typed: String::new() });
        self.invalidate();
    }

    fn hints_key(&mut self, vk: u16) -> bool {
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE => {
                self.hints = None;
                self.invalidate();
            },
            VK_BACK => {
                if let Some(h) = &mut self.hints {
                    h.typed.pop();
                }
                self.invalidate();
            },
            _ => {},
        }
        true
    }

    fn hints_char(&mut self, ch: char) {
        let Some(h) = &mut self.hints else { return };
        if !ch.is_ascii_graphic() {
            return;
        }
        h.typed.push(ch.to_ascii_lowercase());
        let typed = h.typed.clone();
        let exact = h.matches.iter().find(|m| m.label == typed).map(|m| m.text.clone());
        let any_prefix = h.matches.iter().any(|m| m.label.starts_with(&typed));
        if let Some(text) = exact {
            self.hints = None;
            self.with_active_term(|t| t.pty.write(text.as_bytes()));
        } else if !any_prefix {
            self.hints = None;
        }
        self.invalidate();
    }

    fn draw_hints(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx) {
        let Some(h) = &self.hints else { return };
        let Some(tab) = self.tabs.get(self.active) else { return };
        let lay = tab.layout(self.pane_area());
        let multi = lay.panes.len() > 1;
        let Some(r) = lay.rect_of(tab.active) else { return };
        let c = content_rect(r, multi);
        let f = tab.fonts.as_ref().unwrap_or(&self.fonts);
        for m in &h.matches {
            if !m.label.starts_with(h.typed.as_str()) {
                continue;
            }
            let x = c.x + PANE_PAD + m.col as f32 * f.cell_w;
            let y = c.y + PANE_PAD + m.row as f32 * f.cell_h;
            let w = 8.0 + m.label.len() as f32 * 8.0;
            let chip = rf(x, y - 2.0, x + w, y + 16.0);
            win.rounded(chip, 3.0, palette::d2d(palette::ACCENT));
            win.text(
                gfx,
                &m.label,
                &self.fonts.ui,
                rf(chip.left + 4.0, chip.top, chip.right, chip.bottom),
                palette::d2d(palette::rgb(0x10, 0x10, 0x18)),
            );
        }
    }

    // ----- command palette ---------------------------------------------------

    fn open_palette(&mut self) {
        let themes: Vec<String> =
            crate::config::list_themes().into_iter().map(|(n, _)| n).collect();
        let items = crate::command_palette::items(&app::config().profiles, &themes);
        let filtered = crate::command_palette::filter(&items, "");
        self.palette = Some(PaletteUi { items, query: String::new(), filtered, selected: 0 });
        self.invalidate();
    }

    fn palette_refresh(&mut self) {
        if let Some(p) = &mut self.palette {
            p.filtered = crate::command_palette::filter(&p.items, &p.query);
            p.selected = 0;
        }
        self.invalidate();
    }

    fn palette_key(&mut self, vk: u16) -> bool {
        match VIRTUAL_KEY(vk) {
            VK_ESCAPE => {
                self.palette = None;
                self.invalidate();
            },
            VK_UP => {
                if let Some(p) = &mut self.palette {
                    p.selected = p.selected.saturating_sub(1);
                }
                self.invalidate();
            },
            VK_DOWN => {
                if let Some(p) = &mut self.palette {
                    p.selected =
                        (p.selected + 1).min(p.filtered.len().saturating_sub(1)).min(9);
                }
                self.invalidate();
            },
            VK_BACK => {
                if let Some(p) = &mut self.palette {
                    p.query.pop();
                }
                self.palette_refresh();
            },
            VK_RETURN => {
                let action = self.palette.as_ref().and_then(|p| {
                    p.filtered.get(p.selected).map(|i| p.items[*i].action.clone())
                });
                self.palette = None;
                self.invalidate();
                if let Some(a) = action {
                    self.run_palette_action(a);
                }
            },
            _ => {},
        }
        true
    }

    fn palette_char(&mut self, ch: char) {
        if ch < ' ' || ch == '\x7f' {
            return;
        }
        if let Some(p) = &mut self.palette {
            p.query.push(ch);
        }
        self.palette_refresh();
    }

    fn run_palette_action(&mut self, a: crate::command_palette::PaletteAction) {
        use crate::command_palette::PaletteAction as A;
        match a {
            A::NewTabProfile(i) => self.new_tab_with_profile(i),
            A::Split(dir) => self.split(dir, false),
            A::BrowserSplit => self.split(Dir::Row, true),
            A::ClosePane => self.close_active_pane(),
            A::Zoom => self.zoom_toggle(),
            A::DetachTab => self.detach_tab(self.active, None),
            A::PaneToNewTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    self.pane_to_new_tab(tab.active);
                }
            },
            A::NewWindow => app::create_window(None, None),
            A::FontBigger => self.set_font_size(self.active_font_size() + 1.0),
            A::FontSmaller => self.set_font_size(self.active_font_size() - 1.0),
            A::FontReset => self.set_font_size(app::config().font_size),
            A::Search => self.toggle_search(),
            A::MoveTabLeft => self.move_tab(-1),
            A::MoveTabRight => self.move_tab(1),
            A::PromptPrev => self.prompt_jump(-1),
            A::PromptNext => self.prompt_jump(1),
            A::Hints => self.open_hints(),
            A::Cheatsheet => self.cheatsheet = true,
            A::OpenSettings => self.open_settings_file(false),
            A::OpenInitLua => self.open_settings_file(true),
            A::ThemeNext => self.cycle_theme(),
            A::Theme(name) => {
                let theme = crate::config::list_themes()
                    .into_iter()
                    .find(|(n, _)| *n == name)
                    .and_then(|(n, p)| crate::config::load_theme_file(&p).map(|s| (n, s)));
                match theme {
                    Some((n, scheme)) => {
                        app::apply_theme(&n, scheme);
                        self.show_toast(format!("\u{1F3A8} {n}"));
                    },
                    None => self.show_toast(format!("couldn't load theme '{name}'")),
                }
            },
        }
    }

    fn draw_palette(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx) {
        let Some(p) = &self.palette else { return };
        let (w, _) = self.client_dips();
        let pw = 480.0_f32.min(w - 40.0);
        let x0 = (w - pw) / 2.0;
        let y0 = TABBAR_H + 10.0;
        let row_h = 26.0;
        let shown = p.filtered.len().min(10);
        let box_h = 34.0 + shown as f32 * row_h + 6.0;
        let bx = rf(x0, y0, x0 + pw, y0 + box_h);
        win.rounded(bx, 6.0, palette::d2d_a(palette::CHROME_BG, 0.98));
        win.frame(bx, palette::d2d_a(palette::ACCENT, 0.8), 1.0);
        win.text(
            gfx,
            &format!("\u{276F} {}\u{2595}", p.query),
            &self.fonts.ui,
            rf(x0 + 12.0, y0 + 4.0, x0 + pw - 12.0, y0 + 30.0),
            palette::d2d(palette::TAB_TEXT_ACTIVE),
        );
        for (vis, idx) in p.filtered.iter().take(10).enumerate() {
            let item = &p.items[*idx];
            let ry = y0 + 34.0 + vis as f32 * row_h;
            let rr = rf(x0 + 4.0, ry, x0 + pw - 4.0, ry + row_h - 2.0);
            if vis == p.selected {
                win.rounded(rr, 4.0, palette::d2d_a(palette::ACCENT, 0.30));
            }
            win.text(
                gfx,
                &item.label,
                &self.fonts.ui,
                rf(rr.left + 8.0, rr.top, rr.right - 120.0, rr.bottom),
                palette::d2d(palette::TAB_TEXT_ACTIVE),
            );
            win.text(
                gfx,
                item.hint,
                &self.fonts.ui,
                rf(rr.right - 150.0, rr.top, rr.right - 8.0, rr.bottom),
                palette::d2d_a(palette::TAB_TEXT, 0.7),
            );
        }
    }

    // ----- keyboard-shortcut overlay -----------------------------------------

    fn toggle_cheatsheet(&mut self) {
        self.cheatsheet = !self.cheatsheet;
        // Ctrl+/ produces a control char (0x1F) on US layouts; don't let it
        // reach the shell.
        self.suppress_char = true;
        self.invalidate();
    }

    fn draw_cheatsheet(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx) {
        if !self.cheatsheet {
            return;
        }
        let (w, h) = self.client_dips();
        win.fill(rf(0.0, 0.0, w, h), palette::d2d_a(palette::rgb(0, 0, 0), 0.45));

        const ROW_H: f32 = 24.0;
        const TITLE_H: f32 = 30.0;
        const SECTION_GAP: f32 = 10.0;
        const COL_W: f32 = 352.0;
        const KEYS_W: f32 = 168.0;
        const PAD: f32 = 18.0;

        let lua_binds = crate::scripting::list_keybinds();
        let col_height = |sections: &[&crate::cheatsheet::Section], extra: usize| {
            sections
                .iter()
                .map(|s| TITLE_H + s.entries.len() as f32 * ROW_H + SECTION_GAP)
                .sum::<f32>()
                + if extra > 0 {
                    TITLE_H + extra as f32 * ROW_H + SECTION_GAP
                } else {
                    0.0
                }
        };
        let cols = crate::cheatsheet::COLUMNS;
        // init.lua bindings join the shorter (left) column.
        let h0 = col_height(cols[0], lua_binds.len());
        let h1 = col_height(cols[1], 0);

        let pw = (PAD + COL_W + PAD + COL_W + PAD).min(w - 24.0);
        let ph = h0.max(h1) + 44.0 + 30.0;
        let x0 = (w - pw) / 2.0;
        let y0 = (TABBAR_H + (h - TABBAR_H - ph) / 2.0).max(TABBAR_H + 8.0);
        let bx = rf(x0, y0, x0 + pw, y0 + ph);
        win.rounded(bx, 8.0, palette::d2d_a(palette::CHROME_BG, 0.98));
        win.frame(bx, palette::d2d_a(palette::ACCENT, 0.8), 1.0);
        win.text(
            gfx,
            "Keyboard shortcuts",
            &self.fonts.ui,
            rf(x0 + PAD, y0 + 10.0, x0 + pw - PAD, y0 + 36.0),
            palette::d2d(palette::TAB_TEXT_ACTIVE),
        );

        let draw_section =
            |title: &str, rows: &mut dyn Iterator<Item = (String, String)>, cx: f32, cy: &mut f32| {
                win.text(
                    gfx,
                    title,
                    &self.fonts.ui,
                    rf(cx, *cy + 4.0, cx + COL_W, *cy + TITLE_H),
                    palette::d2d(palette::ACCENT),
                );
                *cy += TITLE_H;
                for (keys, action) in rows {
                    win.text(
                        gfx,
                        &keys,
                        &self.fonts.ui,
                        rf(cx, *cy, cx + KEYS_W, *cy + ROW_H),
                        palette::d2d(palette::TAB_TEXT_ACTIVE),
                    );
                    win.text(
                        gfx,
                        &action,
                        &self.fonts.ui,
                        rf(cx + KEYS_W + 8.0, *cy, cx + COL_W, *cy + ROW_H),
                        palette::d2d_a(palette::TAB_TEXT, 0.9),
                    );
                    *cy += ROW_H;
                }
                *cy += SECTION_GAP;
            };

        for (ci, col) in cols.iter().enumerate() {
            let cx = x0 + PAD + ci as f32 * (COL_W + PAD);
            let mut cy = y0 + 44.0;
            for s in col.iter() {
                let mut rows = s
                    .entries
                    .iter()
                    .map(|en| (en.keys.to_string(), en.action.to_string()));
                draw_section(s.title, &mut rows, cx, &mut cy);
            }
            if ci == 0 && !lua_binds.is_empty() {
                let mut rows =
                    lua_binds.iter().map(|k| (k.clone(), "init.lua keybinding".to_string()));
                draw_section("Yours (init.lua)", &mut rows, cx, &mut cy);
            }
        }

        win.text(
            gfx,
            "press any key to dismiss",
            &self.fonts.ui,
            rf(x0 + PAD, y0 + ph - 28.0, x0 + pw - PAD, y0 + ph - 6.0),
            palette::d2d_a(palette::TAB_TEXT, 0.55),
        );
    }

    // ----- scrollback search -----------------------------------------------

    fn toggle_search(&mut self) {
        if self.search.is_some() {
            self.close_search();
            return;
        }
        // Prefill from a single-line selection, iTerm2-style.
        let prefill = self
            .with_active_term(|t| t.term.lock().selection_to_string())
            .flatten()
            .filter(|s| !s.is_empty() && !s.contains('\n'))
            .map(|s| regex_escape(&s))
            .unwrap_or_default();
        self.search = Some(SearchUi { query: prefill, dfas: None, found: None });
        self.search_refresh();
    }

    fn close_search(&mut self) {
        self.search = None;
        self.invalidate();
    }

    /// Recompile the query and find the most recent match at/above the
    /// bottom of the viewport.
    fn search_refresh(&mut self) {
        if let Some(s) = &mut self.search {
            s.dfas = if s.query.is_empty() { None } else { RegexSearch::new(&s.query).ok() };
            s.found = None;
            if s.dfas.is_none() {
                self.with_active_term(|t| t.term.lock().selection = None);
                self.invalidate();
                return;
            }
        }
        self.run_search(None, Direction::Left);
    }

    /// Jump to the next match in `dir` from the current one.
    fn search_step(&mut self, dir: Direction) {
        let origin = self.search.as_ref().and_then(|s| s.found.clone()).map(|m| match dir {
                Direction::Left => *m.start(),
                Direction::Right => *m.end(),
            });
        self.run_search(origin, dir);
    }

    /// Core search: from `origin` (None = viewport bottom; Some = step one
    /// cell past it first), select + scroll to the match.
    fn run_search(&mut self, origin: Option<Point>, dir: Direction) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(t) = tab.active_term() else { return };
        let Some(s) = &mut self.search else { return };
        let Some(dfas) = &mut s.dfas else { return };

        let mut term = t.term.lock();
        let origin = match origin {
            Some(p) => match dir {
                Direction::Left => p.sub(term.grid(), Boundary::Grid, 1),
                Direction::Right => p.add(term.grid(), Boundary::Grid, 1),
            },
            None => {
                let d = term.grid().display_offset() as i32;
                Point::new(
                    Line(term.screen_lines() as i32 - 1 - d),
                    Column(term.grid().columns() - 1),
                )
            },
        };
        let side = match dir {
            Direction::Right => Side::Left,
            Direction::Left => Side::Right,
        };
        let m = term.search_next(dfas, origin, dir, side, None);
        match &m {
            Some(m) => {
                term.selection =
                    Some(Selection::new(SelectionType::Simple, *m.start(), Side::Left));
                if let Some(sel) = &mut term.selection {
                    sel.update(*m.end(), Side::Right);
                }
                // Scroll the match's line into view.
                let line = m.start().line.0;
                let d = term.grid().display_offset() as i32;
                let screen = term.screen_lines() as i32;
                if line < -d {
                    term.scroll_display(Scroll::Delta(-d - line));
                } else if line > screen - 1 - d {
                    term.scroll_display(Scroll::Delta(screen - 1 - d - line));
                }
            },
            None => term.selection = None,
        }
        drop(term);
        s.found = m;
        self.invalidate();
    }

    /// Keys while the search bar is open. Returns true when consumed.
    fn search_key(&mut self, vk: u16, m: &Mods) -> bool {
        let vkk = VIRTUAL_KEY(vk);
        match vkk {
            VK_ESCAPE => self.close_search(),
            VK_RETURN | VK_F3 => {
                self.search_step(if m.shift { Direction::Right } else { Direction::Left });
            },
            VK_UP => self.search_step(Direction::Left),
            VK_DOWN => self.search_step(Direction::Right),
            VK_BACK => {
                if let Some(s) = &mut self.search {
                    s.query.pop();
                }
                self.search_refresh();
            },
            _ if m.ctrl && vk as u8 == b'V' => {
                if let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text())
                    && let Some(line) = text.lines().next()
                    && let Some(s) = &mut self.search
                {
                    s.query.push_str(line);
                    self.search_refresh();
                }
                self.suppress_char = true;
            },
            // Everything else is blocked from the shell; printable chars
            // arrive via WM_CHAR and edit the query there.
            _ => {},
        }
        true
    }

    fn draw_search_bar(&self, win: &WindowGfx, gfx: &crate::renderer::Gfx) {
        let Some(s) = &self.search else { return };
        let Some(tab) = self.tabs.get(self.active) else { return };
        let lay = tab.layout(self.pane_area());
        let multi = lay.panes.len() > 1;
        let Some(r) = lay.rect_of(tab.active) else { return };
        let c = content_rect(r, multi);
        let w = 280.0_f32.min(c.w - 16.0);
        let bar = rf(c.x + c.w - w - 8.0, c.y + 6.0, c.x + c.w - 8.0, c.y + 34.0);
        win.rounded(bar, 5.0, palette::d2d_a(palette::TOOLBAR_BG, 0.97));
        let miss = !s.query.is_empty() && s.found.is_none();
        let border = if miss { palette::rgb(0xE7, 0x48, 0x56) } else { palette::ACCENT };
        win.frame(bar, palette::d2d_a(border, 0.9), 1.0);
        win.text(
            gfx,
            "\u{E721}", // magnifier
            &self.fonts.icons,
            rf(bar.left + 4.0, bar.top, bar.left + 24.0, bar.bottom),
            palette::d2d(palette::TAB_TEXT),
        );
        let text = format!("{}\u{2595}", s.query);
        win.text(
            gfx,
            &text,
            &self.fonts.ui,
            rf(bar.left + 28.0, bar.top, bar.right - 6.0, bar.bottom),
            palette::d2d(if miss { palette::rgb(0xE7, 0x48, 0x56) } else { palette::TAB_TEXT_ACTIVE }),
        );
    }

    /// Execute an action queued by a Lua keybind callback.
    fn run_action(&mut self, a: crate::scripting::Action) {
        use crate::scripting::Action;
        match a {
            Action::NewTab(None) => self.new_tab(),
            Action::NewTab(Some(name)) => {
                let idx = app::config().profiles.iter().position(|p| p.name == name);
                match idx {
                    Some(i) => self.new_tab_with_profile(i),
                    None => eprintln!("init.lua: unknown profile '{name}'"),
                }
            },
            Action::Split(dir) => self.split(dir, false),
            Action::Browse(url) => {
                if let Some(pane) = self.tabs.get(self.active).map(|t| t.active) {
                    let req =
                        crate::ctl::CtlReq { pane, verb: "browse".into(), arg: url };
                    self.handle_ctl(&req);
                }
            },
            Action::FontDelta(d) => self.set_font_size(self.active_font_size() + d),
            Action::SendText(s) => {
                self.with_active_term(|t| t.pty.write(s.as_bytes()));
            },
        }
    }

    fn hotkey(&mut self, vk: u16, m: &Mods) -> bool {
        // User keybindings from init.lua run first and may shadow built-ins.
        if let Some(actions) = crate::scripting::handle_key(vk, m) {
            for a in actions {
                self.run_action(a);
            }
            if (vk as u8).is_ascii_alphanumeric() {
                self.suppress_char = true;
            }
            return true;
        }

        let vkk = VIRTUAL_KEY(vk);

        if m.ctrl && m.shift && !m.alt {
            let letter = (vk as u8).is_ascii_uppercase();
            match vk as u8 {
                b'T' => {
                    self.new_tab();
                },
                b'N' => {
                    app::create_window(None, None);
                },
                b'W' => {
                    self.close_active_pane();
                },
                b'D' => self.split(Dir::Row, false),
                b'E' => self.split(Dir::Col, false),
                b'B' => self.split(Dir::Row, true),
                b'M' => {
                    self.detach_tab(self.active, None);
                },
                b'C' => self.copy_selection(),
                b'V' => self.paste(),
                b'F' => self.toggle_search(),
                b'P' => self.open_palette(),
                b'S' => self.cycle_theme(),
                c @ b'1'..=b'9' => {
                    // New tab with profile N (Windows Terminal muscle memory).
                    self.new_tab_with_profile((c - b'1') as usize);
                },
                _ => match vkk {
                    VK_TAB => {
                        let n = self.tabs.len();
                        self.switch_tab((self.active + n - 1) % n.max(1));
                    },
                    VK_PRIOR => self.move_tab(-1),
                    VK_NEXT => self.move_tab(1),
                    VK_RETURN => self.zoom_toggle(),
                    VK_UP => self.prompt_jump(-1),
                    VK_DOWN => self.prompt_jump(1),
                    VK_SPACE => self.open_hints(),
                    VK_OEM_2 => self.toggle_cheatsheet(), // Ctrl+Shift+/ = Ctrl+?
                    _ => return false,
                },
            }
            if letter {
                self.suppress_char = true;
            }
            return true;
        }

        if m.ctrl && !m.shift && !m.alt {
            match vkk {
                VK_TAB => {
                    let n = self.tabs.len().max(1);
                    self.switch_tab((self.active + 1) % n);
                    return true;
                },
                VK_OEM_COMMA => {
                    // Ctrl+, opens settings (Windows Terminal muscle memory);
                    // changes hot-reload on save.
                    self.open_settings_file(false);
                    return true;
                },
                VK_OEM_PLUS => {
                    self.set_font_size(self.active_font_size() + 1.0);
                    return true;
                },
                VK_OEM_MINUS => {
                    self.set_font_size(self.active_font_size() - 1.0);
                    return true;
                },
                _ => {},
            }
            let c = vk as u8;
            // Ctrl+L: focus URL bar — only when a browser pane is active, so
            // terminals keep their clear-screen.
            if c == b'L' {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && let Some(b) = tab.active_browser_mut() {
                        b.focus_url_bar();
                        self.suppress_char = true;
                        return true;
                    }
                return false;
            }
            if c == b'0' {
                self.set_font_size(app::config().font_size);
                return true;
            }
            if (b'1'..=b'9').contains(&c) {
                let i = (c - b'1') as usize;
                let i = if c == b'9' { self.tabs.len().saturating_sub(1) } else { i };
                if i < self.tabs.len() {
                    self.switch_tab(i);
                }
                return true;
            }
            return false;
        }

        // Alt+1..9: go to tab N, 9 = last (iTerm2 muscle memory). Costs the
        // shell ESC+digit, which Lua keybinds can reclaim per key.
        if m.alt && !m.ctrl && !m.shift {
            let c = vk as u8;
            if (b'1'..=b'9').contains(&c) {
                let i = (c - b'1') as usize;
                let i = if c == b'9' { self.tabs.len().saturating_sub(1) } else { i };
                if i < self.tabs.len() {
                    self.switch_tab(i);
                }
                self.suppress_char = true; // swallow the WM_SYSCHAR
                return true;
            }
        }

        if m.ctrl && m.alt {
            match vkk {
                VK_LEFT => {
                    self.focus_dir(-1, 0);
                    return true;
                },
                VK_RIGHT => {
                    self.focus_dir(1, 0);
                    return true;
                },
                VK_UP => {
                    self.focus_dir(0, -1);
                    return true;
                },
                VK_DOWN => {
                    self.focus_dir(0, 1);
                    return true;
                },
                _ => {},
            }
        }

        if vkk == VK_F12
            && let Some(tab) = self.tabs.get_mut(self.active)
                && let Some(b) = tab.active_browser_mut() {
                    b.devtools();
                    return true;
                }

        false
    }

    fn on_char(&mut self, code: u16) {
        if self.suppress_char {
            self.suppress_char = false;
            return;
        }
        // Handled in WM_KEYDOWN already.
        if matches!(code, 0x0d | 0x09 | 0x08 | 0x1b) {
            return;
        }
        if self.palette.is_some() {
            if let Some(ch) = char::from_u32(code as u32) {
                self.palette_char(ch);
            }
            return;
        }
        if self.hints.is_some() {
            if let Some(ch) = char::from_u32(code as u32) {
                self.hints_char(ch);
            }
            return;
        }
        // Search bar edits its query instead of feeding the shell.
        if self.search.is_some() {
            if code >= 0x20 && code != 0x7f
                && let Some(ch) = char::from_u32(code as u32)
            {
                if let Some(s) = &mut self.search {
                    s.query.push(ch);
                }
                self.search_refresh();
            }
            return;
        }
        // Surrogate pairs arrive as two WM_CHARs.
        let ch = if (0xD800..0xDC00).contains(&code) {
            self.pending_surrogate = Some(code);
            return;
        } else if (0xDC00..0xE000).contains(&code) {
            let Some(high) = self.pending_surrogate.take() else { return };
            let c = 0x10000 + (((high as u32) - 0xD800) << 10) + ((code as u32) - 0xDC00);
            match char::from_u32(c) {
                Some(c) => c,
                None => return,
            }
        } else {
            match char::from_u32(code as u32) {
                Some(c) => c,
                None => return,
            }
        };
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        let bytes = s.as_bytes().to_vec();
        self.with_active_term(|t| {
            {
                let mut term = t.term.lock();
                if term.grid().display_offset() != 0 {
                    term.scroll_display(Scroll::Bottom);
                }
            }
            t.pty.write(&bytes);
        });
    }

    // ----- input: mouse ------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn mouse_report(
        &mut self,
        pane_id: PaneId,
        pane_rect: RectF,
        x: f32,
        y: f32,
        button: u8,
        pressed: bool,
        motion: bool,
        mods: &Mods,
    ) -> bool {
        let Some(tab) = self.tabs.get(self.active) else { return false };
        let Some(pane) = tab.pane(pane_id) else { return false };
        let PaneKind::Term(t) = &pane.kind else { return false };
        let term = t.term.lock();
        let mode = *term.mode();
        drop(term);

        if !mode.intersects(TermMode::MOUSE_MODE) || mods.shift {
            return false;
        }
        if motion && !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            return true; // mouse mode active but motion not requested
        }

        let (col, row, _) = self.cell_at(pane_rect, x, y);
        let size = *t.shared.size.lock().unwrap();
        let col = col.min(size.num_cols.saturating_sub(1) as usize);
        let row = row.min(size.num_lines.saturating_sub(1) as usize);

        let mut b = button as u32;
        if motion {
            b += 32;
        }
        if mods.shift {
            b += 4;
        }
        if mods.alt {
            b += 8;
        }
        if mods.ctrl {
            b += 16;
        }

        let seq = if mode.contains(TermMode::SGR_MOUSE) {
            format!(
                "\x1b[<{};{};{}{}",
                b,
                col + 1,
                row + 1,
                if pressed { 'M' } else { 'm' }
            )
            .into_bytes()
        } else {
            let b = if pressed { b } else { 3 };
            vec![
                0x1b,
                b'[',
                b'M',
                (32 + b).min(255) as u8,
                (33 + col).min(223) as u8,
                (33 + row).min(223) as u8,
            ]
        };
        t.pty.write(&seq);
        true
    }

    fn on_lbutton_down(&mut self, x: f32, y: f32) {
        unsafe {
            SetCapture(self.hwnd);
        }

        if self.cheatsheet {
            self.cheatsheet = false;
            self.invalidate();
            return;
        }

        // Tab bar.
        if y < TABBAR_H {
            if let Some(i) = self.tab_at(x, y) {
                let r = self.tab_rect(i);
                // Close glyph zone.
                if x > r.x + r.w - 24.0 {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.close_tab(i);
                    return;
                }
                self.switch_tab(i);
                self.drag = Drag::Tab { idx: i, press: (x, y), cur: (x, y), live: false };
                return;
            }
            let pr = self.plus_rect();
            if pr.contains(x, y) {
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.new_tab();
                return;
            }
            unsafe {
                let _ = ReleaseCapture();
            }
            return;
        }

        // Dividers.
        if let Some(d) = self.divider_at(x, y) {
            self.drag = Drag::Divider { path: d.path, index: d.index, dir: d.dir, last: (x, y) };
            return;
        }

        // Panes.
        if let Some((pane_id, prect)) = self.pane_at(x, y) {
            let multi = self.is_multi();

            // Pane title bar: close glyph or start a rearrange drag.
            if multi && y < prect.y + PANE_TITLE_H {
                if title_close_rect(prect).contains(x, y) {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.close_pane_by_id(pane_id);
                    return;
                }
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.active != pane_id {
                        tab.active = pane_id;
                        self.sync_pane_focus();
                        self.update_title();
                        self.invalidate();
                    }
                self.drag = Drag::Pane { id: pane_id, press: (x, y), cur: (x, y), live: false };
                return;
            }
            let crect = content_rect(prect, multi);

            if let Some(tab) = self.tabs.get_mut(self.active)
                && tab.active != pane_id {
                    tab.active = pane_id;
                    self.sync_pane_focus();
                    self.update_title();
                    self.invalidate();
                }

            let is_browser = {
                let tab = &self.tabs[self.active];
                matches!(tab.pane(pane_id).map(|p| &p.kind), Some(PaneKind::Browser(_)))
            };
            if is_browser {
                let chrome = browser_chrome(crect);
                let mut close = false;
                let tab = &mut self.tabs[self.active];
                if let Some(PaneKind::Browser(b)) = tab.pane_mut(pane_id).map(|p| &mut p.kind) {
                    if chrome.back.contains(x, y) {
                        b.back();
                    } else if chrome.fwd.contains(x, y) {
                        b.forward();
                    } else if chrome.reload.contains(x, y) {
                        b.reload();
                    } else if chrome.dev.contains(x, y) {
                        b.devtools();
                    } else if chrome.close.contains(x, y) {
                        close = true;
                    }
                }
                unsafe {
                    let _ = ReleaseCapture();
                }
                if close {
                    self.close_pane_by_id(pane_id);
                }
                return;
            }

            unsafe {
                let _ = SetFocus(Some(self.hwnd));
            }

            let mods = Mods::current();

            // Ctrl+click: open the URL under the cursor.
            if mods.ctrl && !mods.shift && !mods.alt
                && let Some(url) = self.link_at(pane_id, crect, x, y)
            {
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.open_url(&url);
                return;
            }

            if self.mouse_report(pane_id, crect, x, y, 0, true, false, &mods) {
                self.drag = Drag::None;
                return;
            }

            // Begin text selection.
            let (col, row, side) = self.cell_at(crect, x, y);
            if let Some(tab) = self.tabs.get(self.active)
                && let Some(PaneKind::Term(t)) = tab.pane(tab.active).map(|p| &p.kind) {
                    let mut term = t.term.lock();
                    let display_offset = term.grid().display_offset();
                    let cols = term.grid().columns();
                    let lines = term.grid().screen_lines();
                    let point = Point::new(
                        Line(row.min(lines - 1) as i32 - display_offset as i32),
                        Column(col.min(cols - 1)),
                    );
                    term.selection = Some(Selection::new(SelectionType::Simple, point, side));
                    drop(term);
                    self.drag = Drag::Select;
                    self.invalidate();
                }
        }
    }

    fn on_dblclick(&mut self, x: f32, y: f32) {
        if y < TABBAR_H {
            return;
        }
        let Some((pane_id, prect)) = self.pane_content_at(x, y) else { return };
        let mods = Mods::current();
        if self.mouse_report(pane_id, prect, x, y, 0, true, false, &mods) {
            return;
        }
        let (col, row, side) = self.cell_at(prect, x, y);
        if let Some(tab) = self.tabs.get(self.active)
            && let Some(PaneKind::Term(t)) = tab.pane(pane_id).map(|p| &p.kind) {
                let mut term = t.term.lock();
                let display_offset = term.grid().display_offset();
                let cols = term.grid().columns();
                let lines = term.grid().screen_lines();
                let point = Point::new(
                    Line(row.min(lines - 1) as i32 - display_offset as i32),
                    Column(col.min(cols - 1)),
                );
                term.selection = Some(Selection::new(SelectionType::Semantic, point, side));
                drop(term);
                self.drag = Drag::Select;
                unsafe {
                    SetCapture(self.hwnd);
                }
                self.invalidate();
            }
    }

    fn on_mouse_move(&mut self, x: f32, y: f32, lbutton: bool) {
        match &mut self.drag {
            Drag::None => {
                if lbutton {
                    return;
                }
                // Motion-only mouse reporting (mode 1003).
                let mods = Mods::current();
                if let Some((pane_id, prect)) = self.pane_content_at(x, y) {
                    let wants = self
                        .tabs
                        .get(self.active)
                        .and_then(|t| t.pane(pane_id))
                        .map(|p| match &p.kind {
                            PaneKind::Term(t) => {
                                t.term.lock().mode().contains(TermMode::MOUSE_MOTION)
                            },
                            _ => false,
                        })
                        .unwrap_or(false);
                    if wants {
                        self.mouse_report(pane_id, prect, x, y, 3, true, true, &mods);
                    }
                }
            },
            Drag::Divider { path, index, dir, last } => {
                let path = path.clone();
                let index = *index;
                let dir = *dir;
                let (lx, ly) = *last;
                let delta = match dir {
                    Dir::Row => x - lx,
                    Dir::Col => y - ly,
                };
                if delta.abs() < 0.5 {
                    return;
                }
                // Convert pixel delta into a fraction of the split's extent.
                let area = self.pane_area();
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let extent = split_extent(&tab.root, &path, area, dir);
                    if extent > 1.0 {
                        pane_tree::drag_divider(&mut tab.root, &path, index, delta / extent);
                    }
                }
                if let Drag::Divider { last, .. } = &mut self.drag {
                    *last = (x, y);
                }
                self.relayout();
                self.invalidate();
            },
            Drag::Tab { press, cur, live, .. } => {
                *cur = (x, y);
                if !*live && ((x - press.0).abs() > 6.0 || (y - press.1).abs() > 6.0) {
                    *live = true;
                }
                if *live {
                    self.invalidate();
                }
            },
            Drag::Pane { press, cur, live, .. } => {
                *cur = (x, y);
                if !*live && ((x - press.0).abs() > 6.0 || (y - press.1).abs() > 6.0) {
                    *live = true;
                }
                if *live {
                    self.invalidate();
                }
            },
            Drag::Select => {
                let multi = self.is_multi();
                let Some(tab) = self.tabs.get(self.active) else { return };
                let lay = tab.layout(self.pane_area());
                let Some(prect) = lay.rect_of(tab.active) else { return };
                let prect = content_rect(prect, multi);
                let (col, row, side) = self.cell_at(prect, x, y);
                if let Some(PaneKind::Term(t)) = tab.pane(tab.active).map(|p| &p.kind) {
                    let mut term = t.term.lock();
                    let display_offset = term.grid().display_offset();
                    let cols = term.grid().columns();
                    let lines = term.grid().screen_lines();
                    let point = Point::new(
                        Line(row.min(lines.saturating_sub(1)) as i32 - display_offset as i32),
                        Column(col.min(cols.saturating_sub(1))),
                    );
                    if let Some(sel) = &mut term.selection {
                        sel.update(point, side);
                    }
                    drop(term);
                    self.invalidate();
                }
            },
        }

        // Drag-motion mouse reporting for terminals (button held).
        if lbutton && matches!(self.drag, Drag::None) {
            let mods = Mods::current();
            if let Some((pane_id, prect)) = self.pane_content_at(x, y) {
                self.mouse_report(pane_id, prect, x, y, 0, true, true, &mods);
            }
        }
    }

    fn on_lbutton_up(&mut self, x: f32, y: f32) {
        unsafe {
            let _ = ReleaseCapture();
        }
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        match drag {
            Drag::Tab { idx, live, .. } => {
                if !live {
                    return;
                }
                // Where did it land?
                let mut pt = POINT { x: 0, y: 0 };
                unsafe {
                    let _ = GetCursorPos(&mut pt);
                }
                // Our own tab bar â†’ reorder.
                if (0.0..TABBAR_H).contains(&y) && x >= 0.0 && x <= self.client_dips().0 {
                    let tw = self.tab_width();
                    let to = (((x - 8.0) / tw).floor().max(0.0) as usize).min(self.tabs.len() - 1);
                    if to != idx {
                        let tab = self.tabs.remove(idx);
                        self.tabs.insert(to, tab);
                        self.active = to;
                    }
                    self.relayout();
                    self.invalidate();
                    return;
                }
                // Another term window's tab bar â†’ move the tab there.
                if let Some((target, insert)) = app::tabbar_hit(pt, self.hwnd) {
                    let tab = self.take_tab(idx);
                    app::with_window(target, |w| {
                        w.adopt_tab(tab, Some(insert));
                    });
                    return;
                }
                // Loose drop â†’ tear out into a new window.
                self.detach_tab(idx, Some((pt.x, pt.y)));
                self.invalidate();
            },
            Drag::Pane { id, live, .. } => {
                if !live {
                    return;
                }
                match self.drop_target(x, y, id) {
                    DropTarget::Zone { target, dir, before, .. } => {
                        if let Some(tab) = self.tabs.get_mut(self.active)
                            && pane_tree::move_pane(&mut tab.root, id, target, dir, before)
                        {
                            tab.active = id;
                            tab.zoomed = None;
                        }
                    },
                    DropTarget::Swap { target, .. } => {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            pane_tree::swap(&mut tab.root, id, target);
                        }
                    },
                    DropTarget::NewTab => {
                        self.pane_to_new_tab(id);
                        return; // switch_tab already relaid out
                    },
                    DropTarget::Nothing => {},
                }
                self.relayout();
                self.sync_pane_focus();
                self.update_title();
                self.invalidate();
            },
            Drag::Select => {
                // iTerm2-style copy-on-select.
                self.copy_selection();
                // Mouse-mode release reporting not needed (selection implies no mouse mode).
            },
            Drag::None => {
                let mods = Mods::current();
                if let Some((pane_id, prect)) = self.pane_content_at(x, y) {
                    self.mouse_report(pane_id, prect, x, y, 0, false, false, &mods);
                }
            },
            Drag::Divider { .. } => {},
        }
    }

    fn on_rbutton_down(&mut self, x: f32, y: f32) {
        // Tab bar: right-click anywhere offers the profile list.
        if y < TABBAR_H {
            self.show_profile_menu(x, y);
            return;
        }
        let Some((pane_id, prect)) = self.pane_content_at(x, y) else { return };
        let mods = Mods::current();
        if self.mouse_report(pane_id, prect, x, y, 2, true, false, &mods) {
            return;
        }
        // Copy selection if any, else paste — iTerm2 muscle memory.
        let has_sel = self
            .with_active_term(|t| t.term.lock().selection_to_string().is_some())
            .unwrap_or(false);
        if has_sel {
            self.copy_selection();
            self.with_active_term(|t| {
                t.term.lock().selection = None;
            });
            self.invalidate();
        } else {
            self.paste();
        }
    }

    fn on_wheel(&mut self, x: f32, y: f32, delta: i16) {
        let mods = Mods::current();
        // Ctrl+wheel: font zoom (Windows Terminal / browser convention).
        if mods.ctrl && !mods.shift && !mods.alt {
            let step = if delta > 0 { 1.0 } else { -1.0 };
            self.set_font_size(self.active_font_size() + step);
            return;
        }
        let lines = (delta as f32 / 120.0 * 3.0).round() as i32;
        if lines == 0 {
            return;
        }
        let Some((pane_id, prect)) = self.pane_content_at(x, y) else { return };

        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(pane) = tab.pane(pane_id) else { return };
        let PaneKind::Term(t) = &pane.kind else { return };

        let mode = *t.term.lock().mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !mods.shift {
            let btn = if lines > 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs() {
                self.mouse_report(pane_id, prect, x, y, btn, true, false, &mods);
            }
            return;
        }
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let seq: &[u8] = if mode.contains(TermMode::APP_CURSOR) {
                if lines > 0 { b"\x1bOA" } else { b"\x1bOB" }
            } else if lines > 0 {
                b"\x1b[A"
            } else {
                b"\x1b[B"
            };
            let mut out = Vec::new();
            for _ in 0..lines.unsigned_abs() {
                out.extend_from_slice(seq);
            }
            t.pty.write(&out);
            return;
        }
        t.term.lock().scroll_display(Scroll::Delta(lines));
        self.invalidate();
    }

    fn on_set_cursor(&self) -> bool {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(self.hwnd, &mut pt);
        }
        let s = self.scale();
        let (x, y) = (pt.x as f32 / s, pt.y as f32 / s);
        let cursor = if y < TABBAR_H {
            IDC_ARROW
        } else if let Some(d) = self.divider_at(x, y) {
            match d.dir {
                Dir::Row => IDC_SIZEWE,
                Dir::Col => IDC_SIZENS,
            }
        } else if let Some((pane_id, prect)) = self.pane_at(x, y) {
            if self.is_multi() && y < prect.y + PANE_TITLE_H {
                // Title bar: drag handle (arrow over the close glyph).
                if title_close_rect(prect).contains(x, y) {
                    IDC_ARROW
                } else {
                    IDC_SIZEALL
                }
            } else {
                let is_term = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.pane(pane_id))
                    .map(|p| matches!(p.kind, PaneKind::Term(_)))
                    .unwrap_or(false);
                let mods = Mods::current();
                if is_term
                    && mods.ctrl
                    && !mods.shift
                    && !mods.alt
                    && self
                        .link_at(pane_id, content_rect(prect, self.is_multi()), x, y)
                        .is_some()
                {
                    IDC_HAND
                } else if is_term {
                    IDC_IBEAM
                } else {
                    IDC_ARROW
                }
            }
        } else {
            IDC_ARROW
        };
        unsafe {
            if let Ok(c) = LoadCursorW(None, cursor) {
                SetCursor(Some(c));
            }
        }
        true
    }

    // ----- window proc -----------------------------------------------------

    pub fn message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                if self.custom_frame() {
                    unsafe {
                        // Keep the DWM shadow/rounded corners with our frame.
                        let margins = windows::Win32::UI::Controls::MARGINS {
                            cxLeftWidth: 0,
                            cxRightWidth: 0,
                            cyTopHeight: 1,
                            cyBottomHeight: 0,
                        };
                        let _ = windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea(
                            self.hwnd, &margins,
                        );
                        // Re-run WM_NCCALCSIZE with our handler in place.
                        let _ = SetWindowPos(
                            self.hwnd,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
                        );
                    }
                }
                crate::dragdrop::register(self.hwnd);
                app::ensure_quake_hotkey(self.hwnd);
                unsafe {
                    SetTimer(Some(self.hwnd), app::CONFIG_TIMER_ID, 1000, None);
                }
                match self.pending_init.take() {
                    Some(app::WindowInit::Adopt(tab)) => self.adopt_tab(tab, None),
                    Some(app::WindowInit::Restore(state)) => self.restore_tabs(state),
                    _ => self.new_tab(),
                }
                Some(LRESULT(0))
            },
            WM_DESTROY => {
                crate::dragdrop::revoke(self.hwnd);
                None
            },
            WM_HOTKEY => {
                app::toggle_quake();
                Some(LRESULT(0))
            },
            WM_TIMER if wparam.0 == app::CONFIG_TIMER_ID => {
                app::poll_config_change();
                Some(LRESULT(0))
            },
            WM_TIMER if wparam.0 == TOAST_TIMER_ID => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TOAST_TIMER_ID);
                }
                if self.toast.take().is_some() {
                    self.invalidate();
                }
                Some(LRESULT(0))
            },
            WM_APP_DROP_FILES => {
                let payload = unsafe {
                    Box::from_raw(
                        wparam.0 as *mut (Vec<String>, crate::dragdrop::DropOp, POINT),
                    )
                };
                let (paths, op, pt) = *payload;
                self.do_drop(paths, op, pt);
                Some(LRESULT(0))
            },
            WM_NCCALCSIZE if wparam.0 != 0 && self.custom_frame() => {
                // Claim the caption: keep only the resize borders on the
                // sides/bottom; the client area runs to the top edge.
                let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
                let (fx, fy) = self.frame_metrics();
                let rc = &mut params.rgrc[0];
                rc.left += fx;
                rc.right -= fx;
                rc.bottom -= fy;
                if unsafe { IsZoomed(self.hwnd) }.as_bool() {
                    rc.top += fy; // maximized windows hang off-screen by fy
                }
                Some(LRESULT(0))
            },
            WM_NCHITTEST if self.custom_frame() => {
                let def = unsafe { DefWindowProcW(self.hwnd, msg, wparam, lparam) };
                if def.0 != HTCLIENT as isize {
                    return Some(def);
                }
                let mut pt = POINT {
                    x: loword(lparam.0 as u32) as i16 as i32,
                    y: hiword(lparam.0 as u32) as i16 as i32,
                };
                unsafe {
                    let _ = ScreenToClient(self.hwnd, &mut pt);
                }
                let (_, fy) = self.frame_metrics();
                if pt.y < fy && !unsafe { IsZoomed(self.hwnd) }.as_bool() {
                    return Some(LRESULT(HTTOP as isize));
                }
                let s = self.scale();
                let (x, y) = (pt.x as f32 / s, pt.y as f32 / s);
                if y < TABBAR_H {
                    let (minr, maxr, closer) = self.caption_buttons();
                    if minr.contains(x, y) {
                        return Some(LRESULT(HTMINBUTTON as isize));
                    }
                    if maxr.contains(x, y) {
                        return Some(LRESULT(HTMAXBUTTON as isize));
                    }
                    if closer.contains(x, y) {
                        return Some(LRESULT(HTCLOSE as isize));
                    }
                    if self.tab_at(x, y).is_some() || self.plus_rect().contains(x, y) {
                        return Some(LRESULT(HTCLIENT as isize));
                    }
                    // Empty tab-bar space drags / double-click-maximizes.
                    return Some(LRESULT(HTCAPTION as isize));
                }
                Some(LRESULT(HTCLIENT as isize))
            },
            WM_NCMOUSEMOVE => {
                let code = wparam.0 as u32;
                let hot = matches!(code, HTMINBUTTON | HTMAXBUTTON | HTCLOSE).then_some(code);
                if hot != self.hot_caption {
                    self.hot_caption = hot;
                    self.invalidate();
                }
                unsafe {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
                None
            },
            WM_NCMOUSELEAVE => {
                if self.hot_caption.take().is_some() {
                    self.invalidate();
                }
                None
            },
            WM_NCLBUTTONDOWN
                if matches!(wparam.0 as u32, HTMINBUTTON | HTMAXBUTTON | HTCLOSE) =>
            {
                // Swallow so DefWindowProc doesn't paint legacy buttons;
                // the action happens on button-up, like the real caption.
                Some(LRESULT(0))
            },
            WM_NCLBUTTONUP
                if matches!(wparam.0 as u32, HTMINBUTTON | HTMAXBUTTON | HTCLOSE) =>
            {
                let cmd = match wparam.0 as u32 {
                    HTMINBUTTON => SC_MINIMIZE,
                    HTMAXBUTTON => {
                        if unsafe { IsZoomed(self.hwnd) }.as_bool() {
                            SC_RESTORE
                        } else {
                            SC_MAXIMIZE
                        }
                    },
                    _ => SC_CLOSE,
                };
                unsafe {
                    let _ = PostMessageW(
                        Some(self.hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(cmd as usize),
                        LPARAM(0),
                    );
                }
                Some(LRESULT(0))
            },
            WM_NCRBUTTONUP if wparam.0 as u32 == HTCAPTION => {
                // Standard system menu on right-clicking the "title bar".
                unsafe {
                    let menu = GetSystemMenu(self.hwnd, false);
                    let x = loword(lparam.0 as u32) as i16 as i32;
                    let y = hiword(lparam.0 as u32) as i16 as i32;
                    let cmd = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD,
                        x,
                        y,
                        None,
                        self.hwnd,
                        None,
                    );
                    if cmd.0 != 0 {
                        let _ = PostMessageW(
                            Some(self.hwnd),
                            WM_SYSCOMMAND,
                            WPARAM(cmd.0 as usize),
                            LPARAM(0),
                        );
                    }
                }
                Some(LRESULT(0))
            },
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe {
                    let _ = BeginPaint(self.hwnd, &mut ps);
                }
                self.paint();
                unsafe {
                    let _ = EndPaint(self.hwnd, &ps);
                }
                Some(LRESULT(0))
            },
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_SIZE => {
                let (w, h) = (loword(lparam.0 as u32), hiword(lparam.0 as u32));
                if w > 0 && h > 0 {
                    if let Some(g) = &self.gfx_win {
                        g.resize(w as u32, h as u32);
                    }
                    self.relayout();
                    self.invalidate();
                }
                Some(LRESULT(0))
            },
            WM_DPICHANGED => {
                self.dpi = loword(wparam.0 as u32) as f32;
                if let Some(g) = &self.gfx_win {
                    g.set_dpi(self.dpi);
                }
                // Cell metrics are snapped to the pixel grid per DPI.
                let family = self.fonts.family.clone();
                if let Ok(f) =
                    FontSet::new(&app::gfx(), &family, app::config().font_size, self.dpi)
                {
                    self.fonts = f;
                }
                for tab in &mut self.tabs {
                    if tab.fonts.is_some() {
                        tab.fonts =
                            FontSet::new(&app::gfx(), &family, tab.font_size, self.dpi).ok();
                    }
                }
                unsafe {
                    let rc = *(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        rc.left,
                        rc.top,
                        rc.right - rc.left,
                        rc.bottom - rc.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    let old = self.edit_font;
                    self.edit_font = make_edit_font(self.dpi);
                    for tab in &self.tabs {
                        for pane in tab.panes.values() {
                            if let PaneKind::Browser(b) = &pane.kind {
                                SendMessageW(
                                    b.edit,
                                    WM_SETFONT,
                                    Some(WPARAM(self.edit_font.0 as usize)),
                                    Some(LPARAM(1)),
                                );
                            }
                        }
                    }
                    let _ = DeleteObject(old.into());
                }
                self.relayout();
                self.invalidate();
                Some(LRESULT(0))
            },
            WM_SETFOCUS | WM_KILLFOCUS => {
                self.focused = msg == WM_SETFOCUS;
                self.sync_pane_focus();
                self.invalidate();
                Some(LRESULT(0))
            },
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let handled = self.on_key_down(wparam.0 as u16);
                if handled {
                    Some(LRESULT(0))
                } else if msg == WM_SYSKEYDOWN {
                    None
                } else {
                    Some(LRESULT(0))
                }
            },
            WM_CHAR => {
                self.on_char(wparam.0 as u16);
                Some(LRESULT(0))
            },
            WM_SYSCHAR => {
                // Alt+char â†’ ESC prefix; swallow to avoid menu bell.
                let code = wparam.0 as u16;
                if self.suppress_char {
                    // Handled as a hotkey in WM_SYSKEYDOWN (Alt+digit).
                    self.suppress_char = false;
                    return Some(LRESULT(0));
                }
                if code == b' ' as u16 {
                    return None; // Alt+Space = system menu
                }
                if let Some(ch) = char::from_u32(code as u32) {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    let mut bytes = vec![0x1b];
                    bytes.extend_from_slice(s.as_bytes());
                    self.with_active_term(|t| t.pty.write(&bytes));
                }
                Some(LRESULT(0))
            },
            WM_LBUTTONDOWN => {
                let (x, y) = self.mouse_dips(lparam);
                self.on_lbutton_down(x, y);
                Some(LRESULT(0))
            },
            WM_LBUTTONDBLCLK => {
                let (x, y) = self.mouse_dips(lparam);
                self.on_dblclick(x, y);
                Some(LRESULT(0))
            },
            WM_LBUTTONUP => {
                let (x, y) = self.mouse_dips(lparam);
                self.on_lbutton_up(x, y);
                Some(LRESULT(0))
            },
            WM_RBUTTONDOWN => {
                let (x, y) = self.mouse_dips(lparam);
                self.on_rbutton_down(x, y);
                Some(LRESULT(0))
            },
            WM_MOUSEMOVE => {
                let (x, y) = self.mouse_dips(lparam);
                let lbutton = (wparam.0 & 0x0001) != 0; // MK_LBUTTON
                self.on_mouse_move(x, y, lbutton);
                Some(LRESULT(0))
            },
            WM_MOUSEWHEEL => {
                // Wheel coords are screen-relative.
                let mut pt =
                    POINT { x: loword(lparam.0 as u32) as i16 as i32, y: hiword(lparam.0 as u32) as i16 as i32 };
                unsafe {
                    let _ = ScreenToClient(self.hwnd, &mut pt);
                }
                let s = self.scale();
                let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16;
                self.on_wheel(pt.x as f32 / s, pt.y as f32 / s, delta);
                Some(LRESULT(0))
            },
            WM_MBUTTONUP => {
                let (x, y) = self.mouse_dips(lparam);
                if let Some(i) = self.tab_at(x, y) {
                    self.close_tab(i);
                }
                Some(LRESULT(0))
            },
            WM_SETCURSOR => {
                if loword(lparam.0 as u32) as u32 == HTCLIENT && self.on_set_cursor() {
                    Some(LRESULT(1))
                } else {
                    None
                }
            },
            WM_CTLCOLOREDIT => {
                unsafe {
                    let hdc = HDC(wparam.0 as *mut _);
                    SetTextColor(hdc, COLORREF(0x00F0F0F0));
                    SetBkColor(hdc, COLORREF(0x002A1E1E));
                }
                Some(LRESULT(self.edit_brush.0 as isize))
            },
            WM_COPYDATA => {
                let cds = lparam.0 as *const windows::Win32::System::DataExchange::COPYDATASTRUCT;
                if cds.is_null() {
                    return Some(LRESULT(0));
                }
                let cds = unsafe { &*cds };
                if cds.dwData != crate::ctl::CTL_MAGIC || cds.lpData.is_null() {
                    return None; // not ours; other apps use WM_COPYDATA too
                }
                let bytes = unsafe {
                    std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize)
                };
                let handled = serde_json::from_slice::<crate::ctl::CtlReq>(bytes)
                    .map(|req| self.handle_ctl(&req))
                    .unwrap_or(false);
                Some(LRESULT(handled as isize))
            },
            WM_APP_TERM_DIRTY => {
                self.invalidate();
                Some(LRESULT(0))
            },
            WM_APP_TERM_EVENT => {
                self.update_title();
                self.invalidate();
                Some(LRESULT(0))
            },
            WM_APP_PANE_EXITED => {
                self.close_pane_by_id(wparam.0 as u64);
                Some(LRESULT(0))
            },
            WM_APP_WEBVIEW_READY => {
                let pane_id = wparam.0 as u64;
                if let Some(controller) = browser_pane::take_ready_controller(pane_id) {
                    let mut found = false;
                    for tab in &mut self.tabs {
                        if let Some(pane) = tab.pane_mut(pane_id) {
                            if let PaneKind::Browser(b) = &mut pane.kind {
                                b.install(controller.clone());
                                found = true;
                            }
                            break;
                        }
                    }
                    if found {
                        self.relayout();
                        self.invalidate();
                    }
                }
                Some(LRESULT(0))
            },
            WM_APP_DUMP_FRAME => {
                let path = std::env::temp_dir().join("term-frame.png");
                if let Err(e) = self.dump_frame(&path.to_string_lossy()) {
                    eprintln!("dump_frame failed: {e:?}");
                }
                Some(LRESULT(0))
            },
            #[cfg(debug_assertions)]
            WM_APP_DEBUG_ACTION => {
                match wparam.0 {
                    1 => self.split(Dir::Row, false),
                    2 => self.split(Dir::Col, false),
                    3 => self.new_tab(),
                    4 => self.split(Dir::Row, true),
                    5 => {
                        let n = self.tabs.len().max(1);
                        self.switch_tab((self.active + 1) % n);
                    },
                    6 => self.zoom_toggle(),
                    7 => self.close_active_pane(),
                    8 => self.focus_dir(1, 0),
                    9 => self.focus_dir(-1, 0),
                    10 => self.detach_tab(self.active, Some((300, 300))),
                    11 => self.set_font_size(self.active_font_size() + 2.0),
                    12 => self.new_tab_with_profile(lparam.0 as usize),
                    13 => {
                        if let Some(tab) = self.tabs.get(self.active) {
                            self.pane_to_new_tab(tab.active);
                        }
                    },
                    15 => self.toggle_search(),
                    16 => self.open_palette(),
                    17 => self.open_hints(),
                    18 => self.cycle_theme(),
                    19 => self.toggle_cheatsheet(),
                    14 => {
                        // Simulate a file drop on the active pane's center;
                        // lparam: 0 paste, 1 copy, 2 move.
                        let op = match lparam.0 {
                            1 => crate::dragdrop::DropOp::Copy,
                            2 => crate::dragdrop::DropOp::Move,
                            _ => crate::dragdrop::DropOp::PastePath,
                        };
                        let test_file = std::env::temp_dir().join("bdh-drop-test.txt");
                        let lay = self
                            .tabs
                            .get(self.active)
                            .map(|t| t.layout(self.pane_area()));
                        if let Some(lay) = lay
                            && let Some(r) = lay.rect_of(self.tabs[self.active].active)
                        {
                            let s = self.scale();
                            let (cx, cy) = r.center();
                            let p = POINT { x: (cx * s) as i32, y: (cy * s) as i32 };
                            self.do_drop(
                                vec![test_file.to_string_lossy().into_owned()],
                                op,
                                p,
                            );
                        }
                    },
                    _ => {},
                }
                Some(LRESULT(0))
            },
            WM_APP_URL_ENTER => {
                let pane_id = wparam.0 as u64;
                for tab in &mut self.tabs {
                    if let Some(pane) = tab.pane_mut(pane_id) {
                        if let PaneKind::Browser(b) = &mut pane.kind {
                            let url = b.edit_text();
                            b.navigate(&url);
                            b.focus_webview();
                        }
                        break;
                    }
                }
                Some(LRESULT(0))
            },
            WM_CLOSE => {
                app::save_session();
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            },
            _ => None,
        }
    }

    fn mouse_dips(&self, lparam: LPARAM) -> (f32, f32) {
        let x = loword(lparam.0 as u32) as i16 as i32;
        let y = hiword(lparam.0 as u32) as i16 as i32;
        let s = self.scale();
        (x as f32 / s, y as f32 / s)
    }

    /// Used by cross-window drag hit testing.
    pub fn tabbar_insert_index(&self, client_px: POINT) -> Option<usize> {
        let s = self.scale();
        let (x, y) = (client_px.x as f32 / s, client_px.y as f32 / s);
        if !(0.0..TABBAR_H).contains(&y) || x < 0.0 {
            return None;
        }
        let tw = self.tab_width();
        Some(((((x - 8.0) / tw).floor()).max(0.0) as usize).min(self.tabs.len()))
    }
}

impl Drop for TermWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.edit_font.into());
            let _ = DeleteObject(self.edit_brush.into());
        }
    }
}

fn loword(v: u32) -> u16 {
    (v & 0xffff) as u16
}

fn hiword(v: u32) -> u16 {
    ((v >> 16) & 0xffff) as u16
}

/// Which of a pane's inline images intersect the viewport, and where.
fn visible_images(
    t: &TermPane,
    term: &alacritty_terminal::Term<crate::term_pane::EventProxy>,
    content: RectF,
    fonts: &FontSet,
) -> Vec<(u64, std::sync::Arc<Vec<u8>>, windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F)> {
    use std::sync::atomic::Ordering;
    let images = t.shared.images.lock().unwrap();
    if images.is_empty() || term.mode().contains(TermMode::ALT_SCREEN) {
        return Vec::new();
    }
    let lines_now = t.shared.lines_seen.load(Ordering::Relaxed) as i64;
    let d = term.grid().display_offset() as i64;
    let cursor_row = term.grid().cursor.point.line.0 as i64;
    let screen = term.grid().screen_lines() as i64;
    let mut out = Vec::new();
    for img in images.iter() {
        let delta = lines_now - img.anchor as i64;
        // Viewport row of the image's top edge.
        let row = cursor_row + d - delta;
        if row + img.rows as i64 <= 0 || row >= screen {
            continue;
        }
        let box_w = (content.w - 2.0 * PANE_PAD).max(8.0);
        let box_h = img.rows as f32 * fonts.cell_h;
        let scale = (box_w / img.width as f32).min(box_h / img.height as f32).min(1.0);
        let w = img.width as f32 * scale;
        let h = img.height as f32 * scale;
        let x = content.x + PANE_PAD;
        let y = content.y + PANE_PAD + row as f32 * fonts.cell_h;
        out.push((img.id, img.png.clone(), rf(x, y, x + w, y + h)));
    }
    out
}

/// Escape regex metacharacters so selection prefill searches literally.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Quote a Windows path for the target shell flavor.
fn quote_path(path: &str, flavor: crate::term_pane::ShellFlavor) -> String {
    use crate::term_pane::ShellFlavor;
    match flavor {
        ShellFlavor::Windows => format!("\"{path}\""),
        ShellFlavor::Posix => format!("'{}'", path.replace('\\', "/").replace('\'', "'\\''")),
        ShellFlavor::Wsl => {
            let p = path.replace('\\', "/");
            let translated = match p.as_bytes() {
                [d @ b'A'..=b'Z', b':', ..] | [d @ b'a'..=b'z', b':', ..] => {
                    format!("/mnt/{}{}", (*d as char).to_ascii_lowercase(), &p[2..])
                },
                _ => p,
            };
            format!("'{}'", translated.replace('\'', "'\\''"))
        },
    }
}

/// Explorer-style copy/move into `dest` with undo support and progress UI.
fn shell_file_op(hwnd: HWND, paths: &[String], dest: &str, mv: bool) {
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FO_COPY, FO_MOVE, SHFILEOPSTRUCTW,
    };
    // Double-null-terminated lists.
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut to: Vec<u16> = dest.encode_utf16().collect();
    to.extend([0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        hwnd,
        wFunc: if mv { FO_MOVE } else { FO_COPY },
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        pTo: windows::core::PCWSTR(to.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMMKDIR.0) as u16,
        ..Default::default()
    };
    unsafe {
        let _ = SHFileOperationW(&mut op);
    }
}

fn make_edit_font(dpi: f32) -> HFONT {
    unsafe {
        CreateFontW(
            -(12.0 * dpi / 72.0) as i32,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            windows::core::w!("Consolas"),
        )
    }
}

pub struct BrowserChrome {
    pub toolbar: RectF,
    pub back: RectF,
    pub fwd: RectF,
    pub reload: RectF,
    pub dev: RectF,
    pub close: RectF,
    pub edit: RectF,
}

/// Toolbar geometry for a browser pane (all DIPs):
/// [back][fwd][reload] [ url edit ........ ] [devtools][close]
pub fn browser_chrome(r: RectF) -> BrowserChrome {
    let toolbar = RectF { x: r.x, y: r.y, w: r.w, h: TOOLBAR_H };
    let b = |i: f32| RectF { x: r.x + 4.0 + i * BTN_W, y: r.y + 3.0, w: BTN_W - 4.0, h: TOOLBAR_H - 6.0 };
    let back = b(0.0);
    let fwd = b(1.0);
    let reload = b(2.0);
    let close = RectF { x: r.x + r.w - BTN_W, y: r.y + 3.0, w: BTN_W - 4.0, h: TOOLBAR_H - 6.0 };
    let dev = RectF { x: close.x - BTN_W, y: r.y + 3.0, w: BTN_W - 4.0, h: TOOLBAR_H - 6.0 };
    let edit = RectF {
        x: reload.x + reload.w + 6.0,
        y: r.y + 5.0,
        w: (dev.x - reload.x - reload.w - 12.0).max(40.0),
        h: TOOLBAR_H - 10.0,
    };
    BrowserChrome { toolbar, back, fwd, reload, dev, close, edit }
}

/// Extent (DIPs) of the split node at `path`, along its own axis.
fn split_extent(root: &crate::pane_tree::Node, path: &[usize], area: RectF, dir: Dir) -> f32 {
    // Recompute the rect of the split node by walking the layout tree.
    fn walk(node: &crate::pane_tree::Node, rect: RectF, path: &[usize]) -> Option<RectF> {
        if path.is_empty() {
            return Some(rect);
        }
        let crate::pane_tree::Node::Split { dir, fracs, kids } = node else { return None };
        let n = kids.len();
        let gaps = pane_tree::GAP * (n.saturating_sub(1)) as f32;
        let total = match dir {
            Dir::Row => (rect.w - gaps).max(0.0),
            Dir::Col => (rect.h - gaps).max(0.0),
        };
        let mut pos = match dir {
            Dir::Row => rect.x,
            Dir::Col => rect.y,
        };
        for (i, kid) in kids.iter().enumerate() {
            let extent = total * fracs[i];
            let r = match dir {
                Dir::Row => RectF { x: pos, y: rect.y, w: extent, h: rect.h },
                Dir::Col => RectF { x: rect.x, y: pos, w: rect.w, h: extent },
            };
            if i == path[0] {
                return walk(kid, r, &path[1..]);
            }
            pos += extent + pane_tree::GAP;
        }
        None
    }
    let rect = walk(root, area, path).unwrap_or(area);
    match dir {
        Dir::Row => rect.w.max(1.0),
        Dir::Col => rect.h.max(1.0),
    }
}
