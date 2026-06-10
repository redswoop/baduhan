//! Direct2D / DirectWrite rendering: device resources, font metrics, and the
//! terminal grid painter. Everything is in DIPs; D2D scales to the monitor DPI.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use anyhow::Result;
use windows::core::{w, HSTRING};
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

/// Text formats + cell metrics for one font family and size (all DIPs).
#[derive(Clone)]
pub struct FontSet {
    pub regular: IDWriteTextFormat,
    pub bold: IDWriteTextFormat,
    pub italic: IDWriteTextFormat,
    pub bold_italic: IDWriteTextFormat,
    pub ui: IDWriteTextFormat,
    pub icons: IDWriteTextFormat,
    /// The family actually in use (after fallback validation).
    pub family: String,
    /// Cell box, snapped to whole device pixels so columns/rows sit on the
    /// pixel grid (fractional positions = ClearType smear).
    pub cell_w: f32,
    pub cell_h: f32,
    /// The font's natural advance; the difference vs. cell_w is injected as
    /// per-glyph spacing so long runs stay on the grid.
    pub advance: f32,
}

pub const FALLBACK_FAMILIES: [&str; 2] = ["Cascadia Mono", "Consolas"];

/// Does the system font collection contain this family? (DirectWrite silently
/// substitutes unknown families, which would mangle cell metrics — validate.)
pub fn family_exists(gfx: &Gfx, family: &str) -> bool {
    unsafe {
        let mut coll = None;
        if gfx.dwrite.GetSystemFontCollection(&mut coll, false).is_err() {
            return false;
        }
        let Some(coll) = coll else { return false };
        let mut index = 0u32;
        let mut exists = windows::core::BOOL(0);
        coll.FindFamilyName(&HSTRING::from(family), &mut index, &mut exists).is_ok()
            && exists.as_bool()
    }
}

impl FontSet {
    pub fn new(gfx: &Gfx, family: &str, size: f32, dpi: f32) -> Result<FontSet> {
        let family = if family_exists(gfx, family) {
            family.to_string()
        } else {
            eprintln!("font family '{family}' not installed; falling back");
            FALLBACK_FAMILIES
                .iter()
                .find(|f| family_exists(gfx, f))
                .map(|f| f.to_string())
                .unwrap_or_else(|| "Consolas".to_string())
        };
        let family_h = HSTRING::from(family.as_str());
        unsafe {
            let make = |weight: DWRITE_FONT_WEIGHT, style: DWRITE_FONT_STYLE| -> Result<IDWriteTextFormat> {
                let f = gfx.dwrite.CreateTextFormat(
                    &family_h,
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

            let scale = (dpi / 96.0).max(0.5);
            let snap = |v: f32| ((v * scale).round().max(1.0)) / scale;
            Ok(FontSet {
                regular,
                bold,
                italic,
                bold_italic,
                ui,
                icons,
                family,
                cell_w: snap(metrics.widthIncludingTrailingWhitespace),
                cell_h: snap(metrics.height),
                advance: metrics.widthIncludingTrailingWhitespace,
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
    /// Decoded inline-image bitmaps, keyed by image id. None = decode failed
    /// (don't retry every frame). Lives with the render target.
    img_cache: std::cell::RefCell<std::collections::HashMap<u64, Option<ID2D1Bitmap>>>,
    dpi: std::cell::Cell<f32>,
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
            Ok(WindowGfx {
                rt,
                hwnd_rt: Some(hwnd_rt),
                brush,
                img_cache: Default::default(),
                dpi: std::cell::Cell::new(dpi),
            })
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
            Ok((
                WindowGfx {
                    rt,
                    hwnd_rt: None,
                    brush,
                    img_cache: Default::default(),
                    dpi: std::cell::Cell::new(dpi),
                },
                bmp,
            ))
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
        self.dpi.set(dpi);
        unsafe { self.rt.SetDpi(dpi, dpi) };
    }

    /// Snap a DIP coordinate to the device pixel grid.
    pub fn snap(&self, v: f32) -> f32 {
        let s = (self.dpi.get() / 96.0).max(0.5);
        (v * s).round() / s
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

impl WindowGfx {
    /// Draw an inline image (PNG bytes), decoding + caching on first use.
    pub fn draw_image(&self, id: u64, png: &[u8], dest: D2D_RECT_F) {
        let mut cache = self.img_cache.borrow_mut();
        if cache.len() > 64 {
            cache.clear();
        }
        let entry = cache.entry(id).or_insert_with(|| decode_to_bitmap(&self.rt, png));
        if let Some(bmp) = entry.as_ref() {
            unsafe {
                self.rt.DrawBitmap(
                    bmp,
                    Some(&dest),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            }
        }
    }
}

fn decode_to_bitmap(rt: &ID2D1RenderTarget, png: &[u8]) -> Option<ID2D1Bitmap> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::SHCreateMemStream;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = SHCreateMemStream(Some(png))?;
        let decoder = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let converted = WICConvertBitmapSource(&GUID_WICPixelFormat32bppPBGRA, &frame).ok()?;
        rt.CreateBitmapFromWicBitmap(&converted, None).ok()
    }
}

pub fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

/// Draw a box-drawing / block-element char as geometry filling the cell
/// exactly — fonts never tile U+2500–259F cleanly across a snapped grid.
fn draw_decor(win: &WindowGfx, x: f32, y: f32, cw: f32, ch: f32, c: char, fg: Rgb) {
    let color = palette::d2d(fg);
    let fill = |l: f32, t: f32, r: f32, b: f32| win.fill(rect(x + l, y + t, x + r, y + b), color);
    let alpha = |a: f32| win.fill(rect(x, y, x + cw, y + ch), palette::d2d_a(fg, a));

    // Block elements first (pure rectangles).
    match c {
        '\u{2588}' => return fill(0.0, 0.0, cw, ch),                       // █
        '\u{2580}' => return fill(0.0, 0.0, cw, ch / 2.0),                 // ▀
        '\u{2584}' => return fill(0.0, ch / 2.0, cw, ch),                  // ▄
        '\u{258C}' => return fill(0.0, 0.0, cw / 2.0, ch),                 // ▌
        '\u{2590}' => return fill(cw / 2.0, 0.0, cw, ch),                  // ▐
        '\u{2594}' => return fill(0.0, 0.0, cw, ch / 8.0),                 // ▔
        '\u{2595}' => return fill(cw * 7.0 / 8.0, 0.0, cw, ch),            // ▕
        '\u{2591}' => return alpha(0.25),                                  // ░
        '\u{2592}' => return alpha(0.5),                                   // ▒
        '\u{2593}' => return alpha(0.75),                                  // ▓
        // ▁▂▃▄▅▆▇ lower eighths (2581..2587)
        '\u{2581}'..='\u{2587}' => {
            let k = (c as u32 - 0x2580) as f32; // 1..7 eighths
            return fill(0.0, ch * (1.0 - k / 8.0), cw, ch);
        },
        // ▉▊▋▌▍▎▏ left eighths (2589..258F = 7/8 .. 1/8)
        '\u{2589}'..='\u{258F}' => {
            let k = 8.0 - (c as u32 - 0x2588) as f32; // 7..1 eighths
            return fill(0.0, 0.0, cw * k / 8.0, ch);
        },
        // Quadrants 2596..259F.
        '\u{2596}'..='\u{259F}' => {
            // bits: (upper-left, upper-right, lower-left, lower-right)
            let quads = match c {
                '\u{2596}' => (false, false, true, false),
                '\u{2597}' => (false, false, false, true),
                '\u{2598}' => (true, false, false, false),
                '\u{2599}' => (true, false, true, true),
                '\u{259A}' => (true, false, false, true),
                '\u{259B}' => (true, true, true, false),
                '\u{259C}' => (true, true, false, true),
                '\u{259D}' => (false, true, false, false),
                '\u{259E}' => (false, true, true, false),
                _ => (false, true, true, true), // 259F
            };
            let (hw, hh) = (cw / 2.0, ch / 2.0);
            if quads.0 {
                fill(0.0, 0.0, hw, hh);
            }
            if quads.1 {
                fill(hw, 0.0, cw, hh);
            }
            if quads.2 {
                fill(0.0, hh, hw, ch);
            }
            if quads.3 {
                fill(hw, hh, cw, ch);
            }
            return;
        },
        _ => {},
    }

    // Line-drawing: (up, down, left, right, heavy, double).
    let (u, d, l, r, heavy, double) = match c {
        '\u{2500}' => (false, false, true, true, false, false),  // ─
        '\u{2501}' => (false, false, true, true, true, false),   // ━
        '\u{2502}' => (true, true, false, false, false, false),  // │
        '\u{2503}' => (true, true, false, false, true, false),   // ┃
        '\u{250C}' | '\u{256D}' => (false, true, false, true, false, false), // ┌ ╭
        '\u{250F}' => (false, true, false, true, true, false),
        '\u{2510}' | '\u{256E}' => (false, true, true, false, false, false), // ┐ ╮
        '\u{2513}' => (false, true, true, false, true, false),
        '\u{2514}' | '\u{2570}' => (true, false, false, true, false, false), // └ ╰
        '\u{2517}' => (true, false, false, true, true, false),
        '\u{2518}' | '\u{256F}' => (true, false, true, false, false, false), // ┘ ╯
        '\u{251B}' => (true, false, true, false, true, false),
        '\u{251C}' => (true, true, false, true, false, false),   // ├
        '\u{2523}' => (true, true, false, true, true, false),
        '\u{2524}' => (true, true, true, false, false, false),   // ┤
        '\u{252B}' => (true, true, true, false, true, false),
        '\u{252C}' => (false, true, true, true, false, false),   // ┬
        '\u{2533}' => (false, true, true, true, true, false),
        '\u{2534}' => (true, false, true, true, false, false),   // ┴
        '\u{253B}' => (true, false, true, true, true, false),
        '\u{253C}' => (true, true, true, true, false, false),    // ┼
        '\u{254B}' => (true, true, true, true, true, false),
        '\u{2550}' => (false, false, true, true, false, true),   // ═
        '\u{2551}' => (true, true, false, false, false, true),   // ║
        '\u{2554}' => (false, true, false, true, false, true),   // ╔
        '\u{2557}' => (false, true, true, false, false, true),   // ╗
        '\u{255A}' => (true, false, false, true, false, true),   // ╚
        '\u{255D}' => (true, false, true, false, false, true),   // ╝
        '\u{2560}' => (true, true, false, true, false, true),    // ╠
        '\u{2563}' => (true, true, true, false, false, true),    // ╣
        '\u{2566}' => (false, true, true, true, false, true),    // ╦
        '\u{2569}' => (true, false, true, true, false, true),    // ╩
        '\u{256C}' => (true, true, true, true, false, true),     // ╬
        '\u{2574}' => (false, false, true, false, false, false), // ╴
        '\u{2575}' => (true, false, false, false, false, false), // ╵
        '\u{2576}' => (false, false, false, true, false, false), // ╶
        '\u{2577}' => (false, true, false, false, false, false), // ╷
        // Dashed lines: render as their solid counterparts.
        '\u{254C}' | '\u{2504}' | '\u{2508}' => (false, false, true, true, false, false),
        '\u{254E}' | '\u{2506}' | '\u{250A}' => (true, true, false, false, false, false),
        _ => {
            // Unknown decor char: fall back to a centered dot so it's visible.
            let t = (ch / 10.0).max(1.5);
            fill(cw / 2.0 - t / 2.0, ch / 2.0 - t / 2.0, cw / 2.0 + t / 2.0, ch / 2.0 + t / 2.0);
            return;
        },
    };

    let t = if heavy { (ch / 8.0).max(2.0) } else { (ch / 16.0).max(1.0) };
    let (cx, cy) = (cw / 2.0, ch / 2.0);
    let stroke = |x0: f32, y0: f32, x1: f32, y1: f32| fill(x0, y0, x1, y1);
    if double {
        let off = t * 1.5;
        // Two parallel thin lines per arm.
        let tt = t.max(1.0) / 1.5;
        let arm_h = |from: f32, to: f32, yo: f32| stroke(from, cy + yo - tt / 2.0, to, cy + yo + tt / 2.0);
        let arm_v = |from: f32, to: f32, xo: f32| stroke(cx + xo - tt / 2.0, from, cx + xo + tt / 2.0, to);
        if l {
            arm_h(0.0, cx + off, -off);
            arm_h(0.0, cx + off, off);
        }
        if r {
            arm_h(cx - off, cw, -off);
            arm_h(cx - off, cw, off);
        }
        if u {
            arm_v(0.0, cy + off, -off);
            arm_v(0.0, cy + off, off);
        }
        if d {
            arm_v(cy - off, ch, -off);
            arm_v(cy - off, ch, off);
        }
        return;
    }
    if l {
        stroke(0.0, cy - t / 2.0, cx + t / 2.0, cy + t / 2.0);
    }
    if r {
        stroke(cx - t / 2.0, cy - t / 2.0, cw, cy + t / 2.0);
    }
    if u {
        stroke(cx - t / 2.0, 0.0, cx + t / 2.0, cy + t / 2.0);
    }
    if d {
        stroke(cx - t / 2.0, cy - t / 2.0, cx + t / 2.0, ch);
    }
}

struct Run {
    row: usize,
    col: usize,
    width: usize,
    text: String,
    fg: Rgb,
    underline: Rgb,
    flags: Flags,
}

/// Box-drawing / block-element cell drawn as geometry instead of a glyph
/// (fonts never tile these cleanly across the snapped cell grid).
struct Decor {
    row: usize,
    col: usize,
    c: char,
    fg: Rgb,
}

fn is_decor(c: char) -> bool {
    ('\u{2500}'..='\u{259F}').contains(&c)
}

const ALL_UNDERLINES: Flags = Flags::UNDERLINE
    .union(Flags::DOUBLE_UNDERLINE)
    .union(Flags::UNDERCURL)
    .union(Flags::DOTTED_UNDERLINE)
    .union(Flags::DASHED_UNDERLINE);

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
        // Cell backgrounds tile at fractional coordinates; antialiased fills
        // leave a dark seam at every cell boundary (visible as a hatched
        // texture when an app paints a full-row background, e.g. vim's
        // cursorline). Aliased geometry pixel-snaps the fills instead.
        win.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
    }
    let sch = palette::scheme();
    win.fill(area, palette::d2d(sch.bg));

    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let selection = content.selection;
    let colors = content.colors;
    let (cw, ch) = (fonts.cell_w, fonts.cell_h);
    let ox = win.snap(area.left + 2.0);
    let oy = win.snap(area.top + 2.0);

    let mut runs: Vec<Run> = Vec::new();
    let mut decors: Vec<Decor> = Vec::new();
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
                fg = sch.bg;
                bg = sch.fg;
            }
        }
        let underline = cell
            .underline_color()
            .map(|c| palette::resolve(c, colors))
            .unwrap_or(fg);

        let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };

        // Background cell rect (only when it differs from the cleared default).
        if bg != sch.bg {
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
        let style_flags = cell.flags
            & (Flags::BOLD | Flags::ITALIC | ALL_UNDERLINES | Flags::STRIKEOUT);

        // Flush the run when discontiguous or styles change.
        let flush = match &cur {
            Some(r) => {
                r.row != row
                    || r.col + r.width != col
                    || r.fg != fg
                    || r.underline != underline
                    || r.flags != style_flags
            },
            None => false,
        };
        if flush {
            runs.push(cur.take().unwrap());
        }

        // Box/block characters get crisp geometry, not glyphs.
        if drawable && is_decor(cell.c) && cell.zerowidth().is_none() {
            if let Some(r) = cur.take() {
                runs.push(r);
            }
            decors.push(Decor { row, col, c: cell.c, fg });
            continue;
        }

        if drawable {
            let r = cur.get_or_insert(Run {
                row,
                col,
                width: 0,
                text: String::new(),
                fg,
                underline,
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
            if r.row == row && r.col + r.width == col && !r.flags.intersects(ALL_UNDERLINES | Flags::STRIKEOUT) {
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

    // Per-glyph spacing keeps every column on the snapped pixel grid even
    // though the font's natural advance is fractional.
    let spacing = cw - fonts.advance;
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
                if spacing.abs() > 0.001
                    && let Ok(l1) = windows::core::Interface::cast::<IDWriteTextLayout1>(&layout)
                {
                    let _ = l1.SetCharacterSpacing(
                        0.0,
                        spacing,
                        0.0,
                        DWRITE_TEXT_RANGE { startPosition: 0, length: utf16.len() as u32 },
                    );
                }
                win.brush.SetColor(&palette::d2d(r.fg));
                win.rt.DrawTextLayout(
                    Vector2 { X: x, Y: y },
                    &layout,
                    &win.brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                );
            }
            let end_x = x + r.width as f32 * cw;
            let ucolor = palette::d2d(r.underline);
            let uy = y + ch - 1.5;
            if r.flags.contains(Flags::UNDERCURL) {
                // Sine wave: half a period per cell, ~1.2 DIP amplitude.
                let amp = 1.2f32;
                let step = 1.0f32;
                let freq = std::f32::consts::PI / (cw / 2.0);
                let mut px = x;
                let mut py = uy - amp * ((px - x) * freq).sin();
                while px < end_x {
                    let nx = (px + step).min(end_x);
                    let ny = uy - amp * ((nx - x) * freq).sin();
                    win.line(px, py, nx, ny, ucolor, 1.0);
                    px = nx;
                    py = ny;
                }
            } else if r.flags.contains(Flags::DOTTED_UNDERLINE) {
                let mut px = x;
                while px < end_x {
                    win.fill(rect(px, uy - 0.5, (px + 1.0).min(end_x), uy + 0.5), ucolor);
                    px += 2.5;
                }
            } else if r.flags.contains(Flags::DASHED_UNDERLINE) {
                let mut px = x;
                while px < end_x {
                    win.line(px, uy, (px + 3.0).min(end_x), uy, ucolor, 1.0);
                    px += 5.0;
                }
            } else if r.flags.contains(Flags::DOUBLE_UNDERLINE) {
                win.line(x, uy, end_x, uy, ucolor, 1.0);
                win.line(x, uy - 2.0, end_x, uy - 2.0, ucolor, 1.0);
            } else if r.flags.contains(Flags::UNDERLINE) {
                win.line(x, uy, end_x, uy, ucolor, 1.0);
            }
            if r.flags.contains(Flags::STRIKEOUT) {
                win.line(x, y + ch * 0.55, end_x, y + ch * 0.55, palette::d2d(r.fg), 1.0);
            }
        }
    }

    for d in &decors {
        draw_decor(win, ox + d.col as f32 * cw, oy + d.row as f32 * ch, cw, ch, d.c, d.fg);
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
            // OSC 12 can override the cursor color at runtime.
            let cursor_rgb = palette::resolve(
                alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Cursor,
                ),
                colors,
            );
            let ccolor = palette::d2d(cursor_rgb);
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
                                win.brush.SetColor(&palette::d2d(sch.bg));
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
        win.rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        win.rt.PopAxisAlignedClip();
    }
}
