//! The Windows shell's view of a file: the thumbnail Explorer shows for it, opening it with
//! whatever the user made the default for files of its kind, showing it selected in an Explorer
//! window, and sending it to the Recycle Bin.

use crate::sys::thread;
use std::{
    ffi::OsString,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
};
use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HWND, SIZE},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
        },
        System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
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
use winsafe::{self as w, co, guard::DeleteObjectGuard, prelude::*};

/// Bytes in a pixel of a 32-bit bitmap: blue, green, red and alpha.
const PIXEL: usize = 4;
/// Where the alpha byte sits in a pixel.
const ALPHA: usize = 3;
/// What `ShellExecute` returns above when it succeeded; at or below, the value is an error code.
const SHELL_EXECUTE_OK: usize = 32;
/// How a clip goes to the Recycle Bin: undoably, without the "are you sure" question, the
/// progress window or the shell's own error boxes, since the caller logs a failure. The one
/// question kept is the warning that a file too large for the bin would be destroyed outright.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the flags all sit in the low 16 bits, which is all the structure has room for"
)]
const RECYCLE_FLAGS: u16 = (FOF_ALLOWUNDO.0
    | FOF_NOCONFIRMATION.0
    | FOF_SILENT.0
    | FOF_NOERRORUI.0
    | FOF_WANTNUKEWARNING.0) as u16;
/// Share of a placeholder's height its play mark takes.
const MARK_SHARE: i32 = 3;

/// A picture for a clip: a 32-bit bitmap with an alpha channel, which the shell's picture fills
/// as far as its proportions allow and the rest of which is clear.
pub(crate) struct Thumbnail(DeleteObjectGuard<w::HBITMAP>);

// SAFETY: a GDI bitmap belongs to the process, not to the thread that made it; the shell thread
// makes these and the window thread hands them to an image list, which copies them.
unsafe impl Send for Thumbnail {}

impl Thumbnail {
    /// The bitmap, to copy into an image list.
    pub(crate) fn bitmap(&self) -> &w::HBITMAP {
        &self.0
    }
}

/// Why a thumbnail could not be made.
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

/// Why a file could not be opened.
#[derive(Debug, Error)]
#[error("the shell refused to open the file (code {0})")]
pub(crate) struct OpenError(usize);

/// Why Explorer could not be shown a file.
#[derive(Debug, Error)]
#[error("the shell could not start Explorer (code {0})")]
pub(crate) struct RevealError(usize);

/// Why a file could not be sent to the Recycle Bin.
#[derive(Debug, Error)]
pub(crate) enum RecycleError {
    #[error("the path is not a full one, so the file could not be restored")]
    NotAbsolute,
    #[error("the shell could not move the file to the Recycle Bin (code {0:#x})")]
    Refused(i32),
    #[error("the deletion was cancelled")]
    Cancelled,
}

/// A thumbnail for every path sent on `requests`, in the order asked, each answered on
/// `results`; returns once the requests' sender is dropped. Meant for a thread of its own: the
/// first thumbnail of a video has the shell decode a frame of it, which can take a second, and
/// the thread runs as background work so a game is not slowed by it.
pub(crate) fn serve_thumbnails(
    size: (i32, i32),
    requests: &Receiver<PathBuf>,
    results: &Sender<(PathBuf, Result<Thumbnail, ThumbnailError>)>,
) {
    let _ = thread::enter_background_mode();
    let com = Com::start();
    for path in requests {
        let made = match &com {
            Ok(_) => thumbnail(&path, size),
            Err(error) => Err(ThumbnailError::Com(error.clone())),
        };
        if results.send((path, made)).is_err() {
            return;
        }
    }
}

/// The thumbnail Explorer shows for the file at `path`, fitted into a clear frame `size` pixels
/// across. COM must be started on the calling thread.
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

/// A stand-in `size` pixels across, shown for a clip until its thumbnail is made: a flat card
/// of `card` with a play mark of `mark`.
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

/// Opens `path` with whatever the user made the default for files of its kind, as a double
/// click in Explorer does.
pub(crate) fn open(path: &Path) -> Result<(), OpenError> {
    execute(&HSTRING::from(path.as_os_str()), None).map_err(OpenError)
}

/// Opens an Explorer window on the folder of `path` with the file selected in it.
pub(crate) fn reveal(path: &Path) -> Result<(), RevealError> {
    let parameters = HSTRING::from(select_argument(path));
    execute(&HSTRING::from("explorer.exe"), Some(&parameters)).map_err(RevealError)
}

/// Sends the file at `path` to the Recycle Bin, from where the user can restore it. A file too
/// large for the bin is destroyed only after the shell's warning, as a dialog owned by `owner`.
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
    // SAFETY: the structure and the path list it points to outlive the call, the list ends in
    // the two nulls the call looks for, and the owner is a window of this thread or null.
    let code = unsafe { SHFileOperationW(&raw mut operation) };
    if code != 0 {
        return Err(RecycleError::Refused(code));
    }
    if operation.fAnyOperationsAborted.as_bool() {
        return Err(RecycleError::Cancelled);
    }
    Ok(())
}

/// Has the shell do the default thing with `file`, given `parameters` if any, as a double click
/// in Explorer would; the shell's code when it fails.
fn execute(file: &HSTRING, parameters: Option<&HSTRING>) -> Result<(), usize> {
    let verb = HSTRING::from("open");
    let parameters = parameters.map_or(PCWSTR::null(), |parameters| PCWSTR(parameters.as_ptr()));
    // SAFETY: the strings outlive the call, the absent ones are null, and no window owns the
    // launch.
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
    // The return pretends to be a module handle; small values are error codes.
    let code = result.0 as usize;
    if code > SHELL_EXECUTE_OK {
        Ok(())
    } else {
        Err(code)
    }
}

/// What Explorer is told to open a window on the folder of `path` with it selected. The quotes
/// keep a comma or a space in the path from ending it early.
fn select_argument(path: &Path) -> OsString {
    let mut argument = OsString::from("/select,\"");
    argument.push(path);
    argument.push("\"");
    argument
}

/// `path` as the list of files a shell file operation takes: wide characters, each name ended
/// by a null and the list by a second one.
fn path_list(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0, 0]).collect()
}

/// COM, started for the calling thread in a single-threaded apartment, which the shell's
/// thumbnail providers expect; stopped again on drop.
struct Com(());

impl Com {
    fn start() -> windows::core::Result<Self> {
        // SAFETY: a plain call; every success is matched by the uninitialise in `drop`.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok()?;
        Ok(Self(()))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: matches the successful initialise that made `self`, on the same thread.
        unsafe { CoUninitialize() };
    }
}

/// Where a picture lands inside a frame: as large as its proportions allow, centred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Fit {
    /// A picture `picture` pixels across fitted into a frame `frame` pixels across, scaled
    /// either way to touch two of its sides.
    fn of((width, height): (i32, i32), (frame_width, frame_height): (i32, i32)) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        // Wider than the frame, in proportion, fills its width; otherwise its height.
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

    /// The whole of a frame `size` pixels across.
    const fn whole((width, height): (i32, i32)) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// A clear 32-bit bitmap to draw a thumbnail into, with the address of its pixels. A DIB
/// section rather than a plain bitmap, since an image list honours the alpha channel of nothing
/// else.
struct Frame {
    bitmap: DeleteObjectGuard<w::HBITMAP>,
    /// Owned by the bitmap; `width * height * PIXEL` bytes, top row first.
    pixels: *mut u8,
    width: i32,
    height: i32,
}

impl Frame {
    /// A clear frame `size` pixels across, or `None` when Windows cannot make one.
    fn new((width, height): (i32, i32)) -> Option<Self> {
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
        let mut pixels = std::ptr::null_mut();
        // SAFETY: `info` describes a 32-bit bitmap, and `pixels` receives the address of its
        // memory, which Windows owns for as long as the bitmap lives.
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

    /// Runs `draw` on a device context with the frame selected, then waits for GDI to finish
    /// with the pixels so they can be read and written directly.
    fn paint(&mut self, draw: impl FnOnce(&w::HDC) -> w::SysResult<()>) -> w::SysResult<()> {
        let screen = w::HWND::NULL.GetDC()?;
        let target = screen.CreateCompatibleDC()?;
        let selected = target.SelectObject(&*self.bitmap)?;
        let drawn = draw(&target);
        drop(selected);
        drawn?;
        w::GdiFlush()
    }

    /// Makes every pixel inside `fit` opaque. The shell's picture may carry no alpha (a 24-bit
    /// bitmap, or a 32-bit one with the channel left at zero), and an image list would then
    /// show nothing of it; the rest of the frame stays clear.
    fn make_opaque(&mut self, fit: Fit) {
        let Some(length) = usize::try_from(self.width)
            .ok()
            .zip(usize::try_from(self.height).ok())
            .map(|(width, height)| width * height * PIXEL)
        else {
            return;
        };
        // SAFETY: Windows allotted `length` bytes at `pixels` for the bitmap, which `self`
        // keeps alive, and nothing draws on it now that `paint` has flushed GDI.
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

/// The shell's thumbnail for the file at `path`, no larger than `size`.
fn shell_picture(
    path: &Path,
    (width, height): (i32, i32),
) -> Result<DeleteObjectGuard<w::HBITMAP>, ThumbnailError> {
    let name = HSTRING::from(path.as_os_str());
    // SAFETY: the name is a wide string that outlives the call, and COM is started on this
    // thread.
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&name, None) }.map_err(ThumbnailError::Item)?;
    // Only a real thumbnail will do: without `THUMBNAILONLY` the shell hands back the file
    // type's icon when it has none, which would show as a clip's picture.
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

/// Copies `picture`, `size` pixels across, onto `hdc` inside `fit`, averaging the pixels it
/// merges rather than dropping them.
fn copy_fitted(
    hdc: &w::HDC,
    picture: &w::HBITMAP,
    (width, height): (i32, i32),
    fit: Fit,
) -> w::SysResult<()> {
    let screen = w::HWND::NULL.GetDC()?;
    let source = screen.CreateCompatibleDC()?;
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

/// Fills a frame `size` pixels across on `hdc` with `card` and draws a play mark of `mark` in
/// the middle.
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
    // A triangle a third of the frame tall, its point just right of centre.
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

    #[test]
    fn a_picture_fits_its_frame_touching_two_sides_and_centred() {
        // A 16:9 picture fills a 16:9 frame, shrunk or stretched.
        assert_eq!(Fit::of((1280, 720), (128, 72)), Fit::whole((128, 72)));
        assert_eq!(Fit::of((64, 36), (128, 72)), Fit::whole((128, 72)));
        // A square picture takes the height and sits in the middle.
        assert_eq!(
            Fit::of((100, 100), (128, 72)),
            Fit {
                x: 28,
                y: 0,
                width: 72,
                height: 72
            }
        );
        // A strip takes the width and sits in the middle.
        assert_eq!(
            Fit::of((256, 40), (128, 72)),
            Fit {
                x: 0,
                y: 26,
                width: 128,
                height: 20
            }
        );
        // An empty picture counts as a dot, which is square, so nothing divides by zero.
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
        // Refused before the shell is called, so nothing on disk is touched.
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

    /// Run by hand with `cargo test -- --ignored by_hand`: it needs a clip in the default clip
    /// folder and the shell's video thumbnail provider on this machine.
    #[test]
    #[ignore = "needs a clip in %USERPROFILE%\\Videos\\hindsight and the shell's video codecs"]
    fn by_hand_the_shell_gives_a_thumbnail_for_a_real_clip() {
        let _com = Com::start().unwrap();
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
