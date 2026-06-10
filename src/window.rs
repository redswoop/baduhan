//! Top-level terminal window: tab bar, pane area, input routing, painting.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use windows::core::HSTRING;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
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
    drag: Drag,
    edit_font: HFONT,
    edit_brush: HBRUSH,
    suppress_char: bool,
    pending_surrogate: Option<u16>,
    pending_tab: Option<Tab>,
}

impl TermWindow {
    pub fn new(hwnd: HWND, pending_tab: Option<Tab>) -> TermWindow {
        let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
        let dpi = if dpi <= 0.0 { 96.0 } else { dpi };
        let cfg = app::config();
        let fonts = FontSet::new(&app::gfx(), &cfg.font_family, cfg.font_size).expect("fonts");
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
            drag: Drag::None,
            edit_font,
            edit_brush,
            suppress_char: false,
            pending_surrogate: None,
            pending_tab,
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

    // ----- tabs ------------------------------------------------------------

    fn tab_width(&self) -> f32 {
        let (w, _) = self.client_dips();
        let avail = (w - PLUS_W - 16.0).max(50.0);
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

    fn update_title(&self) {
        if let Some(tab) = self.tabs.get(self.active) {
            unsafe {
                let _ = SetWindowTextW(self.hwnd, &HSTRING::from(format!("{} — baduhan", tab.title())));
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
                        drop(term);
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
        let mut title = tab.title();
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
        } else if let Ok(f) = FontSet::new(&app::gfx(), &family, size) {
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

        if self.hotkey(vk, &mods) {
            return true;
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

    fn hotkey(&mut self, vk: u16, m: &Mods) -> bool {
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
                if is_term {
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
                if let Some(tab) = self.pending_tab.take() {
                    self.adopt_tab(tab, None);
                } else {
                    self.new_tab();
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
