//! Device screenshots come in device pixels (a phone's are three times its
//! points). Sessions work in points, so pictures are scaled down to match:
//! a screenshot's pixel grid is the coordinate grid, as on the desktop.

use anyhow::{bail, Result};

use ocu_core::image::{encode_png, Order};

pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub fn decode_png(bytes: &[u8]) -> Result<Rgba> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let data = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        other => bail!("unexpected PNG colour type {other:?}"),
    };
    Ok(Rgba {
        width: w,
        height: h,
        data,
    })
}

/// Shrinks `img` to `width`×`height` by averaging the pixels each target
/// pixel covers (fractional edges weighted), which keeps text legible.
pub fn resize(img: &Rgba, width: u32, height: u32) -> Rgba {
    let (sw, sh) = (img.width as usize, img.height as usize);
    let (dw, dh) = (width.max(1) as usize, height.max(1) as usize);
    if (sw, sh) == (dw, dh) {
        return Rgba {
            width,
            height,
            data: img.data.clone(),
        };
    }
    // Per axis, the source span of each target pixel as (index, weight).
    let spans = |src: usize, dst: usize| -> Vec<Vec<(usize, f32)>> {
        let scale = src as f64 / dst as f64;
        (0..dst)
            .map(|d| {
                let (a, b) = (d as f64 * scale, (d + 1) as f64 * scale);
                let mut v = Vec::new();
                let mut i = a.floor() as usize;
                while (i as f64) < b && i < src {
                    let w = (b.min((i + 1) as f64) - a.max(i as f64)) as f32;
                    if w > 0.0 {
                        v.push((i, w));
                    }
                    i += 1;
                }
                if v.is_empty() {
                    v.push(((a as usize).min(src - 1), 1.0));
                }
                v
            })
            .collect()
    };
    let xs = spans(sw, dw);
    let ys = spans(sh, dh);
    let mut out = vec![0u8; dw * dh * 4];
    let mut row = vec![[0f32; 4]; dw];
    for (dy, yspan) in ys.iter().enumerate() {
        row.iter_mut().for_each(|p| *p = [0.0; 4]);
        let mut wy_total = 0.0;
        for &(sy, wy) in yspan {
            wy_total += wy;
            let line = &img.data[sy * sw * 4..(sy + 1) * sw * 4];
            for (dx, xspan) in xs.iter().enumerate() {
                let acc = &mut row[dx];
                for &(sx, wx) in xspan {
                    let w = wx * wy;
                    let p = &line[sx * 4..sx * 4 + 4];
                    for c in 0..4 {
                        acc[c] += p[c] as f32 * w;
                    }
                }
            }
        }
        for (dx, xspan) in xs.iter().enumerate() {
            let wx_total: f32 = xspan.iter().map(|&(_, w)| w).sum();
            let total = wx_total * wy_total;
            let o = &mut out[(dy * dw + dx) * 4..(dy * dw + dx) * 4 + 4];
            for c in 0..4 {
                o[c] = (row[dx][c] / total).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Rgba {
        width: dw as u32,
        height: dh as u32,
        data: out,
    }
}

/// A device screenshot (PNG, in pixels) as a PNG in points, `scale` being
/// pixels per point.
pub fn to_points(png: &[u8], scale: f64) -> Result<(Vec<u8>, u32, u32)> {
    let img = decode_png(png)?;
    if scale <= 1.01 {
        let bytes = encode_png(
            img.width,
            img.height,
            img.width as usize * 4,
            &img.data,
            Order::Rgba,
        )?;
        return Ok((bytes, img.width, img.height));
    }
    let w = (img.width as f64 / scale).round() as u32;
    let h = (img.height as f64 / scale).round() as u32;
    let small = resize(&img, w, h);
    let bytes = encode_png(
        small.width,
        small.height,
        small.width as usize * 4,
        &small.data,
        Order::Rgba,
    )?;
    Ok((bytes, small.width, small.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_blocks() {
        // 4x2 → 2x1: each target pixel averages a 2x2 block.
        let mut data = Vec::new();
        for v in [0u8, 100, 200, 255, 0, 100, 200, 255] {
            data.extend_from_slice(&[v, v, v, 255]);
        }
        let img = Rgba {
            width: 4,
            height: 2,
            data,
        };
        let out = resize(&img, 2, 1);
        assert_eq!(out.data[0], 50);
        assert_eq!(out.data[4], 228);
    }

    #[test]
    fn round_trip() {
        let data = vec![10u8; 6 * 6 * 4];
        let png = encode_png(6, 6, 24, &data, Order::Rgba).unwrap();
        let (small, w, h) = to_points(&png, 3.0).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(decode_png(&small).unwrap().data[0], 10);
    }
}
