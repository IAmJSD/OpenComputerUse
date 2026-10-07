//! Pixel buffers to PNG, shared by the backends.

use anyhow::Result;

/// The channel order of a captured buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Rgba,
    Bgra,
}

/// Encodes a 32-bit buffer, dropping alpha (window captures carry none
/// worth keeping) and padding from `stride`.
pub fn encode_png(width: u32, height: u32, stride: usize, data: &[u8], order: Order) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    anyhow::ensure!(stride >= w * 4 && data.len() >= stride * h.saturating_sub(1) + w * 4, "short pixel buffer");
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        let line = &data[row * stride..row * stride + w * 4];
        for px in line.chunks_exact(4) {
            match order {
                Order::Rgba => rgb.extend_from_slice(&[px[0], px[1], px[2]]),
                Order::Bgra => rgb.extend_from_slice(&[px[2], px[1], px[0]]),
            }
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()?.write_image_data(&rgb)?;
    }
    Ok(out)
}
