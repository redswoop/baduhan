//! Inline images: iTerm2's OSC 1337 `File=inline=1` protocol.
//!
//! The PTY reader runs chunks through `ImgScan`, which splits out image
//! sequences (they routinely span read chunks) from plain bytes. For each
//! image the pane reserves grid rows by feeding synthetic newlines through
//! the parser, and remembers the image against the pane's line clock so it
//! scrolls with the text. `baduhan view <file>` is the bundled imgcat.

use std::sync::Arc;

pub const MARKER: &[u8] = b"\x1b]1337;File=";

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
    /// seen yet (starts after the marker).
    pending: Option<Vec<u8>>,
}

impl ImgScan {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Seg> {
        let mut out = Vec::new();
        let mut rest = bytes;

        if let Some(mut buf) = self.pending.take() {
            match find_terminator(rest) {
                Some((end, skip)) => {
                    buf.extend_from_slice(&rest[..end]);
                    if let Some(img) = parse_payload(&buf) {
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
            match find_subslice(rest, MARKER) {
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
                    let payload = &rest[start + MARKER.len()..];
                    match find_terminator(payload) {
                        Some((end, skip)) => {
                            if let Some(img) = parse_payload(&payload[..end]) {
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

/// `inline=1;size=N:BASE64` → decoded image with dimensions.
fn parse_payload(payload: &[u8]) -> Option<ParsedImage> {
    let colon = payload.iter().position(|b| *b == b':')?;
    let args = std::str::from_utf8(&payload[..colon]).ok()?;
    if !args.split(';').any(|a| a.trim() == "inline=1") {
        return None; // download-only transfer; not displayed
    }
    let data = b64_decode(&payload[colon + 1..])?;
    let (width, height) = png_dimensions(&data)?;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    Some(ParsedImage { png: data, width, height })
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
    fn non_inline_transfers_are_ignored() {
        let mut s = MARKER.to_vec();
        s.extend_from_slice(b"name=eA==;size=3:");
        s.extend_from_slice(b64_encode(&tiny_png()).as_bytes());
        s.push(0x07);
        let segs = ImgScan::default().feed(&s);
        assert!(segs.is_empty() || !matches!(segs[0], Seg::Image(_)));
    }
}
