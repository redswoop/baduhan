//! Inline images: iTerm2's OSC 1337 `File=inline=1` protocol.
//!
//! The PTY reader runs chunks through `ImgScan`, which splits out image
//! sequences (they routinely span read chunks) from plain bytes. For each
//! image the pane reserves grid rows by feeding synthetic newlines through
//! the parser, and remembers the image against the pane's line clock so it
//! scrolls with the text. `baduhan view <file>` is the bundled imgcat.

use std::sync::Arc;

/// All inline-image traffic arrives as OSC 1337 sub-commands: `File=` (one
/// giant sequence) or imgcat's default multipart form (`MultipartFile=` →
/// `FilePart=`× → `FileEnd`).
pub const PREFIX: &[u8] = b"\x1b]1337;";

pub struct InlineImage {
    pub id: u64,
    /// Pane line-clock value at the anchor (top-left of the image).
    pub anchor: u64,
    pub png: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// Grid rows reserved below the anchor.
    pub rows: u16,
}

/// One parsed-but-unplaced image (dims known, anchor/rows assigned later).
pub struct ParsedImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub enum Seg {
    Plain(Vec<u8>),
    Image(ParsedImage),
}

/// Stateful splitter: plain output vs. inline-image sequences.
#[derive(Default)]
pub struct ImgScan {
    /// Payload collected so far for a sequence whose terminator we haven't
    /// seen yet (starts after the `OSC 1337;` prefix).
    pending: Option<Vec<u8>>,
    /// Accumulated base64 from a MultipartFile transfer in progress.
    multipart: Option<Vec<u8>>,
}

impl ImgScan {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Seg> {
        let mut out = Vec::new();
        let mut rest = bytes;

        if let Some(mut buf) = self.pending.take() {
            match find_terminator(rest) {
                Some((end, skip)) => {
                    buf.extend_from_slice(&rest[..end]);
                    if let Some(img) = self.classify(&buf) {
                        out.push(Seg::Image(img));
                    }
                    rest = &rest[end + skip..];
                },
                None => {
                    buf.extend_from_slice(rest);
                    // Runaway guard: drop absurd payloads (> 64 MB).
                    if buf.len() > 64 * 1024 * 1024 {
                        eprintln!("inline image too large; dropped");
                    } else {
                        self.pending = Some(buf);
                    }
                    return out;
                },
            }
        }

        loop {
            match find_subslice(rest, PREFIX) {
                None => {
                    if !rest.is_empty() {
                        out.push(Seg::Plain(rest.to_vec()));
                    }
                    break;
                },
                Some(start) => {
                    if start > 0 {
                        out.push(Seg::Plain(rest[..start].to_vec()));
                    }
                    let payload = &rest[start + PREFIX.len()..];
                    match find_terminator(payload) {
                        Some((end, skip)) => {
                            if let Some(img) = self.classify(&payload[..end]) {
                                out.push(Seg::Image(img));
                            }
                            rest = &payload[end + skip..];
                        },
                        None => {
                            self.pending = Some(payload.to_vec());
                            break;
                        },
                    }
                },
            }
        }
        out
    }

    /// Dispatch one complete OSC 1337 payload. Non-image sub-commands
    /// (RemoteHost, SetUserVar, …) are consumed silently — alacritty would
    /// ignore them anyway.
    fn classify(&mut self, payload: &[u8]) -> Option<ParsedImage> {
        if let Some(rest) = payload.strip_prefix(b"File=") {
            return parse_file_payload(rest);
        }
        if payload.starts_with(b"MultipartFile=") {
            self.multipart = Some(Vec::new());
            return None;
        }
        if let Some(part) = payload.strip_prefix(b"FilePart=") {
            if let Some(buf) = &mut self.multipart {
                buf.extend_from_slice(part);
                if buf.len() > 96 * 1024 * 1024 {
                    self.multipart = None; // runaway
                }
            }
            return None;
        }
        if payload.starts_with(b"FileEnd") {
            let b64 = self.multipart.take()?;
            return image_from_bytes(b64_decode(&b64)?);
        }
        None
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// BEL or ST (ESC \). Returns (index, terminator length).
fn find_terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    for (i, b) in bytes.iter().enumerate() {
        if *b == 0x07 {
            return Some((i, 1));
        }
        if *b == 0x1b {
            return if bytes.get(i + 1) == Some(&b'\\') { Some((i, 2)) } else { Some((i, 1)) };
        }
    }
    None
}

/// `inline=1;size=N:BASE64` → decoded image with dimensions. Accepts
/// anything iTerm2's imgcat sends: PNG sized via IHDR, every other format
/// sized via WIC (the renderer decodes through WIC anyway).
fn parse_file_payload(payload: &[u8]) -> Option<ParsedImage> {
    let colon = payload.iter().position(|b| *b == b':')?;
    let args = std::str::from_utf8(&payload[..colon]).ok()?;
    if !args.split(';').any(|a| a.trim() == "inline=1") {
        return None; // download-only transfer; not displayed
    }
    image_from_bytes(b64_decode(&payload[colon + 1..])?)
}

fn image_from_bytes(data: Vec<u8>) -> Option<ParsedImage> {
    let (width, height) = png_dimensions(&data).or_else(|| wic_dimensions(&data))?;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    Some(ParsedImage { png: data, width, height })
}

/// Image dimensions via WIC for non-PNG formats (JPEG/GIF/BMP/WebP/…).
/// Runs on the PTY reader thread; COM is initialized MTA on first use.
pub fn wic_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::SHCreateMemStream;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = SHCreateMemStream(Some(data))?;
        let decoder = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        Some((w, h))
    }
}

/// Width/height from a PNG IHDR.
pub fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if !data.starts_with(SIG) || data.len() < 24 || &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

// ----- base64 (std has none; not worth a dependency) --------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn b64_decode(data: &[u8]) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in data {
        if b == b'=' || b == b'\r' || b == b'\n' {
            continue;
        }
        acc = (acc << 6) | val(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &[u8] = b"\x1b]1337;File=";

    /// Tiny valid PNG header (1×1 IHDR; body truncated — fine for dims).
    fn tiny_png() -> Vec<u8> {
        let mut p = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        p.extend_from_slice(&13u32.to_be_bytes());
        p.extend_from_slice(b"IHDR");
        p.extend_from_slice(&7u32.to_be_bytes()); // width 7
        p.extend_from_slice(&3u32.to_be_bytes()); // height 3
        p.extend_from_slice(&[8, 6, 0, 0, 0]);
        p
    }

    #[test]
    fn b64_roundtrip() {
        for case in [&b""[..], b"f", b"fo", b"foo", b"foob", b"hello world!"] {
            assert_eq!(b64_decode(b64_encode(case).as_bytes()).unwrap(), case);
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_decode(b"aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn png_dims() {
        assert_eq!(png_dimensions(&tiny_png()), Some((7, 3)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    fn seq(png: &[u8]) -> Vec<u8> {
        let mut s = MARKER.to_vec();
        s.extend_from_slice(format!("inline=1;size={}:", png.len()).as_bytes());
        s.extend_from_slice(b64_encode(png).as_bytes());
        s.push(0x07);
        s
    }

    #[test]
    fn scan_extracts_image_between_plain_bytes() {
        let mut scan = ImgScan::default();
        let mut input = b"before ".to_vec();
        input.extend_from_slice(&seq(&tiny_png()));
        input.extend_from_slice(b" after");
        let segs = scan.feed(&input);
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], Seg::Plain(p) if p == b"before "));
        assert!(matches!(&segs[1], Seg::Image(i) if i.width == 7 && i.height == 3));
        assert!(matches!(&segs[2], Seg::Plain(p) if p == b" after"));
    }

    #[test]
    fn scan_handles_chunk_splits() {
        let mut scan = ImgScan::default();
        let full = seq(&tiny_png());
        // Split mid-base64.
        let (a, b) = full.split_at(MARKER.len() + 20);
        let segs = scan.feed(a);
        assert!(segs.is_empty());
        let mut tail = b.to_vec();
        tail.extend_from_slice(b"done");
        let segs = scan.feed(&tail);
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[0], Seg::Image(i) if i.width == 7));
        assert!(matches!(&segs[1], Seg::Plain(p) if p == b"done"));
    }

    #[test]
    fn multipart_imgcat_protocol() {
        // Exactly what modern imgcat emits: MultipartFile header, b64 folded
        // into 200-char FilePart chunks, FileEnd.
        let png = tiny_png();
        let b64 = b64_encode(&png);
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b]1337;MultipartFile=inline=1;size=999\x07");
        for chunk in b64.as_bytes().chunks(200) {
            input.extend_from_slice(b"\x1b]1337;FilePart=");
            input.extend_from_slice(chunk);
            input.push(0x07);
        }
        input.extend_from_slice(b"\x1b]1337;FileEnd\x07");
        input.extend_from_slice(b"\nprompt$ ");

        let mut scan = ImgScan::default();
        // Feed in awkward 37-byte chunks to exercise the pending path.
        let mut segs = Vec::new();
        for c in input.chunks(37) {
            segs.extend(scan.feed(c));
        }
        let images: Vec<&ParsedImage> = segs
            .iter()
            .filter_map(|s| match s {
                Seg::Image(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (7, 3));
        // Trailing plain text survives.
        let plain: Vec<u8> = segs
            .iter()
            .filter_map(|s| match s {
                Seg::Plain(p) => Some(p.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(plain, b"\nprompt$ ");
    }

    #[test]
    fn shell_integration_1337_subcommands_are_consumed() {
        let mut scan = ImgScan::default();
        let segs = scan.feed(b"\x1b]1337;RemoteHost=user@host\x07ok");
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Seg::Plain(p) if p == b"ok"));
    }

    #[test]
    fn non_inline_transfers_are_ignored() {
        let mut s = MARKER.to_vec();
        s.extend_from_slice(b"name=eA==;size=3:");
        s.extend_from_slice(b64_encode(&tiny_png()).as_bytes());
        s.push(0x07);
        let segs = ImgScan::default().feed(&s);
        assert!(segs.is_empty() || !matches!(segs[0], Seg::Image(_)));
    }
}
