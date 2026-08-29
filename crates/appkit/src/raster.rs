//! Decoding the containers `NSImage` accepts, and the TIFF it hands back.

/// An image in memory: RGBA, top row first, `width * height * 4` bytes.
#[derive(Clone, PartialEq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Bitmap {}x{} ({} bytes)",
            self.width,
            self.height,
            self.pixels.len()
        )
    }
}

impl Bitmap {
    pub fn new(width: u32, height: u32) -> Bitmap {
        Bitmap {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    pub fn bytes_per_row(&self) -> usize {
        self.width as usize * 4
    }
}

/// Decode whatever `bytes` is, or `None` for a container this cannot read.
pub fn decode(bytes: &[u8]) -> Option<Bitmap> {
    if bytes.starts_with(b"BM") {
        return bmp(bytes);
    }
    if bytes.starts_with(b"II") || bytes.starts_with(b"MM") {
        return tiff(bytes);
    }
    None
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// An uncompressed Windows BMP of 24 or 32 bits per pixel.
///
/// The two things that are easy to get wrong: rows are padded to a multiple of
/// four bytes, and a positive height means the rows are stored bottom-up. A
/// decoder that ignores either produces an image that is skewed or upside down,
/// both of which look like a drawing bug much later.
fn bmp(bytes: &[u8]) -> Option<Bitmap> {
    let pixel_offset = le32(bytes, 10)? as usize;
    let header_size = le32(bytes, 14)?;
    if header_size < 40 {
        // A `BITMAPCOREHEADER` has twelve-bit fields in different places. Not
        // read rather than misread.
        return None;
    }
    let width = le32(bytes, 18)? as i32;
    let signed_height = le32(bytes, 22)? as i32;
    let bits = le16(bytes, 28)?;
    let compression = le32(bytes, 30)?;
    // 0 is `BI_RGB`, the only uncompressed one. `BI_BITFIELDS` (3) names its
    // channel masks in three words after the header and is not read here.
    if compression != 0 || !matches!(bits, 24 | 32) || width <= 0 || signed_height == 0 {
        return None;
    }

    let width = width as u32;
    let height = signed_height.unsigned_abs();
    let bottom_up = signed_height > 0;
    let source_bytes_per_pixel = bits as usize / 8;
    // Rows are padded to four bytes, which for 32-bit is already the case.
    let stride = (width as usize * source_bytes_per_pixel).next_multiple_of(4);

    let mut out = Bitmap::new(width, height);
    for row in 0..height as usize {
        let source_row = match bottom_up {
            true => height as usize - 1 - row,
            false => row,
        };
        let start = pixel_offset.checked_add(source_row.checked_mul(stride)?)?;
        let source = bytes.get(start..start + width as usize * source_bytes_per_pixel)?;
        let destination = row * out.bytes_per_row();
        for x in 0..width as usize {
            let p = &source[x * source_bytes_per_pixel..];
            // BMP stores blue, green, red - and alpha where it has one.
            out.pixels[destination + x * 4] = p[2];
            out.pixels[destination + x * 4 + 1] = p[1];
            out.pixels[destination + x * 4 + 2] = p[0];
            out.pixels[destination + x * 4 + 3] = match source_bytes_per_pixel {
                4 => p[3],
                _ => 0xff,
            };
        }
    }
    Some(out)
}

/// The TIFF tags this module writes and reads.
mod tag {
    pub const WIDTH: u16 = 256;
    pub const HEIGHT: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const ROWS_PER_STRIP: u16 = 278;
    pub const STRIP_BYTE_COUNTS: u16 = 279;
    pub const PLANAR_CONFIGURATION: u16 = 284;
    pub const EXTRA_SAMPLES: u16 = 338;

    pub const TYPE_SHORT: u16 = 3;
    pub const TYPE_LONG: u16 = 4;
}

/// `bitmap` as an uncompressed little-endian RGBA TIFF.
pub fn encode_tiff(bitmap: &Bitmap) -> Vec<u8> {
    // Eleven entries, each twelve bytes, after a two-byte count and before a
    // four-byte "no next directory" - so the data area starts at a fixed place
    // and the two out-of-line values go there in a known order.
    const ENTRIES: usize = 11;
    let ifd_at = 8usize;
    let data_at = ifd_at + 2 + ENTRIES * 12 + 4;
    let bits_at = data_at;
    let pixels_at = bits_at + 8;

    let mut out = Vec::with_capacity(pixels_at + bitmap.pixels.len());
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&(ifd_at as u32).to_le_bytes());

    out.extend_from_slice(&(ENTRIES as u16).to_le_bytes());
    let mut entry = |tag: u16, kind: u16, count: u32, value: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        // A value narrower than four bytes sits in the low end of the field
        // rather than being right-justified, which is the rule most often got
        // backwards.
        out.extend_from_slice(&value.to_le_bytes());
    };
    entry(tag::WIDTH, tag::TYPE_LONG, 1, bitmap.width);
    entry(tag::HEIGHT, tag::TYPE_LONG, 1, bitmap.height);
    entry(tag::BITS_PER_SAMPLE, tag::TYPE_SHORT, 4, bits_at as u32);
    entry(tag::COMPRESSION, tag::TYPE_SHORT, 1, 1);
    // 2 is RGB.
    entry(tag::PHOTOMETRIC, tag::TYPE_SHORT, 1, 2);
    entry(tag::STRIP_OFFSETS, tag::TYPE_LONG, 1, pixels_at as u32);
    entry(tag::SAMPLES_PER_PIXEL, tag::TYPE_SHORT, 1, 4);
    entry(tag::ROWS_PER_STRIP, tag::TYPE_LONG, 1, bitmap.height);
    entry(
        tag::STRIP_BYTE_COUNTS,
        tag::TYPE_LONG,
        1,
        bitmap.pixels.len() as u32,
    );
    entry(tag::PLANAR_CONFIGURATION, tag::TYPE_SHORT, 1, 1);
    // 2 is "unassociated alpha", which is what an un-premultiplied RGBA is.
    entry(tag::EXTRA_SAMPLES, tag::TYPE_SHORT, 1, 2);
    out.extend_from_slice(&0u32.to_le_bytes());

    debug_assert_eq!(out.len(), bits_at);
    for _ in 0..4 {
        out.extend_from_slice(&8u16.to_le_bytes());
    }
    debug_assert_eq!(out.len(), pixels_at);
    out.extend_from_slice(&bitmap.pixels);
    out
}

/// An uncompressed TIFF, of the shape [`encode_tiff`] writes.
///
/// Little-endian only, which is what `II` says. A `MM` file is big-endian and is
/// declined rather than read with the bytes the wrong way round.
fn tiff(bytes: &[u8]) -> Option<Bitmap> {
    if !bytes.starts_with(b"II") || le16(bytes, 2)? != 42 {
        return None;
    }
    let ifd = le32(bytes, 4)? as usize;
    let count = le16(bytes, ifd)? as usize;

    let mut fields: Vec<(u16, u16, u32, u32)> = Vec::with_capacity(count);
    for i in 0..count {
        let at = ifd + 2 + i * 12;
        fields.push((
            le16(bytes, at)?,
            le16(bytes, at + 2)?,
            le32(bytes, at + 4)?,
            le32(bytes, at + 8)?,
        ));
    }
    let field = |wanted: u16| fields.iter().find(|(t, ..)| *t == wanted).copied();
    // A single-count SHORT sits in the low half of its value field.
    let scalar = |wanted: u16| {
        field(wanted).map(|(_, kind, _, value)| match kind {
            tag::TYPE_SHORT => value & 0xffff,
            _ => value,
        })
    };

    let width = scalar(tag::WIDTH)?;
    let height = scalar(tag::HEIGHT)?;
    let samples = scalar(tag::SAMPLES_PER_PIXEL).unwrap_or(1);
    let compression = scalar(tag::COMPRESSION).unwrap_or(1);
    let planar = scalar(tag::PLANAR_CONFIGURATION).unwrap_or(1);
    if compression != 1 || planar != 1 || !matches!(samples, 3 | 4) {
        return None;
    }
    // Every sample has to be eight bits; the array is out of line for more than
    // two samples, and inline for one or two.
    let (_, _, bits_count, bits_value) = field(tag::BITS_PER_SAMPLE)?;
    let bits_ok = match bits_count > 2 {
        true => (0..bits_count as usize)
            .all(|i| le16(bytes, bits_value as usize + i * 2) == Some(8)),
        false => bits_value & 0xffff == 8,
    };
    if !bits_ok {
        return None;
    }

    // One strip, which is what `rows_per_strip >= height` means and what this
    // module writes. A multi-strip file would need the offsets array walked.
    let rows_per_strip = scalar(tag::ROWS_PER_STRIP).unwrap_or(height);
    let (_, _, strips, first_strip) = field(tag::STRIP_OFFSETS)?;
    if rows_per_strip < height || strips != 1 {
        return None;
    }

    let source_bytes_per_pixel = samples as usize;
    let stride = width as usize * source_bytes_per_pixel;
    let mut out = Bitmap::new(width, height);
    for row in 0..height as usize {
        let start = (first_strip as usize).checked_add(row.checked_mul(stride)?)?;
        let source = bytes.get(start..start + stride)?;
        let destination = row * out.bytes_per_row();
        for x in 0..width as usize {
            let p = &source[x * source_bytes_per_pixel..];
            out.pixels[destination + x * 4] = p[0];
            out.pixels[destination + x * 4 + 1] = p[1];
            out.pixels[destination + x * 4 + 2] = p[2];
            out.pixels[destination + x * 4 + 3] = match source_bytes_per_pixel {
                4 => p[3],
                _ => 0xff,
            };
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bitmap {
        let mut bitmap = Bitmap::new(3, 2);
        for (i, byte) in bitmap.pixels.iter_mut().enumerate() {
            *byte = i as u8;
        }
        bitmap
    }

    /// The encoder and the decoder are each other's inverse, which is the whole
    /// claim: `-TIFFRepresentation` is a round trip through this pair, because
    /// the guest hands the bytes straight back to `NSBitmapImageRep`.
    #[test]
    fn a_tiff_round_trips() {
        let original = sample();
        let encoded = encode_tiff(&original);
        assert_eq!(decode(&encoded).as_ref(), Some(&original));
    }

    /// A BMP's rows are stored bottom-up when its height is positive, and the
    /// decoder has to put them back the other way.
    #[test]
    fn a_bottom_up_bmp_comes_back_the_right_way_up() {
        // Two rows of one pixel, 24-bit: each row is one pixel plus padding to
        // four bytes. The first row stored is the *bottom* one.
        let mut file = b"BM".to_vec();
        file.resize(10, 0);
        file.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        file.extend_from_slice(&40u32.to_le_bytes()); // header size
        file.extend_from_slice(&1i32.to_le_bytes()); // width
        file.extend_from_slice(&2i32.to_le_bytes()); // height, positive
        file.extend_from_slice(&1u16.to_le_bytes()); // planes
        file.extend_from_slice(&24u16.to_le_bytes()); // bits
        file.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        file.resize(54, 0);
        // Bottom row blue, top row red - written bottom first.
        file.extend_from_slice(&[0xff, 0x00, 0x00, 0]);
        file.extend_from_slice(&[0x00, 0x00, 0xff, 0]);

        let decoded = decode(&file).expect("a 24-bit BMP");
        assert_eq!(decoded.width, 1);
        assert_eq!(decoded.height, 2);
        // Row 0 is the top row, which is the one stored last: red.
        assert_eq!(&decoded.pixels[0..4], &[0xff, 0x00, 0x00, 0xff]);
        assert_eq!(&decoded.pixels[4..8], &[0x00, 0x00, 0xff, 0xff]);
    }

    /// A top-down BMP says so with a negative height, and is not flipped twice.
    #[test]
    fn a_top_down_bmp_is_not_flipped() {
        let mut file = b"BM".to_vec();
        file.resize(10, 0);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&1i32.to_le_bytes());
        file.extend_from_slice(&(-2i32).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&24u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.resize(54, 0);
        file.extend_from_slice(&[0x00, 0x00, 0xff, 0]);
        file.extend_from_slice(&[0xff, 0x00, 0x00, 0]);

        let decoded = decode(&file).expect("a top-down BMP");
        assert_eq!(&decoded.pixels[0..4], &[0xff, 0x00, 0x00, 0xff]);
    }

    /// A compression this module cannot read is declined rather than read as if
    /// it were not compressed.
    #[test]
    fn a_compressed_bmp_is_declined() {
        let mut file = b"BM".to_vec();
        file.resize(10, 0);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&1i32.to_le_bytes());
        file.extend_from_slice(&1i32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&24u16.to_le_bytes());
        // `BI_BITFIELDS`.
        file.extend_from_slice(&3u32.to_le_bytes());
        file.resize(64, 0);
        assert_eq!(decode(&file), None);
    }
}
