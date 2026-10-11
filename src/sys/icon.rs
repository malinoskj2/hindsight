//! The application icon, rendered from one description so that the tray, the window and the
//! executable's icon resource (`assets/hindsight.ico`) always match.
//!
//! A rounded blue square with a white play triangle and a red recording dot.

/// The tile's colour.
const BACKGROUND: [u8; 3] = [0x25, 0x63, 0xEB];
/// Fraction of the tile used for the corner radius.
const CORNER_RADIUS: f32 = 0.22;
/// The play triangle's colour.
const GLYPH: [u8; 3] = [0xFF, 0xFF, 0xFF];
/// Triangle corners as fractions of the tile.
const TRIANGLE: [(f32, f32); 3] = [(0.37, 0.27), (0.37, 0.73), (0.76, 0.50)];
/// The recording dot's colour.
const DOT: [u8; 3] = [0xEF, 0x44, 0x44];
/// The recording dot's centre and radius as fractions of the tile.
const DOT_CENTRE: (f32, f32) = (0.76, 0.24);
const DOT_RADIUS: f32 = 0.11;
/// Subsamples per axis for anti-aliasing.
const SUPERSAMPLE: u32 = 4;

/// Sizes stored in the `.ico` resource; Explorer and the Start menu pick the closest.
#[cfg(windows)]
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// How many of a pixel's subsamples fall inside the tile, and how many of those land on the
/// glyph and on the dot.
struct Coverage {
    tile: f32,
    glyph: f32,
    dot: f32,
}

impl Coverage {
    /// Counts the subsamples of the pixel at `x`, `y` in a tile `size` pixels square.
    fn of(x: u32, y: u32, size: u32) -> Self {
        let subsamples = (0..SUPERSAMPLE).flat_map(|sy| (0..SUPERSAMPLE).map(move |sx| (sx, sy)));
        let mut coverage = Self {
            tile: 0.0,
            glyph: 0.0,
            dot: 0.0,
        };
        for (sx, sy) in subsamples {
            let px = (x as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
            let py = (y as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
            if !inside_tile(px, py) {
                continue;
            }
            coverage.tile += 1.0;
            if inside_triangle(px, py) {
                coverage.glyph += 1.0;
            } else if inside_dot(px, py) {
                coverage.dot += 1.0;
            }
        }
        coverage
    }
}

/// RGBA pixels, row-major from the top-left, with straight (unmultiplied) alpha.
pub(crate) fn rgba(size: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            pixels.extend_from_slice(&shade(&Coverage::of(x, y, size)));
        }
    }
    pixels
}

/// The icon as a Windows `.ico` file with 32-bit BMP entries for every size in `ICO_SIZES`.
/// BMP rather than PNG entries, even at 256 pixels, so that every icon reader understands them.
#[cfg(windows)]
pub(crate) fn ico() -> Vec<u8> {
    let mut directory = ico::IconDir::new(ico::ResourceType::Icon);
    for size in ICO_SIZES {
        let image = ico::IconImage::from_rgba_data(size, size, rgba(size));
        // Encoding into memory fails only for bit depths this image never uses.
        let entry = ico::IconDirEntry::encode_as_bmp(&image).expect("the icon encodes as a BMP");
        directory.add_entry(entry);
    }
    let mut bytes = Vec::new();
    directory
        .write(&mut bytes)
        .expect("writing to memory cannot fail");
    bytes
}

/// One RGBA pixel: the share of its subsamples inside the tile is its alpha, and the shares of
/// those on the glyph and the dot mix their colours into the background.
fn shade(coverage: &Coverage) -> [u8; 4] {
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
    let alpha = coverage.tile / samples;
    let (glyph_share, dot_share) = if coverage.tile > 0.0 {
        (coverage.glyph / coverage.tile, coverage.dot / coverage.tile)
    } else {
        (0.0, 0.0)
    };
    let mix = |channel: usize| {
        to_byte(
            f32::from(BACKGROUND[channel]) * (1.0 - glyph_share - dot_share)
                + f32::from(GLYPH[channel]) * glyph_share
                + f32::from(DOT[channel]) * dot_share,
        )
    };
    [mix(0), mix(1), mix(2), to_byte(alpha * 255.0)]
}

/// Whether a point, in fractions of the tile, is inside the rounded tile.
fn inside_tile(x: f32, y: f32) -> bool {
    if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
        return false;
    }

    // Distance past the straight edges; inside the cross-shaped body or a corner circle.
    let dx = (x - 0.5).abs() - (0.5 - CORNER_RADIUS);
    let dy = (y - 0.5).abs() - (0.5 - CORNER_RADIUS);
    dx <= 0.0 || dy <= 0.0 || dx * dx + dy * dy <= CORNER_RADIUS * CORNER_RADIUS
}

/// Whether a point, in fractions of the tile, is inside the play triangle.
fn inside_triangle(x: f32, y: f32) -> bool {
    let [first, second, third] = TRIANGLE;
    let side = |from: (f32, f32), to: (f32, f32)| {
        (to.0 - from.0) * (y - from.1) - (to.1 - from.1) * (x - from.0)
    };
    let sides = [side(first, second), side(second, third), side(third, first)];
    sides.iter().all(|value| *value >= 0.0) || sides.iter().all(|value| *value <= 0.0)
}

/// Whether a point, in fractions of the tile, is inside the recording dot.
fn inside_dot(x: f32, y: f32) -> bool {
    let (dx, dy) = (x - DOT_CENTRE.0, y - DOT_CENTRE.1);
    dx * dx + dy * dy <= DOT_RADIUS * DOT_RADIUS
}

/// A colour channel or alpha from its value on the 0 to 255 scale.
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

    #[cfg(windows)]
    #[test]
    fn the_ico_holds_every_size_as_a_32_bit_bmp_of_the_rendered_pixels() {
        let directory = ico::IconDir::read(std::io::Cursor::new(ico())).unwrap();

        let sizes: Vec<u32> = directory
            .entries()
            .iter()
            .map(ico::IconDirEntry::width)
            .collect();
        assert_eq!(sizes, ICO_SIZES);
        for entry in directory.entries() {
            assert!(!entry.is_png());
            assert_eq!(entry.bits_per_pixel(), 32);
            let image = entry.decode().unwrap();
            assert_eq!(image.rgba_data(), rgba(entry.width()).as_slice());
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_committed_ico_matches_the_renderer() {
        let committed: &[u8] = include_bytes!("../../assets/hindsight.ico");
        assert!(
            committed == ico().as_slice(),
            "regenerate assets/hindsight.ico with `hindsight icon assets/hindsight.ico`"
        );
    }
}
