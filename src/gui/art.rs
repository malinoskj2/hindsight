//! Theme artwork: PNG layers decoded once, then scaled to the size they are drawn at and kept
//! as bitmaps GDI can blend. Everything here is plain arithmetic on pixels; only the bitmap
//! itself is a Windows object.

use winsafe::{self as w};

/// Bytes in a pixel: blue, green, red, assert!(alphas.iter().any(|alpha| *alpha >= 250), "{name}");, with the colour premultiplied by the alpha as
/// `AlphaBlend` expects.
const PIXEL: usize = 4;

/// A decoded picture, top row first.
pub(super) struct Image {
    width: usize,
    height: usize,
    /// Premultiplied BGRA.
    pixels: Vec<u8>,
}

impl Image {
    /// Decodes an 8-bit RGBA PNG; anything else is not artwork this app ships.
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
        Some(Self {
            width: usize::try_from(info.width).ok()?,
            height: usize::try_from(info.height).ok()?,
            pixels,
        })
    }

    /// This picture shrunk (or stretched) to `height` rows, keeping its proportions. Each new
    /// pixel averages the old ones it covers, so a thin line stays a line instead of vanishing.
    pub(super) fn scaled_to_height(&self, height: usize) -> Self {
        let height = height.max(1);
        let width = (self.width * height).div_ceil(self.height).max(1);
        // Rows first, then columns: each pass averages a run of source pixels per target pixel.
        let across = resample(
            &self.pixels,
            self.width,
            self.height,
            width,
            self.height,
            true,
        );
        let pixels = resample(&across, width, self.height, width, height, false);
        Self {
            width,
            height,
            pixels,
        }
    }

    pub(super) fn size(&self) -> (i32, i32) {
        let clamp = |length: usize| i32::try_from(length).unwrap_or(i32::MAX);
        (clamp(self.width), clamp(self.height))
    }
}

/// Averages `source` (`from_width` by `from_height`) down to `to_width` by `to_height` along
/// one axis: horizontally when `horizontal`, else vertically; the other axis must not change.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn resample(
    source: &[u8],
    from_width: usize,
    from_height: usize,
    to_width: usize,
    to_height: usize,
    horizontal: bool,
) -> Vec<u8> {
    let (from_len, to_len) = if horizontal {
        (from_width, to_width)
    } else {
        (from_height, to_height)
    };
    let lines = if horizontal { from_height } else { from_width };
    let mut target = vec![0; to_width * to_height * PIXEL];
    let scale = from_len as f32 / to_len as f32;
    for index in 0..to_len {
        let start = index as f32 * scale;
        let end = start + scale;
        // The source pixels this target pixel covers, and how much of each.
        let first = start.floor() as usize;
        let last = (end.ceil() as usize).min(from_len);
        for line in 0..lines {
            let mut sum = [0.0_f32; PIXEL];
            let mut weight = 0.0;
            for position in first..last {
                let overlap =
                    (end.min(position as f32 + 1.0) - start.max(position as f32)).max(0.0);
                let offset = if horizontal {
                    (line * from_width + position) * PIXEL
                } else {
                    (position * from_width + line) * PIXEL
                };
                for (channel, total) in sum.iter_mut().enumerate() {
                    *total += f32::from(source[offset + channel]) * overlap;
                }
                weight += overlap;
            }
            let offset = if horizontal {
                (line * to_width + index) * PIXEL
            } else {
                (index * to_width + line) * PIXEL
            };
            for (channel, total) in sum.iter().enumerate() {
                target[offset + channel] = (total / weight.max(f32::EPSILON)).round() as u8;
            }
        }
    }
    target
}

/// A picture ready to be blended onto a window.
pub(super) struct Layer {
    bitmap: w::guard::DeleteObjectGuard<w::HBITMAP>,
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

    /// Blends the picture onto `hdc` with its top left corner at (`x`, `y`), at `opacity` out of
    /// 255 on top of its own transparency.
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

/// Blends `source`, `size` pixels square-on, onto `dest` at `origin`: over what is there, by the
/// picture's own alpha channel and then `opacity` out of 255. (`winsafe`'s `AlphaBlend` hands
/// Windows a pointer where it wants the blend settings themselves.)
#[expect(unsafe_code)]
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
    // SAFETY: both handles are device contexts that stay alive for the call, and the source
    // has the bitmap selected into it.
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

/// A 32-bit top-down bitmap holding `pixels`. `AlphaBlend` only honours a picture's own alpha
/// channel when the bitmap is a DIB section, which `winsafe` has no wrapper for.
#[expect(unsafe_code)]
fn dib_section(
    width: i32,
    height: i32,
    pixels: &[u8],
) -> Option<w::guard::DeleteObjectGuard<w::HBITMAP>> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
    };

    let wanted = usize::try_from(width).ok()? * usize::try_from(height).ok()? * PIXEL;
    if pixels.len() != wanted {
        return None;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).ok()?,
            biWidth: width,
            // A negative height makes the rows run top to bottom, as `pixels` does.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: `info` describes a 32-bit bitmap, and `bits` receives the address of its pixels,
    // which Windows owns for as long as the bitmap lives.
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
    if bits.is_null() {
        // SAFETY: the handle was just created and nothing else refers to it.
        let _ = unsafe { DeleteObject(bitmap.into()) };
        return None;
    }
    // SAFETY: the bitmap's pixel memory holds `width * height * 4` bytes, which is exactly
    // `pixels.len()`, and the two do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len()) };
    // SAFETY: the guard takes over the new bitmap handle, which nothing else frees.
    Some(unsafe { w::guard::DeleteObjectGuard::new(w::HBITMAP::from_ptr(bitmap.0)) })
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
        // The right column is white.
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
            // Some of the picture is opaque, and some is transparent.
            let alphas: Vec<u8> = scaled
                .pixels
                .as_chunks::<PIXEL>()
                .0
                .iter()
                .map(|pixel| pixel[3])
                .collect();
            // The artwork's most opaque pixels are 254 out of 255.
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
