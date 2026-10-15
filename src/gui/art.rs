//! Theme artwork: PNGs decoded once and scaled into bitmaps GDI can alpha-blend.

use super::layout::saturating_i32;
use winsafe::{self as w, guard::DeleteObjectGuard};

/// Bytes per pixel. Pixels are premultiplied BGRA, as `AlphaBlend` expects.
const PIXEL: usize = 4;

pub(super) struct Image {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Image {
    pub(super) fn decode(png_bytes: &[u8]) -> Option<Self> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().ok()?;
        let mut pixels = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut pixels).ok()?;
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        pixels.truncate(info.buffer_size());
        premultiply_to_bgra(&mut pixels);
        Some(Self {
            width: usize::try_from(info.width).ok()?,
            height: usize::try_from(info.height).ok()?,
            pixels,
        })
    }

    pub(super) fn scaled_to_height(&self, height: usize) -> Self {
        let height = height.max(1);
        let width = (self.width * height).div_ceil(self.height).max(1);
        let across = resample(
            &self.pixels,
            (self.width, self.height),
            (width, self.height),
            Axis::Horizontal,
        );
        let pixels = resample(
            &across,
            (width, self.height),
            (width, height),
            Axis::Vertical,
        );
        Self {
            width,
            height,
            pixels,
        }
    }

    pub(super) fn size(&self) -> (i32, i32) {
        (saturating_i32(self.width), saturating_i32(self.height))
    }
}

pub(super) struct Layer {
    bitmap: DeleteObjectGuard<w::HBITMAP>,
    width: i32,
    height: i32,
}

impl Layer {
    pub(super) fn new(image: &Image) -> Option<Self> {
        let (width, height) = image.size();
        Some(Self {
            bitmap: dib_section(width, height, &image.pixels)?,
            width,
            height,
        })
    }

    pub(super) const fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub(super) fn draw(&self, hdc: &w::HDC, x: i32, y: i32, opacity: u8) {
        let Ok(source) = hdc.CreateCompatibleDC() else {
            return;
        };
        let Ok(_selected) = source.SelectObject(&*self.bitmap) else {
            return;
        };
        alpha_blend(hdc, (x, y), &source, (self.width, self.height), opacity);
    }
}

#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

fn premultiply_to_bgra(pixels: &mut [u8]) {
    let (rows, _) = pixels.as_chunks_mut::<PIXEL>();
    for pixel in rows {
        let alpha = u32::from(pixel[3]);
        let [red, green, blue] = [pixel[0], pixel[1], pixel[2]].map(|channel| {
            u8::try_from((u32::from(channel) * alpha + 127) / 255).unwrap_or(u8::MAX)
        });
        pixel[0] = blue;
        pixel[1] = green;
        pixel[2] = red;
    }
}

fn resample(
    source: &[u8],
    (from_width, from_height): (usize, usize),
    (to_width, to_height): (usize, usize),
    axis: Axis,
) -> Vec<u8> {
    let horizontal = matches!(axis, Axis::Horizontal);
    let (from_len, to_len, lines) = if horizontal {
        (from_width, to_width, from_height)
    } else {
        (from_height, to_height, from_width)
    };
    let offset = |width: usize, line: usize, position: usize| {
        if horizontal {
            (line * width + position) * PIXEL
        } else {
            (position * width + line) * PIXEL
        }
    };
    let mut target = vec![0; to_width * to_height * PIXEL];
    let scale = from_len as f32 / to_len as f32;
    for index in 0..to_len {
        let covered = coverage(index, scale, from_len);
        for line in 0..lines {
            let pixels = covered
                .clone()
                .map(|(position, share)| (offset(from_width, line, position), share));
            let at = offset(to_width, line, index);
            target[at..at + PIXEL].copy_from_slice(&weighted_average(source, pixels));
        }
    }
    target
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the positions are non-negative and capped at the source length"
)]
fn coverage(
    index: usize,
    scale: f32,
    from_len: usize,
) -> impl Iterator<Item = (usize, f32)> + Clone {
    let start = index as f32 * scale;
    let end = start + scale;
    let first = start.floor() as usize;
    let last = (end.ceil() as usize).min(from_len);
    (first..last).map(move |position| {
        let overlap = (end.min(position as f32 + 1.0) - start.max(position as f32)).max(0.0);
        (position, overlap)
    })
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a weighted average of bytes stays within 0..=255"
)]
fn weighted_average(source: &[u8], pixels: impl Iterator<Item = (usize, f32)>) -> [u8; PIXEL] {
    let mut sum = [0.0_f32; PIXEL];
    let mut weight = 0.0_f32;
    for (offset, share) in pixels {
        for (channel, total) in sum.iter_mut().enumerate() {
            *total = f32::from(source[offset + channel]).mul_add(share, *total);
        }
        weight += share;
    }
    sum.map(|total| (total / weight.max(f32::EPSILON)).round() as u8)
}

/// Calls `AlphaBlend` directly, because `winsafe`'s wrapper passes the blend settings by
/// pointer.
#[expect(unsafe_code, reason = "calls AlphaBlend")]
fn alpha_blend(
    dest: &w::HDC,
    (x, y): (i32, i32),
    source: &w::HDC,
    (width, height): (i32, i32),
    opacity: u8,
) {
    use windows::Win32::Graphics::Gdi::{AlphaBlend, BLENDFUNCTION, HDC};

    /// `AC_SRC_OVER` and `AC_SRC_ALPHA`.
    const SOURCE_OVER: u8 = 0;
    const SOURCE_ALPHA: u8 = 1;

    let blend = BLENDFUNCTION {
        BlendOp: SOURCE_OVER,
        BlendFlags: 0,
        SourceConstantAlpha: opacity,
        AlphaFormat: SOURCE_ALPHA,
    };
    // SAFETY: both DCs are live for the call, and the bitmap is selected into `source`.
    let _ = unsafe {
        AlphaBlend(
            HDC(dest.ptr()),
            x,
            y,
            width,
            height,
            HDC(source.ptr()),
            0,
            0,
            width,
            height,
            blend,
        )
    };
}

/// Creates a DIB section holding `pixels`. `AlphaBlend` honours per-pixel alpha only for a DIB
/// section, and `winsafe` has no wrapper for `CreateDIBSection`.
#[expect(unsafe_code, reason = "calls CreateDIBSection")]
fn dib_section(width: i32, height: i32, pixels: &[u8]) -> Option<DeleteObjectGuard<w::HBITMAP>> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS,
    };

    let wanted = usize::try_from(width).ok()? * usize::try_from(height).ok()? * PIXEL;
    if pixels.len() != wanted {
        return None;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).ok()?,
            biWidth: width,
            // A negative height makes the rows run top to bottom.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: `info` describes a 32-bit bitmap, and `bits` receives its pixel pointer.
    let bitmap = unsafe {
        CreateDIBSection(
            None,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    }
    .ok()?;
    // SAFETY: `bitmap` is the live handle `CreateDIBSection` just returned.
    let bitmap = unsafe { w::HBITMAP::from_ptr(bitmap.0) };
    // SAFETY: the guard takes ownership of the new bitmap handle, which nothing else frees.
    let bitmap = unsafe { DeleteObjectGuard::new(bitmap) };
    if bits.is_null() {
        return None;
    }
    // SAFETY: the DIB holds exactly `pixels.len()` bytes and does not overlap `pixels`.
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len()) };
    Some(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(width: usize, height: usize, pixel: [u8; 4]) -> Image {
        Image {
            width,
            height,
            pixels: pixel.repeat(width * height),
        }
    }

    #[test]
    fn decoded_pixels_are_premultiplied_and_reordered_to_bgra() {
        let mut pixels = [200, 100, 50, 128, 255, 255, 255, 0];
        premultiply_to_bgra(&mut pixels);

        assert_eq!(pixels, [25, 50, 100, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn scaling_keeps_proportions_and_a_flat_colour() {
        let image = flat(8, 12, [10, 20, 30, 255]);
        let scaled = image.scaled_to_height(6);

        assert_eq!(scaled.size(), (4, 6));
        assert!(
            scaled
                .pixels
                .as_chunks::<PIXEL>()
                .0
                .iter()
                .all(|pixel| *pixel == [10, 20, 30, 255])
        );
    }

    #[test]
    fn scaling_averages_the_pixels_it_merges() {
        let mut image = flat(2, 2, [0, 0, 0, 255]);
        for row in 0..2 {
            let offset = (row * 2 + 1) * PIXEL;
            image.pixels[offset..offset + PIXEL].copy_from_slice(&[255, 255, 255, 255]);
        }
        let scaled = image.scaled_to_height(1);

        assert_eq!(scaled.size(), (1, 1));
        assert_eq!(scaled.pixels, [128, 128, 128, 255]);
    }

    #[test]
    fn the_shipped_artwork_decodes_and_scales() {
        for (name, bytes) in [
            (
                "ember",
                include_bytes!("../../assets/themes/ember/ember-character.png").as_slice(),
            ),
            (
                "selene",
                include_bytes!("../../assets/themes/selene/selene-character.png").as_slice(),
            ),
            (
                "nova",
                include_bytes!("../../assets/themes/nova/nova-character.png").as_slice(),
            ),
            (
                "ember ornament",
                include_bytes!("../../assets/themes/ember/ember-decoration.png").as_slice(),
            ),
        ] {
            let image = Image::decode(bytes).unwrap_or_else(|| panic!("{name} decodes"));
            assert_eq!(image.size(), (1024, 1536), "{name}");
            let scaled = image.scaled_to_height(340);
            assert_eq!(scaled.size(), (227, 340), "{name}");
            let alphas: Vec<u8> = scaled
                .pixels
                .as_chunks::<PIXEL>()
                .0
                .iter()
                .map(|pixel| pixel[3])
                .collect();
            assert!(alphas.iter().any(|alpha| *alpha >= 250), "{name}");
            assert!(alphas.contains(&0), "{name}");
        }
    }

    #[test]
    fn transparent_pixels_stay_transparent_when_merged() {
        let image = flat(4, 4, [0, 0, 0, 0]);
        let scaled = image.scaled_to_height(2);

        assert!(scaled.pixels.iter().all(|byte| *byte == 0));
    }
}
