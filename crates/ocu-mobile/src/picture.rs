//! Device screenshots come in device pixels (a phone's are three times its
//! points). Sessions work in points, so pictures are scaled down to match:
//! a screenshot's pixel grid is the coordinate grid, as on the desktop.

use anyhow::Result;

use ocu_core::image::{encode_png, Order};

use ocu_core::image::{decode_png, resize};

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
    fn round_trip() {
        let data = vec![10u8; 6 * 6 * 4];
        let png = encode_png(6, 6, 24, &data, Order::Rgba).unwrap();
        let (small, w, h) = to_points(&png, 3.0).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(decode_png(&small).unwrap().data[0], 10);
    }
}
