//! Screenshots marked up for a decision model that cannot point: a grid of
//! numbered cells it can choose between, and zoomed crops to choose again
//! inside the winner. Plain RGB pixels and a tiny built-in digit font, so
//! there is no image or font dependency.

use anyhow::{bail, Context as _, Result};

#[derive(Clone)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// RGB, row-major, no padding.
    pub rgb: Vec<u8>,
}

/// A rectangle in pixels of the image it was found in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Cell {
    pub fn center(&self) -> (f64, f64) {
        (self.x as f64 + self.width as f64 / 2.0, self.y as f64 + self.height as f64 / 2.0)
    }
}

impl Image {
    pub fn decode(png_bytes: &[u8]) -> Result<Self> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().context("reading the screenshot")?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf)?;
        let (w, h) = (info.width as usize, info.height as usize);
        let channels = match info.color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            other => bail!("unsupported screenshot colour type {other:?}"),
        };
        let mut rgb = Vec::with_capacity(w * h * 3);
        for row in 0..h {
            let line = &buf[row * info.line_size..row * info.line_size + w * channels];
            for px in line.chunks_exact(channels) {
                match channels {
                    1 | 2 => rgb.extend_from_slice(&[px[0], px[0], px[0]]),
                    _ => rgb.extend_from_slice(&px[..3]),
                }
            }
        }
        Ok(Self { width: w, height: h, rgb })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, self.width as u32, self.height as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            enc.write_header()?.write_image_data(&self.rgb)?;
        }
        Ok(out)
    }

    fn put(&mut self, x: usize, y: usize, c: [u8; 3]) {
        if x < self.width && y < self.height {
            let i = (y * self.width + x) * 3;
            self.rgb[i..i + 3].copy_from_slice(&c);
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: [u8; 3]) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                self.put(xx, yy, c);
            }
        }
    }

    /// The region `cell`, scaled up (nearest neighbour) by `scale`.
    pub fn crop(&self, cell: Cell, scale: usize) -> Image {
        let scale = scale.max(1);
        let (w, h) = (cell.width * scale, cell.height * scale);
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            let sy = (cell.y + y / scale).min(self.height - 1);
            for x in 0..w {
                let sx = (cell.x + x / scale).min(self.width - 1);
                let i = (sy * self.width + sx) * 3;
                rgb.extend_from_slice(&self.rgb[i..i + 3]);
            }
        }
        Image { width: w, height: h, rgb }
    }

    /// The image shrunk (box-averaged) so its longer side is at most `max`.
    pub fn shrink(&self, max: usize) -> Image {
        let longest = self.width.max(self.height);
        if longest <= max {
            return self.clone();
        }
        let (w, h) = ((self.width * max / longest).max(1), (self.height * max / longest).max(1));
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            let (y0, y1) = (y * self.height / h, ((y + 1) * self.height / h).max(y * self.height / h + 1));
            for x in 0..w {
                let (x0, x1) = (x * self.width / w, ((x + 1) * self.width / w).max(x * self.width / w + 1));
                let mut sum = [0u32; 3];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let i = (sy * self.width + sx) * 3;
                        for c in 0..3 {
                            sum[c] += self.rgb[i + c] as u32;
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                rgb.extend(sum.iter().map(|v| (v / n) as u8));
            }
        }
        Image { width: w, height: h, rgb }
    }

    /// A rectangle outline, `thick` pixels wide.
    pub fn outline(&mut self, c: Cell, thick: usize, color: [u8; 3]) {
        for t in 0..thick {
            for x in c.x..c.x + c.width {
                self.put(x, c.y + t, color);
                self.put(x, (c.y + c.height).saturating_sub(1 + t), color);
            }
            for y in c.y..c.y + c.height {
                self.put(c.x + t, y, color);
                self.put((c.x + c.width).saturating_sub(1 + t), y, color);
            }
        }
    }
}

/// Splits `width`×`height` into `cols`×`rows` cells, numbered from 1 left to
/// right, top to bottom.
pub fn cells(width: usize, height: usize, cols: usize, rows: usize) -> Vec<Cell> {
    let mut out = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let x0 = width * c / cols;
            let x1 = width * (c + 1) / cols;
            let y0 = height * r / rows;
            let y1 = height * (r + 1) / rows;
            out.push(Cell { x: x0, y: y0, width: x1 - x0, height: y1 - y0 });
        }
    }
    out
}

/// 3×5 digits, one row per byte, bits from the left.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

fn label(img: &mut Image, x: usize, y: usize, n: usize, px: usize) {
    let text = n.to_string();
    let (w, h) = (text.len() * 4 * px + px, 7 * px);
    // A dark tag with white digits reads on any background.
    img.fill(x, y, w, h, [20, 20, 20]);
    for (i, ch) in text.bytes().enumerate() {
        let glyph = DIGITS[(ch - b'0') as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    img.fill(x + px + i * 4 * px + col * px, y + px + row * px, px, px, [255, 255, 255]);
                }
            }
        }
    }
}

/// Draws the grid lines and a number in each cell's top-left corner.
pub fn draw_grid(img: &mut Image, grid: &[Cell]) {
    let px = (img.width.min(img.height) / 160).clamp(2, 4);
    for c in grid {
        img.outline(*c, 2, [255, 40, 160]);
    }
    for (i, c) in grid.iter().enumerate() {
        label(img, c.x + 3, c.y + 3, i + 1, px);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_covers_the_image() {
        let g = cells(101, 57, 4, 4);
        assert_eq!(g.len(), 16);
        assert_eq!(g.iter().map(|c| c.width * c.height).sum::<usize>(), 101 * 57);
        let mut img = Image { width: 101, height: 57, rgb: vec![0; 101 * 57 * 3] };
        draw_grid(&mut img, &g);
        let png = img.encode().unwrap();
        let back = Image::decode(&png).unwrap();
        assert_eq!((back.width, back.height), (101, 57));
    }
}

#[cfg(test)]
mod preview {
    /// `OCU_GRID_IN=shot.png OCU_GRID_OUT=out.png cargo test grid_preview -- --ignored`
    #[test]
    #[ignore]
    fn grid_preview() {
        use super::*;
        let input = std::env::var("OCU_GRID_IN").unwrap();
        let mut img = Image::decode(&std::fs::read(input).unwrap()).unwrap();
        let grid = cells(img.width, img.height, 4, 4);
        draw_grid(&mut img, &grid);
        std::fs::write(std::env::var("OCU_GRID_OUT").unwrap(), img.encode().unwrap()).unwrap();
    }
}
