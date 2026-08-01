//! Kitty graphics protocol encoder (PLAN.md M8).
//!
//! Side channel: sequences are written after the cell buffer present, like
//! OSC 52 yank — not mixed into per-cell SGR.
//!
//! Transmit (`a=t`) and place (`a=p`) are split so scroll can re-place without
//! re-base64-encoding RGBA (M8 fast gate).

use super::DecodedImage;

/// One image placement to emit after the frame is presented.
#[derive(Clone, Debug)]
pub struct KittyPlacement {
    /// 1-based terminal column (CUP).
    pub col: u16,
    /// 1-based terminal row (CUP).
    pub row: u16,
    pub cells_w: u16,
    pub cells_h: u16,
    pub image: std::sync::Arc<DecodedImage>,
    /// Kitty image id (non-zero). Shared across placements of the same bitmap.
    pub id: u32,
}

/// Delete every placement but keep uploaded image data (`a=d,d=a`).
pub fn delete_all_placements() -> &'static [u8] {
    b"\x1b_Ga=d,d=a\x1b\\"
}

/// Delete all images and placements (`a=d,d=A`) — free terminal memory.
pub fn delete_all_images() -> &'static [u8] {
    b"\x1b_Ga=d,d=A\x1b\\"
}

/// Transmit RGBA for `id` without placing (`a=t`). Call once per bitmap.
pub fn transmit_sequence(id: u32, img: &DecodedImage) -> Vec<u8> {
    let payload = base64_encode(&img.rgba);
    let mut out = Vec::with_capacity(payload.len() + 64);
    const CHUNK: usize = 4096;
    let total = payload.len();
    if total == 0 {
        return out;
    }
    let mut offset = 0;
    let mut first = true;
    while offset < total {
        let end = (offset + CHUNK).min(total);
        let more = if end < total { 1 } else { 0 };
        if first {
            let _ = std::io::Write::write_fmt(
                &mut out,
                format_args!(
                    "\x1b_Ga=t,f=32,s={},v={},i={},q=2,m={};",
                    img.width, img.height, id, more
                ),
            );
            first = false;
        } else {
            let _ = std::io::Write::write_fmt(&mut out, format_args!("\x1b_Gm={};", more));
        }
        out.extend_from_slice(&payload[offset..end]);
        out.extend_from_slice(b"\x1b\\");
        offset = end;
    }
    out
}

/// Place a previously transmitted image at the cursor (`a=p`). No pixel payload.
pub fn place_sequence(p: &KittyPlacement) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    let _ = std::io::Write::write_fmt(&mut out, format_args!("\x1b[{};{}H", p.row, p.col));
    let _ = std::io::Write::write_fmt(
        &mut out,
        format_args!(
            "\x1b_Ga=p,i={},c={},r={},q=2\x1b\\",
            p.id, p.cells_w, p.cells_h
        ),
    );
    out
}

/// Transmit-and-place in one shot (tests / one-shot demos).
pub fn placement_sequence(p: &KittyPlacement) -> Vec<u8> {
    let mut out = transmit_sequence(p.id, &p.image);
    out.extend_from_slice(&place_sequence(p));
    out
}

/// Minimal base64 encoder (no dependency). Standard alphabet, no line wraps.
fn base64_encode(data: &[u8]) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(T[((n >> 18) & 63) as usize]);
        out.push(T[((n >> 12) & 63) as usize]);
        out.push(T[((n >> 6) & 63) as usize]);
        out.push(T[(n & 63) as usize]);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = (data[i] as u32) << 16;
            out.push(T[((n >> 18) & 63) as usize]);
            out.push(T[((n >> 12) & 63) as usize]);
            out.push(b'=');
            out.push(b'=');
        }
        2 => {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            out.push(T[((n >> 18) & 63) as usize]);
            out.push(T[((n >> 12) & 63) as usize]);
            out.push(T[((n >> 6) & 63) as usize]);
            out.push(b'=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn sequence_has_kitty_framing() {
        let img = Arc::new(DecodedImage::new(1, 1, vec![255, 0, 0, 255]));
        let p = KittyPlacement {
            col: 2,
            row: 3,
            cells_w: 1,
            cells_h: 1,
            image: img,
            id: 7,
        };
        let seq = placement_sequence(&p);
        let s = String::from_utf8_lossy(&seq);
        assert!(s.contains("\x1b_G"), "{s}");
        assert!(s.contains("a=t") || s.contains("a=T"), "{s}");
        assert!(s.contains("a=p"), "{s}");
        assert!(s.contains("f=32"), "{s}");
        assert!(s.contains("i=7"), "{s}");
        assert!(s.contains("\x1b\\"), "{s}");
        assert!(s.len() > 20);
    }

    #[test]
    fn place_has_no_pixel_payload() {
        let img = Arc::new(DecodedImage::new(1, 1, vec![255, 0, 0, 255]));
        let p = KittyPlacement {
            col: 1,
            row: 1,
            cells_w: 2,
            cells_h: 2,
            image: img,
            id: 3,
        };
        let place = place_sequence(&p);
        let s = String::from_utf8_lossy(&place);
        assert!(s.contains("a=p"));
        assert!(
            !s.contains("f=32"),
            "place must not carry a pixel format/payload"
        );
    }

    #[test]
    fn base64_known_vector() {
        assert_eq!(base64_encode(b"cat"), b"Y2F0");
        assert_eq!(base64_encode(b"c"), b"Yw==");
        assert_eq!(base64_encode(b"ca"), b"Y2E=");
    }

    #[test]
    fn delete_all_is_static() {
        assert!(delete_all_placements().starts_with(b"\x1b_G"));
        assert!(delete_all_images().ends_with(b"\x1b\\"));
    }
}
