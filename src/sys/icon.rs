//! The application icons: the owl logos resampled into the `.ico` files `build.rs` embeds.

use crate::config::Theme;
use std::{fs, io, path::Path};
use table::{ICONS, IconEntry};

mod table;

const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

const fn entry(theme: Theme) -> IconEntry {
    match theme {
        Theme::Classic => table::CLASSIC,
        Theme::Ember => table::EMBER,
        Theme::Selene => table::SELENE,
        Theme::Nova => table::NOVA,
    }
}

/// Resource id of the embedded icon for `theme`.
pub(crate) const fn resource(theme: Theme) -> u16 {
    entry(theme).id
}

/// Writes every theme's `.ico` under `assets` from the logos under `assets/logo`.
pub(crate) fn write_all(assets: &Path) -> io::Result<()> {
    for IconEntry { logo, ico, .. } in ICONS {
        let picture = Picture::decode(&fs::read(assets.join(logo))?)?;
        fs::write(assets.join(ico), picture.ico()?)?;
    }
    Ok(())
}

/// A decoded picture: straight-alpha RGBA rows, starting at the top-left.
struct Picture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Picture {
    fn decode(png_bytes: &[u8]) -> io::Result<Self> {
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        let mut decoder = png::Decoder::new(io::Cursor::new(png_bytes));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().map_err(io::Error::other)?;
        let mut rgba = vec![
            0;
            reader
                .output_buffer_size()
                .ok_or_else(|| bad("empty PNG"))?
        ];
        let info = reader.next_frame(&mut rgba).map_err(io::Error::other)?;
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err(bad("the logo must be an 8-bit RGBA PNG"));
        }
        rgba.truncate(info.buffer_size());
        Ok(Self {
            width: info.width,
            height: info.height,
            rgba,
        })
    }

    /// Encodes the icon with BMP entries rather than PNG, even at 256 pixels, so that every icon
    /// reader can decode them.
    fn ico(&self) -> io::Result<Vec<u8>> {
        let mut directory = ico::IconDir::new(ico::ResourceType::Icon);
        for size in ICO_SIZES {
            let image = ico::IconImage::from_rgba_data(size, size, self.resampled(size));
            directory.add_entry(ico::IconDirEntry::encode_as_bmp(&image)?);
        }
        let mut bytes = Vec::new();
        directory.write(&mut bytes)?;
        Ok(bytes)
    }

    /// Resamples to `size` by area averaging in premultiplied colour, so that transparent
    /// surroundings never bleed into the result.
    fn resampled(&self, size: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            let (top, bottom) = span(y, size, self.height);
            for x in 0..size {
                let (left, right) = span(x, size, self.width);
                let mut sum = [0.0f64; 4];
                let mut count = 0.0f64;
                for sy in top..bottom {
                    for sx in left..right {
                        let at = ((sy * self.width + sx) * 4) as usize;
                        let alpha = f64::from(self.rgba[at + 3]) / 255.0;
                        for (channel, total) in sum.iter_mut().take(3).enumerate() {
                            *total += f64::from(self.rgba[at + channel]) * alpha;
                        }
                        sum[3] += alpha;
                        count += 1.0;
                    }
                }
                let colour = |channel: usize| {
                    if sum[3] > 0.0 {
                        to_byte(sum[channel] / sum[3])
                    } else {
                        0
                    }
                };
                out.extend([
                    colour(0),
                    colour(1),
                    colour(2),
                    to_byte(sum[3] / count * 255.0),
                ]);
            }
        }
        out
    }
}

/// The range of source rows or columns that map to destination `index` of `size`.
fn span(index: u32, size: u32, source: u32) -> (u32, u32) {
    let start = index * source / size;
    let end = ((index + 1) * source / size).max(start + 1).min(source);
    (start, end)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is rounded and clamped to 0..=255 first"
)]
fn to_byte(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Picture {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                rgba.extend(pixel(x, y));
            }
        }
        Picture {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn resampling_averages_boxes_and_ignores_transparent_colour() {
        let source = picture(4, 4, |x, y| {
            if x < 2 && y < 2 {
                [255, 0, 0, 255]
            } else if x >= 2 && y < 2 {
                [0, 0, 255, 255]
            } else {
                [0, 255, 0, 0]
            }
        });
        let small = source.resampled(2);
        assert_eq!(&small[0..4], [255, 0, 0, 255]);
        assert_eq!(&small[4..8], [0, 0, 255, 255]);
        assert_eq!(small[11], 0);
        assert_eq!(small[15], 0);

        let mixed = picture(2, 1, |x, _| {
            if x == 0 {
                [255, 0, 0, 255]
            } else {
                [0, 255, 0, 0]
            }
        });
        assert_eq!(mixed.resampled(1), [255, 0, 0, 128]);
    }

    #[test]
    fn spans_cover_the_source_exactly_once() {
        let covered: Vec<u32> = (0..3)
            .flat_map(|i| span(i, 3, 10).0..span(i, 3, 10).1)
            .collect();
        assert_eq!(covered, (0..10).collect::<Vec<_>>());
        assert_eq!(span(0, 4, 2), (0, 1));
        assert_eq!(span(3, 4, 2), (1, 2));
    }

    #[test]
    fn the_ico_holds_every_size_as_a_32_bit_bmp() {
        let source = picture(8, 8, |x, y| {
            let byte = |value: u32| u8::try_from(value).unwrap();
            [byte(x * 30), byte(y * 30), 0, 255 - byte(x * 7)]
        });
        let directory = ico::IconDir::read(io::Cursor::new(source.ico().unwrap())).unwrap();
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
            assert_eq!(
                image.rgba_data(),
                source.resampled(entry.width()).as_slice()
            );
        }
    }

    #[test]
    fn the_committed_icons_match_the_logos() {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        for IconEntry { logo, ico, .. } in ICONS {
            let picture = Picture::decode(&fs::read(assets.join(logo)).unwrap()).unwrap();
            assert!(
                picture.ico().unwrap() == fs::read(assets.join(ico)).unwrap(),
                "regenerate the icons with `hindsight icon assets`"
            );
        }
    }

    #[test]
    fn every_theme_has_its_own_icon_in_the_table() {
        assert_eq!(ICONS.len(), Theme::ALL.len());
        let mut ids = Vec::new();
        for theme in Theme::ALL {
            let icon = entry(theme);
            assert!(ICONS.contains(&icon), "{theme:?} is embedded");
            assert!(icon.id > 0, "resource ids start at 1");
            ids.push(icon.id);
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ICONS.len(), "the ids are distinct");
    }
}
