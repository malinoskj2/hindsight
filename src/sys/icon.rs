//! The application icon, rendered from one description so that the tray, the window and the
//! executable's icon resource (`assets/dxgi.ico`) always match.
//!
//! A rounded blue square with a white play triangle and a red recording dot.

const BACKGROUND: [u8; 3] = [0x25, 0x63, 0xEB];
const GLYPH: [u8; 3] = [0xFF, 0xFF, 0xFF];
const DOT: [u8; 3] = [0xEF, 0x44, 0x44];
/// The recording dot's centre and radius as fractions of the tile.
const DOT_CENTRE: (f32, f32) = (0.76, 0.24);
const DOT_RADIUS: f32 = 0.11;
/// Fraction of the tile used for the corner radius.
const CORNER_RADIUS: f32 = 0.22;
/// Triangle corners as fractions of the tile.
const TRIANGLE: [(f32, f32); 3] = [(0.37, 0.27), (0.37, 0.73), (0.76, 0.50)];
/// Subsamples per axis for anti-aliasing.
const SUPERSAMPLE: u32 = 4;
/// Sizes stored in the `.ico` resource; Explorer and the Start menu pick the closest.
pub(crate) const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
const ENTRY_COUNT: u16 = 7;
const _: () = assert!(ENTRY_COUNT as usize == ICO_SIZES.len());

/// `.ico` directory fields are 32-bit; the icon is a few hundred kilobytes.
fn length_field(bytes: usize) -> u32 {
    u32::try_from(bytes).expect("icon data fits a 32-bit directory field")
}

/// RGBA pixels, row-major from the top-left, with straight (unmultiplied) alpha.
pub(crate) fn rgba(size: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
    for y in 0..size {
        for x in 0..size {
            let mut tile = 0.0;
            let mut glyph = 0.0;
            let mut dot = 0.0;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let px = (x as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
                    if inside_tile(px, py) {
                        tile += 1.0;
                        if inside_triangle(px, py) {
                            glyph += 1.0;
                        } else if inside_dot(px, py) {
                            dot += 1.0;
                        }
                    }
                }
            }
            let coverage = tile / samples;
            let (glyph_share, dot_share) = if tile > 0.0 {
                (glyph / tile, dot / tile)
            } else {
                (0.0, 0.0)
            };
            for channel in 0..3 {
                let mixed = f32::from(BACKGROUND[channel]) * (1.0 - glyph_share - dot_share)
                    + f32::from(GLYPH[channel]) * glyph_share
                    + f32::from(DOT[channel]) * dot_share;
                pixels.push(to_byte(mixed));
            }
            pixels.push(to_byte(coverage * 255.0));
        }
    }

    pixels
}

/// The icon as a Windows `.ico` file with 32-bit BMP entries for every size in `ICO_SIZES`.
pub(crate) fn ico() -> Vec<u8> {
    let header_bytes = 6 + 16 * ICO_SIZES.len();
    let mut directory = Vec::with_capacity(header_bytes);
    directory.extend_from_slice(&0_u16.to_le_bytes());
    directory.extend_from_slice(&1_u16.to_le_bytes());
    directory.extend_from_slice(&ENTRY_COUNT.to_le_bytes());

    let mut images = Vec::new();
    for size in ICO_SIZES {
        let image = bmp_entry(size);
        let offset = header_bytes + images.len();
        // 256 is stored as 0 in the one-byte dimension fields.
        let dimension = u8::try_from(size).unwrap_or(0);
        directory.push(dimension);
        directory.push(dimension);
        directory.push(0);
        directory.push(0);
        directory.extend_from_slice(&1_u16.to_le_bytes());
        directory.extend_from_slice(&32_u16.to_le_bytes());
        directory.extend_from_slice(&length_field(image.len()).to_le_bytes());
        directory.extend_from_slice(&length_field(offset).to_le_bytes());
        images.extend_from_slice(&image);
    }

    directory.extend_from_slice(&images);
    directory
}

/// A `BITMAPINFOHEADER` followed by bottom-up BGRA rows and an empty 1-bit mask.
fn bmp_entry(size: u32) -> Vec<u8> {
    let pixels = rgba(size);
    let mask_row_bytes = size.div_ceil(32) * 4;
    let mut entry = Vec::with_capacity(40 + pixels.len() + (mask_row_bytes * size) as usize);
    entry.extend_from_slice(&40_u32.to_le_bytes());
    entry.extend_from_slice(&size.to_le_bytes());
    entry.extend_from_slice(&(size * 2).to_le_bytes());
    entry.extend_from_slice(&1_u16.to_le_bytes());
    entry.extend_from_slice(&32_u16.to_le_bytes());
    entry.extend_from_slice(&0_u32.to_le_bytes());
    entry.extend_from_slice(&(size * size * 4 + mask_row_bytes * size).to_le_bytes());
    entry.extend_from_slice(&[0; 16]);

    let row_bytes = (size * 4) as usize;
    for row in pixels.chunks_exact(row_bytes).rev() {
        for pixel in row.as_chunks::<4>().0 {
            entry.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    entry.resize(entry.len() + (mask_row_bytes * size) as usize, 0);
    entry
}

fn inside_tile(x: f32, y: f32) -> bool {
    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
        return false;
    }

    // Distance past the straight edges; inside the cross-shaped body or a corner circle.
    let dx = (x - 0.5).abs() - (0.5 - CORNER_RADIUS);
    let dy = (y - 0.5).abs() - (0.5 - CORNER_RADIUS);
    dx <= 0.0 || dy <= 0.0 || dx * dx + dy * dy <= CORNER_RADIUS * CORNER_RADIUS
}

fn inside_dot(x: f32, y: f32) -> bool {
    let (dx, dy) = (x - DOT_CENTRE.0, y - DOT_CENTRE.1);
    dx * dx + dy * dy <= DOT_RADIUS * DOT_RADIUS
}

fn inside_triangle(x: f32, y: f32) -> bool {
    let [first, second, third] = TRIANGLE;
    let side = |from: (f32, f32), to: (f32, f32)| {
        (to.0 - from.0) * (y - from.1) - (to.1 - from.1) * (x - from.0)
    };
    let sides = [side(first, second), side(second, third), side(third, first)];
    sides.iter().all(|value| *value >= 0.0) || sides.iter().all(|value| *value <= 0.0)
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_byte(value: f32) -> u8 {
    // Clamped to 0..=255 first, so the cast neither truncates nor loses a sign.
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent_and_the_centre_is_the_glyph() {
        let size = 64;
        let pixels = rgba(size);
        assert_eq!(pixels.len(), (size * size * 4) as usize);

        let at = |x: u32, y: u32| {
            let index = ((y * size + x) * 4) as usize;
            [
                pixels[index],
                pixels[index + 1],
                pixels[index + 2],
                pixels[index + 3],
            ]
        };
        assert_eq!(at(0, 0)[3], 0);
        assert_eq!(at(size - 1, size - 1)[3], 0);
        assert_eq!(at(size / 2, size / 2), [0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(at(size / 8, size / 2), [0x25, 0x63, 0xEB, 0xFF]);
        assert_eq!(at(48, 15), [0xEF, 0x44, 0x44, 0xFF]);
    }

    #[test]
    fn ico_directory_lists_every_size_with_consistent_offsets() {
        let bytes = ico();
        assert_eq!(&bytes[..6], &[0, 0, 1, 0, 7, 0]);

        let mut expected_offset = 6 + 16 * ICO_SIZES.len();
        for (index, size) in ICO_SIZES.iter().enumerate() {
            let entry = &bytes[6 + 16 * index..6 + 16 * (index + 1)];
            assert_eq!(u32::from(entry[0]), size % 256);
            let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
            let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
            assert_eq!(offset, expected_offset);
            assert_eq!(&bytes[offset..offset + 4], &40_u32.to_le_bytes());
            expected_offset += length;
        }
        assert_eq!(expected_offset, bytes.len());
    }

    #[test]
    fn committed_ico_matches_the_renderer() {
        let committed: &[u8] = include_bytes!("../../assets/dxgi.ico");
        assert!(
            committed == ico().as_slice(),
            "regenerate assets/dxgi.ico with `dxgi icon assets/dxgi.ico`"
        );
    }
}
