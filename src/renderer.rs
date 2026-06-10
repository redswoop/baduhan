//! Direct2D / DirectWrite rendering: device resources, font metrics, and the
//! terminal grid painter. Everything is in DIPs; D2D scales to the monitor DPI.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use anyhow::Result;
use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::Vector2;

use crate::palette;
use crate::term_pane::EventProxy;

pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dwrite: IDWriteFactory,
}

impl Gfx {
    pub fn new() -> Result<Gfx> {
        unsafe {
            let d2d: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Gfx { d2d, dwrite })
        }
    }
}

/// Text formats + cell metrics for one font size (all DIPs).
pub struct FontSet {
    pub regular: IDWriteTextFormat,
    pub bold: IDWriteTextFormat,
    pub italic: IDWriteTextFormat,
    pub bold_italic: IDWriteTextFormat,
    pub ui: IDWriteTextFormat,
    pub icons: IDWriteTextFormat,
    pub cell_w: f32,
    pub cell_h: f32,
}

pub const FONT_FAMILY: windows::core::PCWSTR = w!("Cascadia Mono");

impl FontSet {
    pub fn new(gfx: &Gfx, size: f32) -> Result<FontSet> {
        unsafe {
            let make = |weight: DWRITE_FONT_WEIGHT, style: DWRITE_FONT_STYLE| -> Result<IDWriteTextFormat> {
                let f = gfx.dwrite.CreateTextFormat(
                    FONT_FAMILY,
                    None,
                    weight,
                    style,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                Ok(f)
            };
            let regular = make(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL)?;
            let bold = make(DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_NORMAL)?;
            let italic = make(DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_ITALIC)?;
            let bold_italic = make(DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STYLE_ITALIC)?;

            let ui = gfx.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                12.5,
                w!("en-us"),
            )?;
            ui.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            ui.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let icons = gfx.dwrite.CreateTextFormat(
                w!("Segoe MDL2 Assets"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                13.0,
                w!("en-us"),
            )?;
            icons.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            icons.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            icons.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;

            // Measure the advance of one glyph for the cell box.
            let m: Vec<u16> = "M".encode_utf16().collect();
            let layout = gfx.dwrite.CreateTextLayout(&m, &regular, 4096.0, 4096.0)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;

            Ok(FontSet {
                regular,
                bold,
                italic,
                bold_italic,
                ui,
                icons,
                cell_w: metrics.widthIncludingTrailingWhitespace,
                cell_h: metrics.height,
            })
        }
    }

    pub fn pick(&self, flags: Flags) -> &IDWriteTextFormat {
        let bold = flags.contains(Flags::BOLD);
        let italic = flags.contains(Flags::ITALIC);
        match (bold, italic) {
            (true, true) => &self.bold_italic,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (false, false) => &self.regular,
        }
    }
}

/// Per-window render target + scratch brush. `rt` is the generic interface
/// so the same painting code can draw to the screen or to a WIC bitmap
/// (debug frame dumps).
pub struct WindowGfx {
    pub rt: ID2D1RenderTarget,
    hwnd_rt: Option<ID2D1HwndRenderTarget>,
    pub brush: ID2D1SolidColorBrush,
}

impl WindowGfx {
    pub fn new(gfx: &Gfx, hwnd: HWND, width_px: u32, height_px: u32, dpi: f32) -> Result<WindowGfx> {
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES::default();
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: width_px, height: height_px },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let hwnd_rt = gfx.d2d.CreateHwndRenderTarget(&props, &hwnd_props)?;
            let rt: ID2D1RenderTarget = windows::core::Interface::cast(&hwnd_rt)?;
            rt.SetDpi(dpi, dpi);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            let brush = rt.CreateSolidColorBrush(&palette::d2d(palette::DEFAULT_FG), None)?;
            Ok(WindowGfx { rt, hwnd_rt: Some(hwnd_rt), brush })
        }
    }

    /// Offscreen target for debug frame dumps; returns the bitmap to encode.
    pub fn new_wic(
        gfx: &Gfx,
        wic: &windows::Win32::Graphics::Imaging::IWICImagingFactory,
        width_px: u32,
        height_px: u32,
        dpi: f32,
    ) -> Result<(WindowGfx, windows::Win32::Graphics::Imaging::IWICBitmap)> {
        use windows::Win32::Graphics::Imaging::*;
        unsafe {
            let bmp = wic.CreateBitmap(
                width_px,
                height_px,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnDemand,
            )?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                ..Default::default()
            };
            let rt = gfx.d2d.CreateWicBitmapRenderTarget(&bmp, &props)?;
            rt.SetDpi(dpi, dpi);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            let brush = rt.CreateSolidColorBrush(&palette::d2d(palette::DEFAULT_FG), None)?;
            Ok((WindowGfx { rt, hwnd_rt: None, brush }, bmp))
        }
    }

    pub fn resize(&self, width_px: u32, height_px: u32) {
        unsafe {
            if let Some(rt) = &self.hwnd_rt {
                let _ = rt.Resize(&D2D_SIZE_U { width: width_px, height: height_px });
            }
        }
    }

    pub fn set_dpi(&self, dpi: f32) {
        unsafe { self.rt.SetDpi(dpi, dpi) };
    }

    pub fn fill(&self, rect: D2D_RECT_F, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.FillRectangle(&rect, &self.brush);
        }
    }

    pub fn frame(&self, rect: D2D_RECT_F, color: D2D1_COLOR_F, width: f32) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.DrawRectangle(&rect, &self.brush, width, None);
        }
    }

    pub fn rounded(&self, rect: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            let rr = D2D1_ROUNDED_RECT { rect, radiusX: radius, radiusY: radius };
            self.rt.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, color: D2D1_COLOR_F, width: f32) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.DrawLine(
                Vector2 { X: x0, Y: y0 },
                Vector2 { X: x1, Y: y1 },
                &self.brush,
                width,
                None,
            );
        }
    }

    /// Draw a single-line string clipped to `rect`, vertically centered when
    /// the format has centered paragraph alignment.
    pub fn text(
        &self,
        gfx: &Gfx,
        s: &str,
        format: &IDWriteTextFormat,
        rect: D2D_RECT_F,
        color: D2D1_COLOR_F,
    ) {
        if s.is_empty() {
            return;
        }
        unsafe {
            let utf16: Vec<u16> = s.encode_utf16().collect();
            if let Ok(layout) = gfx.dwrite.CreateTextLayout(
                &utf16,
                format,
                (rect.right - rect.left).max(0.0),
                (rect.bottom - rect.top).max(0.0),
            ) {
                let _ = layout.SetTrimming(
                    &DWRITE_TRIMMING {
                        granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                        delimiter: 0,
                        delimiterCount: 0,
                    },
                    None,
                );
                self.brush.SetColor(&color);
                self.rt.DrawTextLayout(
                    Vector2 { X: rect.left, Y: rect.top },
                    &layout,
                    &self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                );
            }
        }
    }
}

pub fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

struct Run {
    row: usize,
    col: usize,
    width: usize,
    text: String,
    fg: Rgb,
    flags: Flags,
}

/// Paint one terminal pane's grid into `area` (DIPs).
pub fn draw_term(
    win: &WindowGfx,
    gfx: &Gfx,
    fonts: &FontSet,
    term: &Term<EventProxy>,
    area: D2D_RECT_F,
    focused: bool,
) {
    unsafe {
        win.rt.PushAxisAlignedClip(&area, D2D1_ANTIALIAS_MODE_ALIASED);
    }
    win.fill(area, palette::d2d(palette::DEFAULT_BG));

    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let selection = content.selection;
    let colors = content.colors;
    let (cw, ch) = (fonts.cell_w, fonts.cell_h);
    let ox = area.left + 2.0;
    let oy = area.top + 2.0;

    let mut runs: Vec<Run> = Vec::new();
    let mut cur: Option<Run> = None;

    for indexed in content.display_iter {
        let cell = &indexed.cell;
        let point = indexed.point;
        let Some(vp) = point_to_viewport(display_offset, point) else { continue };
        let (row, col) = (vp.line, vp.column.0);

        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }

        let mut fg = palette::resolve(cell.fg, colors);
        let mut bg = palette::resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::DIM) {
            fg = palette::dim(fg);
        }
        let selected = selection.is_some_and(|s| s.contains(point));
        if selected {
            std::mem::swap(&mut fg, &mut bg);
            if fg == bg {
                fg = palette::DEFAULT_BG;
                bg = palette::DEFAULT_FG;
            }
        }

        let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };

        // Background cell rect (only when it differs from the cleared default).
        if bg != palette::DEFAULT_BG {
            win.fill(
                rect(
                    ox + col as f32 * cw,
                    oy + row as f32 * ch,
                    ox + (col + width) as f32 * cw,
                    oy + (row + 1) as f32 * ch,
                ),
                palette::d2d(bg),
            );
        }

        let drawable = cell.c != ' ' || cell.zerowidth().is_some();
        let style_flags = cell.flags & (Flags::BOLD | Flags::ITALIC | Flags::UNDERLINE
            | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL | Flags::DOTTED_UNDERLINE
            | Flags::DASHED_UNDERLINE | Flags::STRIKEOUT);

        // Flush the run when discontiguous or styles change.
        let flush = match &cur {
            Some(r) => {
                r.row != row || r.col + r.width != col || r.fg != fg || r.flags != style_flags
            },
            None => false,
        };
        if flush {
            runs.push(cur.take().unwrap());
        }

        if drawable {
            let r = cur.get_or_insert(Run {
                row,
                col,
                width: 0,
                text: String::new(),
                fg,
                flags: style_flags,
            });
            // Pad for any skipped blank cells within the same logical run start.
            r.text.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                r.text.extend(zw);
            }
            r.width += width;
        } else if let Some(r) = &mut cur {
            // Blank cell inside a run: keep monospace alignment with a space.
            if r.row == row && r.col + r.width == col && !r.flags.intersects(Flags::UNDERLINE | Flags::STRIKEOUT) {
                r.text.push(' ');
                r.width += 1;
            } else {
                runs.push(cur.take().unwrap());
            }
        }
    }
    if let Some(r) = cur.take() {
        runs.push(r);
    }

    unsafe {
        for r in &runs {
            let x = ox + r.col as f32 * cw;
            let y = oy + r.row as f32 * ch;
            let utf16: Vec<u16> = r.text.encode_utf16().collect();
            if let Ok(layout) = gfx.dwrite.CreateTextLayout(
                &utf16,
                fonts.pick(r.flags),
                (r.width as f32 + 2.0) * cw,
                ch * 2.0,
            ) {
                win.brush.SetColor(&palette::d2d(r.fg));
                win.rt.DrawTextLayout(
                    Vector2 { X: x, Y: y },
                    &layout,
                    &win.brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                );
            }
            let end_x = x + r.width as f32 * cw;
            if r.flags.intersects(
                Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL
                    | Flags::DOTTED_UNDERLINE | Flags::DASHED_UNDERLINE,
            ) {
                win.line(x, y + ch - 1.5, end_x, y + ch - 1.5, palette::d2d(r.fg), 1.0);
                if r.flags.contains(Flags::DOUBLE_UNDERLINE) {
                    win.line(x, y + ch - 3.5, end_x, y + ch - 3.5, palette::d2d(r.fg), 1.0);
                }
            }
            if r.flags.contains(Flags::STRIKEOUT) {
                win.line(x, y + ch * 0.55, end_x, y + ch * 0.55, palette::d2d(r.fg), 1.0);
            }
        }
    }

    // Cursor.
    let cursor = content.cursor;
    if cursor.shape != CursorShape::Hidden
        && let Some(vp) = point_to_viewport(display_offset, cursor.point) {
            let (row, col) = (vp.line, vp.column.0);
            let cell = &term.grid()[cursor.point];
            let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2usize } else { 1 };
            let x = ox + col as f32 * cw;
            let y = oy + row as f32 * ch;
            let cell_rect = rect(x, y, x + width as f32 * cw, y + ch);
            let ccolor = palette::d2d(palette::DEFAULT_FG);
            let shape = if focused { cursor.shape } else { CursorShape::HollowBlock };
            match shape {
                CursorShape::Block => {
                    win.fill(cell_rect, ccolor);
                    if cell.c != ' ' {
                        let s = cell.c.to_string();
                        let utf16: Vec<u16> = s.encode_utf16().collect();
                        unsafe {
                            if let Ok(layout) = gfx.dwrite.CreateTextLayout(
                                &utf16,
                                fonts.pick(cell.flags),
                                cw * 3.0,
                                ch * 2.0,
                            ) {
                                win.brush.SetColor(&palette::d2d(palette::DEFAULT_BG));
                                win.rt.DrawTextLayout(
                                    Vector2 { X: x, Y: y },
                                    &layout,
                                    &win.brush,
                                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                                );
                            }
                        }
                    }
                },
                CursorShape::Beam => win.fill(rect(x, y, x + 2.0, y + ch), ccolor),
                CursorShape::Underline => win.fill(rect(x, y + ch - 2.5, x + cw, y + ch), ccolor),
                CursorShape::HollowBlock => win.frame(cell_rect, ccolor, 1.0),
                CursorShape::Hidden => {},
            }
        }

    // Scrollback position indicator on the right edge.
    if display_offset > 0 && !term.mode().contains(TermMode::ALT_SCREEN) {
        let total = term.grid().history_size() + term.grid().screen_lines();
        let frac_off = display_offset as f32 / total.max(1) as f32;
        let frac_view = term.grid().screen_lines() as f32 / total.max(1) as f32;
        let h = area.bottom - area.top;
        let thumb_h = (frac_view * h).max(24.0);
        let top = area.top + (1.0 - frac_off - frac_view).max(0.0) * (h - thumb_h);
        win.rounded(
            rect(area.right - 7.0, top, area.right - 3.0, top + thumb_h),
            2.0,
            palette::d2d_a(palette::rgb(0xAA, 0xAA, 0xBB), 0.45),
        );
    }

    unsafe {
        win.rt.PopAxisAlignedClip();
    }
}
