//! How a monitor is driven: HDR, colour space, luminance range and SDR white level.

use std::fmt;
use windows::{
    Win32::{
        Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
            GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
        },
        Foundation::ERROR_SUCCESS,
        Graphics::{
            Dxgi::{
                Common::{
                    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_TYPE,
                },
                CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_OUTPUT_DESC1, IDXGIFactory1,
                IDXGIOutput6,
            },
            Gdi::HMONITOR,
        },
    },
    core::Interface,
};

/// `DISPLAYCONFIG_SDR_WHITE_LEVEL` is in thousandths of 80 nits.
const WHITE_LEVEL_UNIT: f32 = 1000.0;
pub(super) const SCRGB_NITS: f32 = 80.0;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Display {
    /// GDI device name, such as `\\.\DISPLAY1`.
    pub(super) name: String,
    pub(super) color_space: DXGI_COLOR_SPACE_TYPE,
    pub(super) bits_per_color: u32,
    pub(super) min_nits: f32,
    pub(super) max_nits: f32,
    pub(super) max_full_frame_nits: f32,
    pub(super) sdr_white_nits: Option<f32>,
}

impl Display {
    pub(super) fn is_hdr(&self) -> bool {
        is_hdr(self.color_space)
    }
}

impl fmt::Display for Display {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}, {} bits per colour, {:.4} to {:.0} nits ({:.0} full frame)",
            self.name,
            color_space_name(self.color_space),
            self.bits_per_color,
            self.min_nits,
            self.max_nits,
            self.max_full_frame_nits,
        )?;
        match self.sdr_white_nits {
            Some(nits) => write!(formatter, ", SDR white {nits:.0} nits")?,
            None => write!(formatter, ", SDR white unknown")?,
        }
        let hdr = if self.is_hdr() { "on" } else { "off" };
        write!(formatter, ", HDR {hdr}")
    }
}

/// Windows drives a monitor in PQ BT.2020 only while advanced colour is on.
pub(super) fn is_hdr(color_space: DXGI_COLOR_SPACE_TYPE) -> bool {
    color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
}

fn color_space_name(color_space: DXGI_COLOR_SPACE_TYPE) -> String {
    match color_space {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 => "SDR (sRGB, BT.709)".to_owned(),
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 => "HDR (PQ, BT.2020)".to_owned(),
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 => "scRGB (linear, BT.709)".to_owned(),
        other => format!("colour space {}", other.0),
    }
}

pub(super) fn white_level_nits(level: u32) -> f32 {
    level as f32 * SCRGB_NITS / WHITE_LEVEL_UNIT
}

pub(super) fn describe(monitor: HMONITOR) -> Option<Display> {
    output_of(monitor).map(|description| from_description(&description))
}

/// Skips the white-level look-up. A fresh DXGI factory each call: an older factory's outputs
/// keep reporting the colour space they had then.
pub(super) fn is_in_hdr(monitor: HMONITOR) -> Option<bool> {
    output_of(monitor).map(|description| is_hdr(description.ColorSpace))
}

#[cfg(test)]
fn all() -> windows::core::Result<Vec<Display>> {
    Ok(outputs()?.iter().map(from_description).collect())
}

fn output_of(monitor: HMONITOR) -> Option<DXGI_OUTPUT_DESC1> {
    outputs()
        .ok()?
        .into_iter()
        .find(|description| description.Monitor == monitor)
}

fn outputs() -> windows::core::Result<Vec<DXGI_OUTPUT_DESC1>> {
    // SAFETY: creates a factory; no other preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut descriptions = Vec::new();
    for adapter in 0.. {
        // SAFETY: enumeration ends with DXGI_ERROR_NOT_FOUND.
        let adapter = match unsafe { factory.EnumAdapters1(adapter) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error),
        };
        for output in 0.. {
            // SAFETY: as above.
            let output = match unsafe { adapter.EnumOutputs(output) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error),
            };
            // IDXGIOutput6 needs Windows 10 1803; an output without it counts as SDR.
            if let Ok(output) = output.cast::<IDXGIOutput6>() {
                // SAFETY: reads the description of a live output.
                descriptions.push(unsafe { output.GetDesc1() }?);
            }
        }
    }
    Ok(descriptions)
}

fn from_description(description: &DXGI_OUTPUT_DESC1) -> Display {
    let name = utf16_name(&description.DeviceName);
    Display {
        sdr_white_nits: sdr_white_level(&name),
        name,
        color_space: description.ColorSpace,
        bits_per_color: description.BitsPerColor,
        min_nits: description.MinLuminance,
        max_nits: description.MaxLuminance,
        max_full_frame_nits: description.MaxFullFrameLuminance,
    }
}

fn utf16_name(units: &[u16]) -> String {
    let length = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..length])
}

fn sdr_white_level(name: &str) -> Option<f32> {
    active_paths()?
        .iter()
        .find(|path| source_name(path).as_deref() == Some(name))
        .and_then(white_level)
}

fn active_paths() -> Option<Vec<DISPLAYCONFIG_PATH_INFO>> {
    let (mut path_count, mut mode_count) = (0, 0);
    // SAFETY: both out-parameters are live.
    let sized = unsafe {
        GetDisplayConfigBufferSizes(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut path_count,
            &raw mut mode_count,
        )
    };
    if sized != ERROR_SUCCESS {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: each array holds as many elements as the count passed with it.
    let queried = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut path_count,
            paths.as_mut_ptr(),
            &raw mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    };
    if queried != ERROR_SUCCESS {
        return None;
    }
    paths.truncate(path_count as usize);
    Some(paths)
}

fn request_size<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("a display configuration request is a few hundred bytes")
}

fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: request_size::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the header starts a live request of the size it states.
    let status = unsafe { DisplayConfigGetDeviceInfo(&raw mut request.header) };
    (status == 0).then(|| utf16_name(&request.viewGdiDeviceName))
}

fn white_level(path: &DISPLAYCONFIG_PATH_INFO) -> Option<f32> {
    let mut request = DISPLAYCONFIG_SDR_WHITE_LEVEL {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            size: request_size::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(),
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        SDRWhiteLevel: 0,
    };
    // SAFETY: the header starts a live request of the size it states.
    let status = unsafe { DisplayConfigGetDeviceInfo(&raw mut request.header) };
    (status == 0 && request.SDRWhiteLevel != 0).then(|| white_level_nits(request.SDRWhiteLevel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pq_bt2020_colour_space_is_hdr() {
        assert!(is_hdr(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020));
        assert!(!is_hdr(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709));
        assert!(!is_hdr(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709));
    }

    #[test]
    fn white_levels_are_thousandths_of_80_nits() {
        assert!((white_level_nits(1000) - 80.0).abs() < 1e-3);
        assert!((white_level_nits(2500) - 200.0).abs() < 1e-3);
        assert!((white_level_nits(3750) - 300.0).abs() < 1e-3);
    }

    #[test]
    fn device_names_stop_at_the_first_nul() {
        let mut units = [0u16; 32];
        for (unit, byte) in units.iter_mut().zip(br"\\.\DISPLAY1") {
            *unit = u16::from(*byte);
        }
        assert_eq!(utf16_name(&units), r"\\.\DISPLAY1");
    }

    #[test]
    #[ignore = "prints this machine's displays"]
    fn print_displays() {
        for display in all().expect("list the displays") {
            println!("{display}");
        }
    }
}
