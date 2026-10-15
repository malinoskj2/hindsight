//! How a monitor is driven: HDR, colour space, bit depth, peak luminance and SDR white level.

use crate::gpu::win32::{cb_size, utf16_until_nul};
use std::{borrow::Cow, fmt};
use windows::{
    Win32::{
        Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
            GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
        },
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
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
/// The number of `QueryDisplayConfig` attempts made while the display topology keeps changing
/// under it.
const QUERY_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Display {
    /// GDI device name, such as `\\.\DISPLAY1`.
    name: String,
    color_space: DXGI_COLOR_SPACE_TYPE,
    bits_per_color: u32,
    pub(super) max_nits: f32,
    pub(super) sdr_white_nits: Option<f32>,
}

impl Display {
    pub(super) fn is_hdr(&self) -> bool {
        is_hdr(self.color_space)
    }
}

/// One log line's details, such as "DISPLAY1 · HDR · 10-bit · 1000 nits · white 80 nits". The
/// SDR white level only matters in HDR, where it sets the tone map's white.
impl fmt::Display for Display {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} · {} · {}-bit · {:.0} nits",
            self.name.strip_prefix(r"\\.\").unwrap_or(&self.name),
            color_space_name(self.color_space),
            self.bits_per_color,
            self.max_nits,
        )?;
        if self.is_hdr() {
            match self.sdr_white_nits {
                Some(nits) => write!(formatter, " · white {nits:.0} nits")?,
                None => write!(formatter, " · white unknown")?,
            }
        }
        Ok(())
    }
}

/// Windows drives a monitor in PQ BT.2020 only while advanced colour is on.
pub(super) fn is_hdr(color_space: DXGI_COLOR_SPACE_TYPE) -> bool {
    color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
}

fn color_space_name(color_space: DXGI_COLOR_SPACE_TYPE) -> Cow<'static, str> {
    match color_space {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 => "SDR".into(),
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 => "HDR".into(),
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 => "scRGB".into(),
        other => format!("colour space {}", other.0).into(),
    }
}

pub(super) fn white_level_nits(level: u32) -> f32 {
    level as f32 * SCRGB_NITS / WHITE_LEVEL_UNIT
}

pub(super) fn describe(monitor: HMONITOR) -> Option<Display> {
    output_of(monitor).map(|description| from_description(&description))
}

/// Returns whether the monitor is in HDR, skipping the white-level look-up. A fresh DXGI factory
/// is created on each call, because an older factory's outputs keep reporting the colour space
/// they had when it was created.
pub(super) fn is_in_hdr(monitor: HMONITOR) -> Option<bool> {
    output_of(monitor).map(|description| is_hdr(description.ColorSpace))
}

fn output_of(monitor: HMONITOR) -> Option<DXGI_OUTPUT_DESC1> {
    outputs()
        .ok()?
        .into_iter()
        .find(|description| description.Monitor == monitor)
}

#[expect(unsafe_code, reason = "calls DXGI to list the outputs")]
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
            // `IDXGIOutput6` needs Windows 10 1803. An output without it counts as SDR.
            if let Ok(output) = output.cast::<IDXGIOutput6>() {
                // SAFETY: reads the description of a live output.
                descriptions.push(unsafe { output.GetDesc1() }?);
            }
        }
    }
    Ok(descriptions)
}

fn from_description(description: &DXGI_OUTPUT_DESC1) -> Display {
    let name = utf16_until_nul(&description.DeviceName);
    Display {
        sdr_white_nits: sdr_white_level(&name),
        name,
        color_space: description.ColorSpace,
        bits_per_color: description.BitsPerColor,
        max_nits: description.MaxLuminance,
    }
}

fn sdr_white_level(name: &str) -> Option<f32> {
    active_paths()?
        .iter()
        .find(|path| source_name(path).as_deref() == Some(name))
        .and_then(white_level)
}

#[expect(unsafe_code, reason = "calls the display configuration API")]
/// Returns the active display paths, asking again a few times when a display is plugged in
/// between sizing and querying.
fn active_paths() -> Option<Vec<DISPLAYCONFIG_PATH_INFO>> {
    for _ in 0..QUERY_ATTEMPTS {
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
        match queried {
            ERROR_SUCCESS => {
                paths.truncate(path_count as usize);
                return Some(paths);
            }
            ERROR_INSUFFICIENT_BUFFER => {}
            _ => return None,
        }
    }
    None
}

#[expect(unsafe_code, reason = "calls DisplayConfigGetDeviceInfo")]
fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: cb_size::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the header starts a live request of the size it states.
    let status = unsafe { DisplayConfigGetDeviceInfo(&raw mut request.header) };
    (status == ERROR_SUCCESS.0.cast_signed()).then(|| utf16_until_nul(&request.viewGdiDeviceName))
}

#[expect(unsafe_code, reason = "calls DisplayConfigGetDeviceInfo")]
fn white_level(path: &DISPLAYCONFIG_PATH_INFO) -> Option<f32> {
    let mut request = DISPLAYCONFIG_SDR_WHITE_LEVEL {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            size: cb_size::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(),
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        SDRWhiteLevel: 0,
    };
    // SAFETY: the header starts a live request of the size it states.
    let status = unsafe { DisplayConfigGetDeviceInfo(&raw mut request.header) };
    (status == ERROR_SUCCESS.0.cast_signed() && request.SDRWhiteLevel != 0)
        .then(|| white_level_nits(request.SDRWhiteLevel))
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
    fn a_display_is_one_short_line() {
        let mut display = Display {
            name: r"\\.\DISPLAY1".to_owned(),
            color_space: DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            bits_per_color: 8,
            max_nits: 270.0,
            sdr_white_nits: Some(80.0),
        };
        assert_eq!(display.to_string(), "DISPLAY1 · SDR · 8-bit · 270 nits");
        display.color_space = DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
        display.bits_per_color = 10;
        display.max_nits = 1000.0;
        assert_eq!(
            display.to_string(),
            "DISPLAY1 · HDR · 10-bit · 1000 nits · white 80 nits"
        );
    }

    #[test]
    #[ignore = "prints this machine's displays"]
    fn print_displays() {
        for description in outputs().expect("list the displays") {
            println!("{}", from_description(&description));
        }
    }
}
