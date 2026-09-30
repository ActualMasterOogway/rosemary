//! Software composition of premultiplied B, G, R, A pixels, the layout both
//! `wl_shm`'s ARGB8888 on a little-endian host and `appkit::backing::Overlay`
//! use.

/// Where the pixels are drawn: a whole frame, zeroed (transparent) at first.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![0; width * height * 4],
        }
    }
}

/// One surface's buffer, borrowed for the draw.
pub struct Source<'a> {
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    /// XRGB8888: the fourth byte is padding, not alpha.
    pub opaque: bool,
}

/// A rectangle in whole pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// Draw the `from` part of `source` over the `to` rectangle of `canvas`,
/// scaling by nearest sample when their sizes differ, clipped to both.
pub fn draw(canvas: &mut Canvas, source: &Source<'_>, from: Rect, to: Rect) {
    if from.width <= 0 || from.height <= 0 || to.width <= 0 || to.height <= 0 {
        return;
    }
    let left = to.x.max(0);
    let right = (to.x + to.width).min(canvas.width as i64);
    let top = to.y.max(0);
    let bottom = (to.y + to.height).min(canvas.height as i64);
    if left >= right || top >= bottom {
        return;
    }
    // Where a destination column and row sample the source, or `None` for
    // one that falls outside the buffer.
    let sample = |at: i64, start: i64, span: i64, from_start: i64, from_span: i64, limit: usize| {
        let offset = (at - start) * from_span / span;
        let index = from_start + offset;
        (index >= 0 && (index as usize) < limit).then_some(index as usize)
    };
    let columns: Vec<Option<usize>> = (left..right)
        .map(|x| sample(x, to.x, to.width, from.x, from.width, source.width))
        .collect();
    let unscaled = from.width == to.width;
    for y in top..bottom {
        let Some(row) = sample(y, to.y, to.height, from.y, from.height, source.height) else {
            continue;
        };
        let source_row = &source.pixels[row * source.stride..];
        let start = (y as usize * canvas.width + left as usize) * 4;
        let target = &mut canvas.pixels[start..start + (right - left) as usize * 4];
        if unscaled && let Some(Some(first)) = columns.first() {
            let count = columns.len();
            if first + count <= source.width {
                blend_row(
                    target,
                    &source_row[first * 4..(first + count) * 4],
                    source.opaque,
                );
                continue;
            }
        }
        for (pixel, column) in target.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            if let Some(column) = column {
                blend_row(
                    pixel,
                    &source_row[column * 4..column * 4 + 4],
                    source.opaque,
                );
            }
        }
    }
}

/// `source` over `target`, pixel by pixel, both premultiplied.
fn blend_row(target: &mut [u8], source: &[u8], opaque: bool) {
    if opaque {
        target.copy_from_slice(source);
        for pixel in target.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        return;
    }
    for (to, from) in target
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(source.as_chunks::<4>().0)
    {
        match from[3] {
            255 => to.copy_from_slice(from),
            0 => {}
            alpha => {
                let keep = 255 - u32::from(alpha);
                for channel in 0..4 {
                    let under = (u32::from(to[channel]) * keep + 127) / 255;
                    to[channel] = (u32::from(from[channel]) + under).min(255) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i64, y: i64, width: i64, height: i64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn an_opaque_buffer_is_copied_with_full_alpha() {
        let pixels = [1, 2, 3, 0, 4, 5, 6, 0];
        let source = Source {
            pixels: &pixels,
            width: 2,
            height: 1,
            stride: 8,
            opaque: true,
        };
        let mut canvas = Canvas::new(2, 1);
        draw(&mut canvas, &source, rect(0, 0, 2, 1), rect(0, 0, 2, 1));
        assert_eq!(canvas.pixels, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn a_translucent_pixel_is_blended_over_what_is_below() {
        let mut canvas = Canvas::new(1, 1);
        canvas.pixels.copy_from_slice(&[200, 200, 200, 255]);
        // Half-transparent black, premultiplied.
        let pixels = [0, 0, 0, 128];
        let source = Source {
            pixels: &pixels,
            width: 1,
            height: 1,
            stride: 4,
            opaque: false,
        };
        draw(&mut canvas, &source, rect(0, 0, 1, 1), rect(0, 0, 1, 1));
        assert_eq!(canvas.pixels, [100, 100, 100, 255]);
    }

    #[test]
    fn a_popup_past_the_edge_is_clipped() {
        let pixels = [9u8; 4 * 4];
        let source = Source {
            pixels: &pixels,
            width: 2,
            height: 2,
            stride: 8,
            opaque: false,
        };
        let mut canvas = Canvas::new(3, 3);
        draw(&mut canvas, &source, rect(0, 0, 2, 2), rect(2, 2, 2, 2));
        let drawn: Vec<usize> = canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, p)| p[0] == 9)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(drawn, [8]);
    }

    #[test]
    fn a_larger_buffer_is_sampled_down_into_its_rectangle() {
        // Two columns, the left red and the right blue, drawn into one pixel
        // wide and then two pixels wide.
        let pixels = [0, 0, 255, 255, 255, 0, 0, 255];
        let source = Source {
            pixels: &pixels,
            width: 2,
            height: 1,
            stride: 8,
            opaque: false,
        };
        let mut canvas = Canvas::new(4, 1);
        draw(&mut canvas, &source, rect(0, 0, 2, 1), rect(0, 0, 4, 1));
        assert_eq!(
            canvas.pixels,
            [
                0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255, 255, 0, 0, 255
            ]
        );
    }
}
