//! The shell's view of a file: its thumbnail, opening it, revealing it in Explorer, recycling it.

use std::{ffi::OsString, os::windows::ffi::OsStrExt, path::Path};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HWND, SIZE},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
        },
        UI::{
            Shell::{
                FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
                FOF_WANTNUKEWARNING, IShellItemImageFactory, SHCreateItemFromParsingName,
                SHFILEOPSTRUCTW, SHFileOperationW, SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
                ShellExecuteW,
            },
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    },
    core::{HSTRING, PCWSTR},
};
use winsafe::{
    self as w, co,
    guard::{DeleteDCGuard, DeleteObjectGuard},
    prelude::*,
};

const PIXEL: usize = 4;
const ALPHA: usize = 3;
/// `ShellExecute` returns an error code at or below this.
const SHELL_EXECUTE_OK: usize = 32;
#[expect(
    clippy::cast_possible_truncation,
    reason = "the flags all sit in the low 16 bits, which is all the structure has room for"
)]
const RECYCLE_FLAGS: u16 = (FOF_ALLOWUNDO.0
    | FOF_NOCONFIRMATION.0
    | FOF_SILENT.0
    | FOF_NOERRORUI.0
    | FOF_WANTNUKEWARNING.0) as u16;
const MARK_SHARE: i32 = 3;

pub(crate) struct Thumbnail(DeleteObjectGuard<w::HBITMAP>);

// SAFETY: a GDI bitmap belongs to the process, not to the thread that made it.
unsafe impl Send for Thumbnail {}

impl Thumbnail {
    pub(crate) fn bitmap(&self) -> &w::HBITMAP {
        &self.0
    }
}

#[derive(Debug, Error)]
pub(crate) enum ThumbnailError {
    #[error("COM could not be started on the thumbnail thread: {0}")]
    Com(windows::core::Error),
    #[error("the shell could not open the file: {0}")]
    Item(windows::core::Error),
    #[error("the shell has no picture for the file: {0}")]
    Picture(windows::core::Error),
    #[error("no bitmap could be made for the frame")]
    Frame,
    #[error("the picture could not be drawn into the frame: {0}")]
    Draw(co::ERROR),
    #[error("the picture could not be added to the image list: {0}")]
    ImageList(co::HRESULT),
}

#[derive(Debug, Error)]
#[error("the shell refused to open the file (code {0})")]
pub(crate) struct OpenError(usize);

#[derive(Debug, Error)]
#[error("the shell could not start Explorer (code {0})")]
pub(crate) struct RevealError(usize);

#[derive(Debug, Error)]
pub(crate) enum RecycleError {
    #[error("the path is not a full one, so the file could not be restored")]
    NotAbsolute,
    #[error("the shell could not move the file to the Recycle Bin (code {0:#x})")]
    Refused(i32),
    #[error("the deletion was cancelled")]
    Cancelled,
}

/// COM must be started on the calling thread; the shell may take a second per frame.
pub(crate) fn thumbnail(path: &Path, size: (i32, i32)) -> Result<Thumbnail, ThumbnailError> {
    let picture = shell_picture(path, size)?;
    let measured = picture.GetObject().map_err(ThumbnailError::Draw)?;
    let picture_size = (measured.bmWidth, measured.bmHeight);
    let fit = Fit::of(picture_size, size);
    let mut frame = Frame::new(size).ok_or(ThumbnailError::Frame)?;
    frame
        .paint(|hdc| copy_fitted(hdc, &picture, picture_size, fit))
        .map_err(ThumbnailError::Draw)?;
    frame.make_opaque(fit);
    Ok(frame.into_thumbnail())
}

pub(crate) fn placeholder(
    size: (i32, i32),
    card: w::COLORREF,
    mark: w::COLORREF,
) -> Result<Thumbnail, ThumbnailError> {
    let mut frame = Frame::new(size).ok_or(ThumbnailError::Frame)?;
    frame
        .paint(|hdc| draw_placeholder(hdc, size, card, mark))
        .map_err(ThumbnailError::Draw)?;
    frame.make_opaque(Fit::whole(size));
    Ok(frame.into_thumbnail())
}

pub(crate) fn open(path: &Path) -> Result<(), OpenError> {
    execute(&HSTRING::from(path.as_os_str()), None).map_err(OpenError)
}

pub(crate) fn reveal(path: &Path) -> Result<(), RevealError> {
    let parameters = HSTRING::from(select_argument(path));
    execute(&HSTRING::from("explorer.exe"), Some(&parameters)).map_err(RevealError)
}

pub(crate) fn recycle(path: &Path, owner: &w::HWND) -> Result<(), RecycleError> {
    // Without a full path the shell deletes for good, as it cannot tell where to restore to.
    if !path.is_absolute() {
        return Err(RecycleError::NotAbsolute);
    }
    let from = path_list(path);
    let mut operation = SHFILEOPSTRUCTW {
        hwnd: HWND(owner.ptr()),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: RECYCLE_FLAGS,
        ..Default::default()
    };
    // SAFETY: the structure and the double-null-ended path list outlive the call.
    let code = unsafe { SHFileOperationW(&raw mut operation) };
    if code != 0 {
        return Err(RecycleError::Refused(code));
    }
    if operation.fAnyOperationsAborted.as_bool() {
        return Err(RecycleError::Cancelled);
    }
    Ok(())
}

fn execute(file: &HSTRING, parameters: Option<&HSTRING>) -> Result<(), usize> {
    let verb = HSTRING::from("open");
    let parameters = parameters.map_or(PCWSTR::null(), |parameters| PCWSTR(parameters.as_ptr()));
    // SAFETY: the strings outlive the call; absent ones are null.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            parameters,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0.addr();
    if code > SHELL_EXECUTE_OK {
        Ok(())
    } else {
        Err(code)
    }
}

/// Quoted, so a comma or a space in the path does not end it early.
fn select_argument(path: &Path) -> OsString {
    let mut argument = OsString::from("/select,\"");
    argument.push(path);
    argument.push("\"");
    argument
}

fn path_list(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0, 0]).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Fit {
    fn of((width, height): (i32, i32), (frame_width, frame_height): (i32, i32)) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let (fitted_width, fitted_height) = if width * frame_height >= height * frame_width {
            (frame_width, (height * frame_width / width).max(1))
        } else {
            ((width * frame_height / height).max(1), frame_height)
        };
        Self {
            x: (frame_width - fitted_width) / 2,
            y: (frame_height - fitted_height) / 2,
            width: fitted_width,
            height: fitted_height,
        }
    }

    const fn whole((width, height): (i32, i32)) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// A DIB section, not a plain bitmap: an image list honours the alpha channel of nothing else.
struct Frame {
    bitmap: DeleteObjectGuard<w::HBITMAP>,
    /// Owned by the bitmap; `width * height * PIXEL` bytes, top row first.
    pixels: *mut u8,
    width: i32,
    height: i32,
}

impl Frame {
    fn new((width, height): (i32, i32)) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).ok()?,
                biWidth: width,
                // Negative: rows run top to bottom.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        // SAFETY: `info` describes a valid 32-bit bitmap; `pixels` is a live out-parameter.
        let bitmap = unsafe {
            CreateDIBSection(
                None,
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut pixels,
                None,
                0,
            )
        }
        .ok()?;
        if pixels.is_null() {
            // SAFETY: the handle was just created and nothing else refers to it.
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return None;
        }
        Some(Self {
            // SAFETY: the guard takes over the new bitmap handle, which nothing else frees.
            bitmap: unsafe { DeleteObjectGuard::new(w::HBITMAP::from_ptr(bitmap.0)) },
            pixels: pixels.cast::<u8>(),
            width,
            height,
        })
    }

    fn paint(&mut self, draw: impl FnOnce(&w::HDC) -> w::SysResult<()>) -> w::SysResult<()> {
        let target = memory_dc()?;
        let selected = target.SelectObject(&*self.bitmap)?;
        let drawn = draw(&target);
        drop(selected);
        drawn?;
        w::GdiFlush()
    }

    /// The shell's picture may carry no alpha, and an image list would then show nothing of it.
    fn make_opaque(&mut self, fit: Fit) {
        let Some(length) = usize::try_from(self.width)
            .ok()
            .zip(usize::try_from(self.height).ok())
            .map(|(width, height)| width * height * PIXEL)
        else {
            return;
        };
        // SAFETY: Windows allotted `length` bytes at `pixels`, and `paint` has flushed GDI.
        let pixels = unsafe { std::slice::from_raw_parts_mut(self.pixels, length) };
        for row in fit.y..fit.y + fit.height {
            for column in fit.x..fit.x + fit.width {
                let Ok(index) = usize::try_from(row * self.width + column) else {
                    continue;
                };
                if let Some(alpha) = pixels.get_mut(index * PIXEL + ALPHA) {
                    *alpha = u8::MAX;
                }
            }
        }
    }

    fn into_thumbnail(self) -> Thumbnail {
        Thumbnail(self.bitmap)
    }
}

fn shell_picture(
    path: &Path,
    (width, height): (i32, i32),
) -> Result<DeleteObjectGuard<w::HBITMAP>, ThumbnailError> {
    let name = HSTRING::from(path.as_os_str());
    // SAFETY: the wide string outlives the call, and COM is started on this thread.
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&name, None) }.map_err(ThumbnailError::Item)?;
    // Without `THUMBNAILONLY` the shell hands back the file type's icon instead.
    let flags = SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT;
    let size = SIZE {
        cx: width,
        cy: height,
    };
    // SAFETY: a plain call on a live interface; the bitmap it returns is ours to delete.
    let picture = unsafe { factory.GetImage(size, flags) }.map_err(ThumbnailError::Picture)?;
    // SAFETY: the guard takes over the handle the shell gave, which nothing else frees.
    Ok(unsafe { DeleteObjectGuard::new(w::HBITMAP::from_ptr(picture.0)) })
}

fn copy_fitted(
    hdc: &w::HDC,
    picture: &w::HBITMAP,
    (width, height): (i32, i32),
    fit: Fit,
) -> w::SysResult<()> {
    let source = memory_dc()?;
    let _selected = source.SelectObject(picture)?;
    hdc.SetStretchBltMode(co::STRETCH_MODE::HALFTONE)?;
    hdc.StretchBlt(
        w::POINT::with(fit.x, fit.y),
        w::SIZE::with(fit.width, fit.height),
        &source,
        w::POINT::default(),
        w::SIZE::with(width, height),
        co::ROP::SRCCOPY,
    )
}

/// A device context in memory that matches the screen's pixel format.
fn memory_dc() -> w::SysResult<DeleteDCGuard> {
    w::HWND::NULL.GetDC()?.CreateCompatibleDC()
}

fn draw_placeholder(
    hdc: &w::HDC,
    (width, height): (i32, i32),
    card: w::COLORREF,
    mark: w::COLORREF,
) -> w::SysResult<()> {
    let card = w::HBRUSH::CreateSolidBrush(card)?;
    hdc.FillRect(
        w::RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        &card,
    )?;
    let brush = w::HBRUSH::CreateSolidBrush(mark)?;
    let pen = w::HPEN::CreatePen(co::PS::SOLID, 1, mark)?;
    let _brush = hdc.SelectObject(&*brush)?;
    let _pen = hdc.SelectObject(&*pen)?;
    let side = height / MARK_SHARE;
    let left = width / 2 - side * 2 / 5;
    let top = (height - side) / 2;
    hdc.Polygon(&[
        w::POINT::with(left, top),
        w::POINT::with(left, top + side),
        w::POINT::with(left + side / 2, height / 2),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::com::Apartment;

    #[test]
    fn a_picture_fits_its_frame_touching_two_sides_and_centred() {
        assert_eq!(Fit::of((1280, 720), (128, 72)), Fit::whole((128, 72)));
        assert_eq!(Fit::of((64, 36), (128, 72)), Fit::whole((128, 72)));
        assert_eq!(
            Fit::of((100, 100), (128, 72)),
            Fit {
                x: 28,
                y: 0,
                width: 72,
                height: 72
            }
        );
        assert_eq!(
            Fit::of((256, 40), (128, 72)),
            Fit {
                x: 0,
                y: 26,
                width: 128,
                height: 20
            }
        );
        assert_eq!(Fit::of((0, 0), (128, 72)), Fit::of((1, 1), (128, 72)));
    }

    #[test]
    fn explorer_is_asked_to_select_the_whole_path_however_it_is_spelled() {
        assert_eq!(
            select_argument(Path::new(r"D:\My Clips, 2026\boss fight.mp4")),
            r#"/select,"D:\My Clips, 2026\boss fight.mp4""#
        );
    }

    #[test]
    fn a_file_operation_gets_its_path_ended_by_two_nulls() {
        let list = path_list(Path::new(r"D:\a.mp4"));
        assert_eq!(
            String::from_utf16_lossy(&list[..list.len() - 2]),
            r"D:\a.mp4"
        );
        assert_eq!(list[list.len() - 2..], [0, 0]);
    }

    #[test]
    fn only_a_full_path_goes_to_the_recycle_bin() {
        assert!(matches!(
            recycle(
                Path::new("hindsight_2026-10-12_21-14-05.mp4"),
                &w::HWND::NULL
            ),
            Err(RecycleError::NotAbsolute)
        ));
    }

    #[test]
    fn a_placeholder_is_an_opaque_bitmap_of_the_size_asked() {
        let gray = w::COLORREF::from_rgb(0xE4, 0xE4, 0xE4);
        let made = placeholder((128, 72), gray, gray).unwrap();
        let bitmap = made.bitmap().GetObject().unwrap();
        assert_eq!(
            (bitmap.bmWidth, bitmap.bmHeight, bitmap.bmBitsPixel),
            (128, 72, 32)
        );
    }

    #[test]
    #[ignore = "needs a clip in %USERPROFILE%\\Videos\\hindsight and the shell's video codecs"]
    fn by_hand_the_shell_gives_a_thumbnail_for_a_real_clip() {
        let _com = Apartment::single_threaded().unwrap();
        let folder = crate::config::default_clip_directory();
        let clip = std::fs::read_dir(&folder)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "mp4"))
            .expect("a clip in the default clip folder");
        let made = thumbnail(&clip, (128, 72)).unwrap();
        let bitmap = made.bitmap().GetObject().unwrap();
        assert_eq!(
            (bitmap.bmWidth, bitmap.bmHeight, bitmap.bmBitsPixel),
            (128, 72, 32)
        );
    }
}
