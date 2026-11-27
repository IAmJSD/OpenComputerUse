//! The VNC (RFB 003.008, RFC 6143) server: lets any VNC client (macOS
//! Screen Sharing, TigerVNC, RealVNC) watch and drive a background session
//! through its [`Viewer`], using the view token as the VNC password.

use anyhow::{bail, ensure, Context as _, Result};

use ocu_core::image::Order;
use ocu_core::Frame;

use super::protocol::Region;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Rfb33,
    Rfb37,
    Rfb38,
}

/// RFB `PIXEL_FORMAT` (16 bytes, RFC 6143 §7.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl Default for PixelFormat {
    fn default() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_color: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }
}

impl PixelFormat {
    pub fn to_bytes(self) -> [u8; 16] {
        let [rm0, rm1] = self.red_max.to_be_bytes();
        let [gm0, gm1] = self.green_max.to_be_bytes();
        let [bm0, bm1] = self.blue_max.to_be_bytes();
        [
            self.bits_per_pixel,
            self.depth,
            u8::from(self.big_endian),
            u8::from(self.true_color),
            rm0,
            rm1,
            gm0,
            gm1,
            bm0,
            bm1,
            self.red_shift,
            self.green_shift,
            self.blue_shift,
            0,
            0,
            0,
        ]
    }

    pub fn from_bytes(b: &[u8; 16]) -> Result<Self> {
        let bpp = b[0];
        ensure!(
            matches!(bpp, 8 | 16 | 32),
            "unsupported VNC bits_per_pixel {bpp}"
        );
        ensure!(
            b[3] != 0,
            "VNC colour-map (non-true-colour) pixel format is not supported"
        );
        let red_max = u16::from_be_bytes([b[4], b[5]]);
        let green_max = u16::from_be_bytes([b[6], b[7]]);
        let blue_max = u16::from_be_bytes([b[8], b[9]]);
        ensure!(
            red_max > 0 && green_max > 0 && blue_max > 0,
            "invalid VNC colour max 0"
        );
        Ok(Self {
            bits_per_pixel: bpp,
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: true,
            red_max,
            green_max,
            blue_max,
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        })
    }
}

/// Converts `region` of `frame` into the client's negotiated [`PixelFormat`].
pub fn encode_raw(frame: &Frame, region: Region, pf: PixelFormat) -> Vec<u8> {
    let (w, h) = (region.width as usize, region.height as usize);
    let bpp_bytes = (pf.bits_per_pixel / 8) as usize;
    let mut out = Vec::with_capacity(w * h * bpp_bytes);

    let fast_bgra = pf.bits_per_pixel == 32
        && !pf.big_endian
        && pf.true_color
        && pf.red_max == 255
        && pf.green_max == 255
        && pf.blue_max == 255
        && pf.red_shift == 16
        && pf.green_shift == 8
        && pf.blue_shift == 0;

    for y in region.y as usize..region.y as usize + h {
        let row_start = y * frame.stride + region.x as usize * 4;
        let row = &frame.data[row_start..row_start + w * 4];
        if fast_bgra && frame.order == Order::Bgra {
            out.extend_from_slice(row);
            continue;
        }
        for px in row.as_chunks::<4>().0 {
            let (r, g, b) = match frame.order {
                Order::Bgra => (px[2], px[1], px[0]),
                Order::Rgba => (px[0], px[1], px[2]),
            };
            if fast_bgra {
                out.extend_from_slice(&[b, g, r, 0]);
                continue;
            }
            let rm = pf.red_max.max(1) as u32;
            let gm = pf.green_max.max(1) as u32;
            let bm = pf.blue_max.max(1) as u32;
            let (rs, gs, bs) = (pf.red_shift, pf.green_shift, pf.blue_shift);
            let rv = ((r as u32 * rm + 127) / 255) & rm;
            let gv = ((g as u32 * gm + 127) / 255) & gm;
            let bv = ((b as u32 * bm + 127) / 255) & bm;
            let val = (rv << (rs & 31)) | (gv << (gs & 31)) | (bv << (bs & 31));
            match (pf.bits_per_pixel, pf.big_endian) {
                (8, _) => out.push(val as u8),
                (16, true) => out.extend_from_slice(&(val as u16).to_be_bytes()),
                (16, false) => out.extend_from_slice(&(val as u16).to_le_bytes()),
                (_, true) => out.extend_from_slice(&val.to_be_bytes()),
                (_, false) => out.extend_from_slice(&val.to_le_bytes()),
            }
        }
    }
    out
}

/// Crops or pads `frame` into a canvas of size `(width, height)`, for VNC
/// clients that did not advertise the `DesktopSize` pseudo-encoding.
pub fn fit_frame(frame: &Frame, width: u16, height: u16) -> Frame {
    let (tw, th) = (width as u32, height as u32);
    if frame.width == tw && frame.height == th {
        return frame.clone();
    }
    let stride = tw as usize * 4;
    let mut data = vec![0u8; stride * th as usize];
    let (copy_w, copy_h) = (
        frame.width.min(tw) as usize * 4,
        frame.height.min(th) as usize,
    );
    for y in 0..copy_h {
        data[y * stride..y * stride + copy_w]
            .copy_from_slice(&frame.data[y * frame.stride..y * frame.stride + copy_w]);
    }
    Frame {
        window_id: frame.window_id,
        width: tw,
        height: th,
        stride,
        data,
        order: frame.order,
    }
}

/// Numeric sequence `n` from a session id `s{n}-{hash}`, so `"s10-..."` sorts
/// after `"s2-..."`.
pub fn session_seq(id: &str) -> u64 {
    id.strip_prefix('s')
        .and_then(|s| s.split_once('-'))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or(0)
}

/// Parses the client's 12-byte `ProtocolVersion` banner (`RFB xxx.yyy\n`).
/// Accepts 3.8, Apple Screen Sharing's 3.889 (which speaks the 3.8
/// handshake), 3.7 and 3.3.
pub(crate) fn parse_version(banner: &[u8; 12]) -> Result<Protocol> {
    let s = std::str::from_utf8(banner).context("invalid RFB version banner")?;
    let Some(ver) = s.strip_prefix("RFB ").and_then(|s| s.strip_suffix('\n')) else {
        bail!("not an RFB version banner: {:?}", s.trim());
    };
    let Some((maj, min)) = ver.split_once('.') else {
        bail!("malformed RFB version: {ver}");
    };
    match (
        maj.trim().parse::<u32>().context("RFB major version")?,
        min.trim().parse::<u32>().context("RFB minor version")?,
    ) {
        (3, 8 | 889) => Ok(Protocol::Rfb38),
        (3, 7) => Ok(Protocol::Rfb37),
        (3, 3) => Ok(Protocol::Rfb33),
        (maj, min) => bail!("unsupported RFB version {maj}.{min}"),
    }
}

/// Encrypts a 16-byte VNC challenge with `password` per RFC 6143 §7.2.2.
pub fn vnc_encrypt_challenge(challenge: &[u8; 16], password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (i, &b) in password.iter().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let subkeys = des_subkeys(u64::from_be_bytes(key));
    let b0 = des_block(
        u64::from_be_bytes(challenge[0..8].try_into().unwrap()),
        &subkeys,
    );
    let b1 = des_block(
        u64::from_be_bytes(challenge[8..16].try_into().unwrap()),
        &subkeys,
    );
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&b0.to_be_bytes());
    out[8..16].copy_from_slice(&b1.to_be_bytes());
    out
}

#[rustfmt::skip]
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3,
    60, 52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37,
    29, 21, 13, 5, 28, 20, 12, 4,
];
#[rustfmt::skip]
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2,
    41, 52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
#[rustfmt::skip]
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3,
    61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
#[rustfmt::skip]
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
#[rustfmt::skip]
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17,
    16, 17, 18, 19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
#[rustfmt::skip]
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10,
    2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4, 25,
];
#[rustfmt::skip]
const SBOX: [[[u8; 16]; 4]; 8] = [
    [[14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7], [0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8], [4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0], [15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13]],
    [[15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10], [3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5], [0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15], [13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9]],
    [[10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8], [13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1], [13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7], [1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12]],
    [[7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15], [13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9], [10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4], [3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14]],
    [[2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9], [14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6], [4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14], [11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3]],
    [[12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11], [10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8], [9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6], [4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13]],
    [[4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1], [13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6], [1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2], [6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12]],
    [[13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7], [1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2], [7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8], [2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11]],
];

fn permute(v: u64, in_bits: u8, table: &[u8]) -> u64 {
    let mut out = 0u64;
    for &pos in table {
        out = (out << 1) | ((v >> (in_bits - pos)) & 1);
    }
    out
}

fn des_subkeys(key: u64) -> [u64; 16] {
    let cd = permute(key, 64, &PC1);
    let mut c = ((cd >> 28) & 0x0fff_ffff) as u32;
    let mut d = (cd & 0x0fff_ffff) as u32;
    let rot28 = |x: u32, n: u8| ((x << n) | (x >> (28 - n))) & 0x0fff_ffff;
    let mut subkeys = [0u64; 16];
    for (i, &s) in SHIFTS.iter().enumerate() {
        c = rot28(c, s);
        d = rot28(d, s);
        let merged = ((c as u64) << 28) | (d as u64);
        subkeys[i] = permute(merged, 56, &PC2);
    }
    subkeys
}

fn des_block(block: u64, subkeys: &[u64; 16]) -> u64 {
    let ip = permute(block, 64, &IP);
    let mut l = (ip >> 32) as u32;
    let mut r = ip as u32;
    for &k in subkeys {
        let expanded = permute(r as u64, 32, &E) ^ k;
        let mut s_out = 0u32;
        for (i, sbox) in SBOX.iter().enumerate() {
            let six = ((expanded >> (42 - i * 6)) & 0x3f) as usize;
            let row = ((six & 0x20) >> 4) | (six & 0x01);
            let col = (six >> 1) & 0x0f;
            s_out = (s_out << 4) | (sbox[row][col] as u32);
        }
        let f = permute(s_out as u64, 32, &P) as u32;
        (l, r) = (r, l ^ f);
    }
    let pre = ((r as u64) << 32) | (l as u64);
    permute(pre, 64, &FP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_fips_and_vnc_bit_reversal() {
        let subkeys = des_subkeys(0x1334_5779_9bbc_dff1);
        assert_eq!(
            des_block(0x0123_4567_89ab_cdef, &subkeys),
            0x85e8_1354_0f0a_b405
        );
        let raw_key = 0x1334_5779_9bbc_dff1u64.to_be_bytes();
        let vnc_pw: Vec<u8> = raw_key.iter().map(|b| b.reverse_bits()).collect();
        let challenge: [u8; 16] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef,
        ];
        let enc = vnc_encrypt_challenge(&challenge, &vnc_pw);
        assert_eq!(&enc[0..8], &0x85e8_1354_0f0a_b405u64.to_be_bytes());
        assert_eq!(&enc[8..16], &0x85e8_1354_0f0a_b405u64.to_be_bytes());
    }

    #[test]
    fn pixel_formats_pack_bgra_and_rgba() {
        let frame = Frame {
            window_id: 1,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![255, 0, 0, 255, 0, 255, 128, 255],
            order: Order::Rgba,
        };
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        let raw32 = encode_raw(&frame, region, PixelFormat::default());
        assert_eq!(raw32, vec![0, 0, 255, 0, 128, 255, 0, 0]);

        let rgb565 = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: true,
            true_color: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        let raw16 = encode_raw(&frame, region, rgb565);
        assert_eq!(&raw16[0..2], &(31u16 << 11).to_be_bytes());

        let mut bad_tc = rgb565.to_bytes();
        bad_tc[3] = 0;
        assert!(PixelFormat::from_bytes(&bad_tc).is_err());
        let mut bad_max = rgb565.to_bytes();
        bad_max[4] = 0;
        bad_max[5] = 0;
        assert!(PixelFormat::from_bytes(&bad_max).is_err());
    }

    #[test]
    fn session_sequence_sorts_numerically() {
        assert!(session_seq("s10-abcdef") > session_seq("s2-abcdef"));
        assert_eq!(parse_version(b"RFB 003.889\n").unwrap(), Protocol::Rfb38);
    }
}
