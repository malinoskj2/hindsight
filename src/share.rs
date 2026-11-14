//! Preparing a size-limited clipboard copy while keeping the saved original.

use crate::{log, replay::CLOCK_RATE};
use anyhow::Context;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};
use thiserror::Error;

mod worker;

pub(crate) use worker::Worker;

/// The folder beside a clip that holds the shareable copies of the clips in its own folder.
const FOLDER: &str = "shareable";

/// Where the shareable copy of `clip` is kept: beside it, in [`FOLDER`], under its name.
pub(crate) fn copy_path(clip: &Path) -> Option<PathBuf> {
    let stem = clip.file_stem()?;
    let mut name = stem.to_owned();
    name.push(".mp4");
    Some(clip.parent()?.join(FOLDER).join(name))
}

/// The file to paste for `clip` within `limit` bytes, if there is one already: the clip itself
/// when it fits, or its shareable copy when that was made and fits.
pub(crate) fn existing(clip: &Path, limit: u64) -> Option<PathBuf> {
    let fits =
        |path: &Path| fs::metadata(path).is_ok_and(|file| file.len() > 0 && file.len() <= limit);
    if fits(clip) {
        return Some(clip.to_owned());
    }
    copy_path(clip).filter(|copy| fits(copy))
}

/// Returns the file to paste for `clip` within `limit` bytes, making its shareable copy first
/// when there is none. `progress` counts each attempt's encoding in thousandths.
pub(crate) fn prepare(
    clip: &Path,
    limit: u64,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> anyhow::Result<PathBuf> {
    if let Some(existing) = existing(clip, limit) {
        return Ok(existing);
    }
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), ShareError::Cancelled);
    let copy = copy_path(clip).context("name the shareable copy")?;
    log::info(format!(
        "Share · making a copy of {} within {} MB",
        clip.display(),
        limit / 1_000_000
    ));
    let made = encode(clip, limit, progress, cancel)?;
    if let Err(error) = fs::rename(&made, &copy) {
        let _ = fs::remove_file(&made);
        return Err(anyhow::Error::new(error).context("name the shareable copy"));
    }
    Ok(copy)
}

#[cfg(windows)]
fn encode(
    clip: &Path,
    limit: u64,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> anyhow::Result<PathBuf> {
    crate::gpu::share(clip, limit, progress, cancel).context("make the shareable copy with NVENC")
}

#[cfg(not(windows))]
fn encode(_: &Path, _: u64, _: &AtomicU32, _: &AtomicBool) -> anyhow::Result<PathBuf> {
    anyhow::bail!("NVENC requires Windows")
}

/// The file the user wants to paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Copy {
    Original,
    Shareable,
}

/// Leave room for container indexes and variation in NVENC's variable bitrate output.
const HEADROOM_PERCENT: u64 = 10;
/// Refuse a budget too small even for low-quality video rather than exceed the limit.
const MIN_BITRATE: u64 = 64_000;
const ATTEMPTS: usize = 3;

#[derive(Debug, Error)]
pub(crate) enum ShareError {
    #[error(
        "the share limit is too small for this clip's duration and sound; increase it in Settings"
    )]
    Budget,
    #[error("NVENC could not fit this clip within the share limit; increase it in Settings")]
    TooLarge,
    #[error("making the shareable copy was cancelled")]
    Cancelled,
}

pub(crate) const fn limit_bytes(megabytes: u32) -> u64 {
    megabytes as u64 * 1_000_000
}

/// Video's budget after retaining the first (mixed, when present) AAC track and reserving
/// space for the container. Decimal MB match the limit shown in Settings.
pub(crate) fn bitrate(limit: u64, audio_bytes: u64, duration: i64) -> Result<u32, ShareError> {
    let duration = u64::try_from(duration)
        .ok()
        .filter(|&ticks| ticks > 0)
        .ok_or(ShareError::Budget)?;
    let available = (limit - limit / 100 * HEADROOM_PERCENT)
        .checked_sub(audio_bytes)
        .ok_or(ShareError::Budget)?;
    let bits = u128::from(available) * 8 * u128::from(CLOCK_RATE.unsigned_abs());
    let rate = u64::try_from(bits / u128::from(duration)).unwrap_or(u64::MAX);
    if rate < MIN_BITRATE {
        return Err(ShareError::Budget);
    }
    Ok(u32::try_from(rate).unwrap_or(u32::MAX))
}

/// A bounded retry uses the measured output, with another margin, to lower the video rate.
pub(crate) fn retry_bitrate(rate: u32, actual: u64, limit: u64) -> Result<u32, ShareError> {
    let reduced = u128::from(rate) * u128::from(limit) * 8 / (u128::from(actual.max(1)) * 10);
    let reduced = u32::try_from(reduced)
        .unwrap_or(rate)
        .min(rate.saturating_sub(1));
    if u64::from(reduced) < MIN_BITRATE {
        return Err(ShareError::Budget);
    }
    Ok(reduced)
}

/// Encode, measure the finished container, and retry at a lower rate a bounded number of
/// times. Failed or cancelled outputs are removed; only a verified file is returned.
pub(crate) fn encode_with_limit(
    limit: u64,
    audio_bytes: u64,
    duration: i64,
    cancel: &AtomicBool,
    mut encode: impl FnMut(u32) -> anyhow::Result<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let mut rate = bitrate(limit, audio_bytes, duration)?;
    for attempt in 0..ATTEMPTS {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), ShareError::Cancelled);
        let output = encode(rate)?;
        let measured = fs::metadata(&output).context("check the shareable copy's size");
        if let Ok(metadata) = &measured
            && metadata.len() <= limit
            && !cancel.load(Ordering::Relaxed)
        {
            return Ok(output);
        }
        fs::remove_file(&output).context("remove an unfinished shareable copy")?;
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), ShareError::Cancelled);
        let actual = measured?.len();
        if attempt + 1 < ATTEMPTS {
            rate = retry_bitrate(rate, actual, limit)?;
        }
    }
    Err(ShareError::TooLarge.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_s_shareable_copy_is_named_after_it_in_the_folder_beside_it() {
        assert_eq!(
            copy_path(Path::new("clips/videos/2026-11-12 20-01-02.mkv")),
            Some(PathBuf::from(
                "clips/videos/shareable/2026-11-12 20-01-02.mp4"
            ))
        );
    }

    #[test]
    fn the_file_to_paste_is_the_clip_when_it_fits_and_otherwise_a_copy_that_fits() {
        let directory = tempfile::tempdir().unwrap();
        let clip = directory.path().join("clip.mkv");
        fs::File::create(&clip).unwrap().set_len(2_000).unwrap();
        assert_eq!(existing(&clip, 2_000), Some(clip.clone()));
        assert_eq!(existing(&clip, 1_000), None, "no copy yet");

        let copy = copy_path(&clip).unwrap();
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::File::create(&copy).unwrap().set_len(900).unwrap();
        assert_eq!(existing(&clip, 1_000), Some(copy.clone()));
        assert_eq!(existing(&clip, 800), None, "a copy made for a larger limit");
        assert_eq!(
            prepare(&clip, 1_000, &AtomicU32::new(0), &AtomicBool::new(false)).unwrap(),
            copy,
            "an existing copy is not made again"
        );
    }

    #[test]
    fn the_budget_reserves_sound_and_container_space() {
        let rate = bitrate(limit_bytes(25), 1_000_000, 30 * CLOCK_RATE).unwrap();
        assert_eq!(rate, 5_733_333);
        assert!(bitrate(1_000_000, 900_001, CLOCK_RATE).is_err());
        assert!(bitrate(1_000_000, 0, 0).is_err());
        assert!(bitrate(1_000_000, 0, -1).is_err());
        assert!(bitrate(1_000_000, 0, 3600 * CLOCK_RATE).is_err());
    }

    #[test]
    fn an_oversize_result_lowers_the_rate_with_a_margin() {
        assert_eq!(
            retry_bitrate(8_000_000, 30_000_000, 25_000_000).unwrap(),
            5_333_333
        );
        assert!(retry_bitrate(64_000, 30_000_000, 25_000_000).is_err());
    }

    #[test]
    fn an_oversized_export_is_deleted_and_retried_before_it_is_returned() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("too-large.mp4");
        let second = directory.path().join("fits.mp4");
        let mut rates = Vec::new();
        let output = encode_with_limit(1_000_000, 0, CLOCK_RATE, &AtomicBool::new(false), |rate| {
            rates.push(rate);
            let (path, bytes) = if rates.len() == 1 {
                (&first, 1_100_000)
            } else {
                (&second, 1_000_000)
            };
            fs::File::create(path)?.set_len(bytes)?;
            Ok(path.clone())
        })
        .unwrap();
        assert_eq!(output, second);
        assert!(!first.exists());
        assert!(rates[1] < rates[0]);
    }

    #[test]
    fn oversize_exports_are_bounded_and_never_returned() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.mp4");
        let mut calls = 0;
        let result = encode_with_limit(1_000_000, 0, CLOCK_RATE, &AtomicBool::new(false), |_| {
            calls += 1;
            fs::File::create(&path)?.set_len(1_100_000)?;
            Ok(path.clone())
        });
        assert!(matches!(
            result.unwrap_err().downcast_ref::<ShareError>(),
            Some(ShareError::TooLarge)
        ));
        assert_eq!(calls, ATTEMPTS);
        assert!(!path.exists());
    }

    #[test]
    fn cancellation_removes_a_finished_export_and_an_impossible_budget_never_encodes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cancelled.mp4");
        let cancel = AtomicBool::new(false);
        let result = encode_with_limit(1_000_000, 0, CLOCK_RATE, &cancel, |_| {
            fs::write(&path, b"complete")?;
            cancel.store(true, Ordering::Relaxed);
            Ok(path.clone())
        });
        assert!(matches!(
            result.unwrap_err().downcast_ref::<ShareError>(),
            Some(ShareError::Cancelled)
        ));
        assert!(!path.exists());
        assert!(
            encode_with_limit(
                1_000_000,
                1_000_000,
                CLOCK_RATE,
                &AtomicBool::new(false),
                |_| { panic!("audio already exceeds the budget") }
            )
            .is_err()
        );
    }
}
