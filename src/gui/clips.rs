//! The Clips tab: every clip in the clip folder as a thumbnail, newest first, which a click
//! opens in whatever plays videos on this machine. The thumbnails are the ones Explorer shows,
//! made by the shell on a thread of its own since a video's first one takes it a while; a
//! placeholder stands in until each arrives.

use super::{
    layout::{self, BLACK, GROUP_WIDTH, GROUP_X, LINE},
    text,
    theme::{Skin, Surface},
};
use crate::{
    log,
    service::Status,
    sys::shell::{self, Thumbnail, ThumbnailError},
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};
use winsafe::{self as w, co, gui, prelude::*};

/// Where the list starts on the page.
const LIST_Y: i32 = 6;
/// Down to where the Settings tab's last group ends, so the page never asks for more height.
const LIST_HEIGHT: i32 = 364;
/// A thumbnail's size, in logical pixels: 16:9, as the clips are.
const THUMB_WIDTH: i32 = 128;
const THUMB_HEIGHT: i32 = 72;
/// The cell each clip takes: three across the list, with two lines of caption under the
/// thumbnail.
const CELL_WIDTH: i32 = 134;
const CELL_HEIGHT: i32 = THUMB_HEIGHT + 2 * LINE + 8;
/// Clips across the list.
const COLUMNS: i32 = 3;
/// What the names of clips start with and end with, as `replay` names them.
const CLIP_PREFIX: &str = "hindsight_";
const CLIP_EXTENSION: &str = "mp4";
/// Length of the time in a clip's name, `YYYY-MM-DD_HH-MM-SS`.
const STAMP_LENGTH: usize = 19;
/// The placeholder's card and its play mark.
const CARD: w::COLORREF = w::COLORREF::from_rgb(0xE4, 0xE4, 0xE4);
const MARK: w::COLORREF = w::COLORREF::from_rgb(0xA6, 0xA6, 0xA6);

#[derive(Clone)]
pub(super) struct ClipsPage {
    page: gui::TabPage,
    list: gui::ListView,
    state: Rc<RefCell<State>>,
}

impl ClipsPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = gui::TabPage::new(parent, gui::TabPageOpts::default());
        let list = gui::ListView::new(
            &page,
            gui::ListViewOpts {
                position: gui::dpi(GROUP_X, LIST_Y),
                size: gui::dpi(GROUP_WIDTH, LIST_HEIGHT),
                // Rows of large icons from the top, arranged afresh as items come and go.
                control_style: co::LVS::ICON
                    | co::LVS::ALIGNTOP
                    | co::LVS::SINGLESEL
                    | co::LVS::SHOWSELALWAYS,
                control_ex_style: co::LVS_EX::DOUBLEBUFFER,
                ..Default::default()
            },
        );
        layout::draw(&page, page.on(), Vec::new(), skin, Surface::Page, |_| BLACK);

        let me = Self {
            page,
            list,
            state: Rc::default(),
        };
        me.events();
        me
    }

    /// The tab page the list sits on.
    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    /// The list fills the page and grows with it.
    pub(super) fn list(&self) -> &gui::ListView {
        &self.list
    }

    /// Finishes the parts that need the control to exist: the cells' pitch and the image list.
    pub(super) fn on_create(&self) {
        // The image list first: giving the control one resets its icon spacing.
        self.install_image_list();
        self.set_icon_spacing();
    }

    /// Dresses the list in the chosen look: a character theme's panel and text colours, or
    /// Windows' own.
    pub(super) fn apply_look(&self, skin: &Skin) {
        let (background, text) = skin.list_colours().unwrap_or_else(|| {
            (
                w::GetSysColor(co::COLOR::WINDOW),
                w::GetSysColor(co::COLOR::WINDOWTEXT),
            )
        });
        self.set_colours(background, text);
    }

    /// Shows the clips of `folder` while the tab is in view (`shown`), reading the folder again
    /// when it is not the one listed or `status` says a clip was saved since. Thumbnails made
    /// meanwhile are taken in whether the tab is in view or not, so they are there when it is.
    pub(super) fn update(&self, status: &Status, folder: &Path, shown: bool) {
        self.receive_thumbnails();
        let rebuild = {
            let mut state = self.state.borrow_mut();
            if state.folder.as_deref() != Some(folder) || state.last_clip != status.last_clip {
                state.folder = Some(folder.to_owned());
                state.last_clip = status.last_clip.clone();
                state.stale = true;
            }
            let rebuild = shown && state.stale;
            if rebuild {
                state.stale = false;
            }
            rebuild
        };
        if rebuild {
            self.rebuild(list_clips(folder));
        }
    }

    /// Wires a click to opening the clip, and the line the empty list shows.
    fn events(&self) {
        let me = self.clone();
        self.list.on().nm_click(move |click| {
            me.open(click.iItem);
            Ok(())
        });
        self.list.on().lvn_get_empty_markup(|markup| {
            markup.dwFlags = co::EMF::CENTERED;
            markup.set_szMarkup(text::NO_CLIPS);
            Ok(true)
        });
    }

    /// Opens the clip of item `index` with the user's video player; a click beside the items
    /// does nothing.
    fn open(&self, index: i32) {
        let path = usize::try_from(index).ok().and_then(|index| {
            self.state
                .borrow()
                .clips
                .get(index)
                .map(|clip| clip.path.clone())
        });
        if let Some(path) = path
            && let Err(error) = shell::open(&path)
        {
            log::warning(text::clip_open_failed(&path, error));
        }
    }

    /// Lists `clips` afresh, each with the thumbnail already made for it or the placeholder,
    /// and asks for the thumbnails still missing, newest first.
    fn rebuild(&self, clips: Vec<Clip>) {
        let mut state = self.state.borrow_mut();
        self.list.set_redraw(false);
        let _ = self.list.items().delete_all();
        for clip in &clips {
            let image = state
                .thumbnails
                .get(&clip.path)
                .copied()
                .or(state.placeholder);
            if let Err(error) = self.insert_item(&clip.caption(), image) {
                log::warning(text::clip_not_listed(&clip.path, error));
            }
        }
        self.place_items(clips.len());
        self.list.set_redraw(true);
        let _ = self.list.hwnd().InvalidateRect(None, true);
        state.clips = clips;
        state.request_missing_thumbnails();
    }

    /// Puts `count` items on a grid of [`COLUMNS`] set in from the list's edges by the same
    /// amount on each side, so the thumbnails sit as far from the right border as from the
    /// left. Left to arrange them itself, the list starts at its left edge and leaves the
    /// slack on the right, and it keeps that even when told a work area.
    #[expect(unsafe_code)]
    fn place_items(&self, count: usize) {
        let hwnd = self.list.hwnd();
        let Ok(client) = hwnd.GetClientRect() else {
            return;
        };
        let scroll_bar = w::GetSystemMetricsForDpi(co::SM::CXVSCROLL, hwnd.GetDpiForWindow())
            .unwrap_or_else(|_| w::GetSystemMetrics(co::SM::CXVSCROLL));
        let cell = (gui::dpi_x(CELL_WIDTH), gui::dpi_y(CELL_HEIGHT));
        let gutter = cell.0 - gui::dpi_x(THUMB_WIDTH);
        let area = work_area(
            (client.right - client.left, client.bottom - client.top),
            cell,
            gutter,
            scroll_bar,
            count,
        );
        let offset = self.icon_offset();
        for (index, position) in grid_positions(area, cell, offset).take(count).enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            // SAFETY: LVM_SETITEMPOSITION32 reads the point during the call.
            unsafe { hwnd.SendMessage(w::msg::LvmSetItemPosition32 { index, position }) };
        }
    }

    /// Where the list draws an item's thumbnail relative to the position the item is given,
    /// found by putting the first item at the origin and asking where its icon went; the
    /// origin itself when there is no item to ask about.
    #[expect(unsafe_code)]
    fn icon_offset(&self) -> w::POINT {
        let hwnd = self.list.hwnd();
        let origin = w::POINT::with(0, 0);
        // SAFETY: LVM_SETITEMPOSITION32 reads the point during the call; with no item at the
        // index it does nothing.
        unsafe {
            hwnd.SendMessage(w::msg::LvmSetItemPosition32 {
                index: 0,
                position: origin,
            });
        }
        let mut icon = w::RECT::default();
        // SAFETY: LVM_GETITEMRECT writes the rectangle during the call.
        let found = unsafe {
            hwnd.SendMessage(w::msg::LvmGetItemRect {
                index: 0,
                rect: &mut icon,
                portion: co::LVIR::ICON,
            })
        };
        if found.is_err() {
            return origin;
        }
        // The rectangle is the icon's padded area, with the thumbnail centred in it.
        let (width, height) = thumbnail_size();
        w::POINT::with(
            icon.left + (icon.right - icon.left - width) / 2,
            icon.top + (icon.bottom - icon.top - height) / 2,
        )
    }

    /// Takes in the thumbnails the shell thread has finished, showing each on its clip's item.
    fn receive_thumbnails(&self) {
        let mut state = self.state.borrow_mut();
        let made: Vec<_> = match &state.fetcher {
            Some(fetcher) => fetcher.results.try_iter().collect(),
            None => return,
        };
        for (path, result) in made {
            match result.and_then(|thumbnail| self.add_image(&thumbnail)) {
                Ok(image) => {
                    state.thumbnails.insert(path.clone(), image);
                    if let Some(index) = state.clips.iter().position(|clip| clip.path == path)
                        && let Ok(index) = u32::try_from(index)
                    {
                        let _ = self.list.items().get(index).set_icon_index(Some(image));
                    }
                }
                Err(error) => log::warning(text::thumbnail_failed(&path, error)),
            }
        }
    }

    /// Copies `thumbnail` into the list's image list and returns where it sits.
    fn add_image(&self, thumbnail: &Thumbnail) -> Result<u32, ThumbnailError> {
        self.list
            .image_list(co::LVSIL::NORMAL)
            .and_then(|images| images.Add(thumbnail.bitmap(), None))
            .map_err(ThumbnailError::ImageList)
    }

    /// Makes the image list the thumbnails go in, the placeholder first, and gives it to the
    /// list, which owns it from then on.
    #[expect(unsafe_code)]
    fn install_image_list(&self) {
        let size = thumbnail_size();
        let images = w::HIMAGELIST::Create(w::SIZE::with(size.0, size.1), co::ILC::COLOR32, 16, 16);
        let mut images = match images {
            Ok(images) => images,
            Err(error) => {
                log::warning(text::thumbnails_unavailable(error));
                return;
            }
        };
        let placeholder = shell::placeholder(size, CARD, MARK).and_then(|placeholder| {
            images
                .Add(placeholder.bitmap(), None)
                .map_err(ThumbnailError::ImageList)
        });
        match placeholder {
            Ok(index) => self.state.borrow_mut().placeholder = Some(index),
            Err(error) => log::warning(text::thumbnails_unavailable(error)),
        }
        // SAFETY: LVM_SETIMAGELIST takes the handle by value; the control keeps it and destroys
        // it with itself, which is why the guard lets go of it here.
        unsafe {
            self.list.hwnd().SendMessage(w::msg::LvmSetImageList {
                kind: co::LVSIL::NORMAL,
                himagelist: Some(images.leak()),
            });
        }
    }

    /// Sets the pitch of the cells so three fit across the list.
    #[expect(unsafe_code)]
    fn set_icon_spacing(&self) {
        // SAFETY: LVM_SETICONSPACING takes the two lengths packed by value and returns the
        // previous ones.
        unsafe {
            self.list.hwnd().SendMessage(w::msg::LvmSetIconSpacing {
                size: w::SIZE::with(gui::dpi_x(CELL_WIDTH), gui::dpi_y(CELL_HEIGHT)),
            });
        }
    }

    /// Paints the list's background and its captions in the given colours.
    #[expect(unsafe_code)]
    fn set_colours(&self, background: w::COLORREF, text: w::COLORREF) {
        let hwnd = self.list.hwnd();
        // SAFETY: each message takes one colour by value and nothing by pointer.
        unsafe {
            let _ = hwnd.SendMessage(w::msg::LvmSetBkColor {
                color: Some(background),
            });
            let _ = hwnd.SendMessage(w::msg::LvmSetTextBkColor {
                color: Some(background),
            });
            let _ = hwnd.SendMessage(w::msg::LvmSetTextColor { color: Some(text) });
        }
    }

    /// Appends an item captioned `caption` showing image `image` of the image list, or no
    /// image. (The control's own helper wants a column header, which an icon view has not.)
    #[expect(unsafe_code)]
    fn insert_item(&self, caption: &str, image: Option<u32>) -> w::SysResult<u32> {
        let mut item = w::LVITEM::default();
        item.iItem = i32::MAX;
        item.mask = co::LVIF::TEXT | co::LVIF::IMAGE;
        let mut text = w::WString::from_str(caption);
        item.set_pszText(Some(&mut text));
        item.iImage = image.map_or(-1, |index| i32::try_from(index).unwrap_or(-1));
        // SAFETY: LVM_INSERTITEM reads the item and its text, which both outlive the call.
        unsafe {
            self.list
                .hwnd()
                .SendMessage(w::msg::LvmInsertItem { item: &item })
        }
    }
}

/// What the list shows and what is on its way.
#[derive(Default)]
struct State {
    /// The folder listed, once one has been; a different one means reading again.
    folder: Option<PathBuf>,
    /// The last clip saved when the folder was listed; a later one means reading again.
    last_clip: Option<PathBuf>,
    /// The folder must be read again before the list is next in view.
    stale: bool,
    /// The clips as listed, in the order of the items.
    clips: Vec<Clip>,
    /// Where the placeholder sits in the image list, once added.
    placeholder: Option<u32>,
    /// Where each clip's thumbnail sits in the image list, once made; kept across rebuilds.
    thumbnails: HashMap<PathBuf, u32>,
    /// The clips whose thumbnails have been asked for, made or not.
    requested: HashSet<PathBuf>,
    /// The thread that makes thumbnails, started when the first is wanted.
    fetcher: Option<Fetcher>,
}

impl State {
    /// Asks the shell thread for the thumbnails of the listed clips that have neither one nor
    /// a request out, newest first. Nothing is asked when the thread cannot be started.
    fn request_missing_thumbnails(&mut self) {
        let missing: Vec<PathBuf> = self
            .clips
            .iter()
            .map(|clip| clip.path.clone())
            .filter(|path| !self.thumbnails.contains_key(path) && !self.requested.contains(path))
            .collect();
        if missing.is_empty() {
            return;
        }
        if self.fetcher.is_none() {
            self.fetcher = Fetcher::start(thumbnail_size());
        }
        let Some(fetcher) = &self.fetcher else {
            return;
        };
        for path in missing {
            if fetcher.requests.send(path.clone()).is_ok() {
                self.requested.insert(path);
            }
        }
    }
}

/// The thread that makes thumbnails, and the channels to and from it. The thread ends when
/// this is dropped, after the request it is on.
struct Fetcher {
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Result<Thumbnail, shell::ThumbnailError>)>,
}

impl Fetcher {
    /// Starts the thread, making thumbnails `size` physical pixels across; `None`, logged,
    /// when no thread can be started, which leaves the placeholders standing.
    fn start(size: (i32, i32)) -> Option<Self> {
        let (requests, requests_in) = mpsc::channel();
        let (results_out, results) = mpsc::channel();
        thread::Builder::new()
            .name("thumbnails".to_owned())
            .spawn(move || shell::serve_thumbnails(size, &requests_in, &results_out))
            .inspect_err(|error| log::warning(text::thumbnails_unavailable(error)))
            .ok()?;
        Some(Self { requests, results })
    }
}

/// A clip in the folder, as the list shows it.
#[derive(Debug, PartialEq, Eq)]
struct Clip {
    path: PathBuf,
    /// The time in its name, when the name is one the recorder gives.
    name: Option<ClipName>,
    /// In bytes.
    size: u64,
}

impl Clip {
    /// The clip at `path` of `size` bytes, if it is a video file.
    fn new(path: PathBuf, size: u64) -> Option<Self> {
        let file_name = path.file_name()?.to_str()?;
        if !file_name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case(CLIP_EXTENSION))
        {
            return None;
        }
        let name = ClipName::parse(file_name);
        Some(Self { path, name, size })
    }

    /// The file's name, for a clip that was named by hand.
    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The caption under the thumbnail: the time taken, or else the file's name, and the size.
    fn caption(&self) -> String {
        let title = self
            .name
            .as_ref()
            .map_or_else(|| self.file_name(), ClipName::title);
        text::clip_caption(&title, self.size)
    }

    /// What newest-first sorts by: clips named by the recorder before the rest, later times
    /// and later attempts at the same time first, the rest by name.
    fn order(&self) -> (Option<&ClipName>, String) {
        (self.name.as_ref(), self.file_name())
    }
}

/// The time in a clip's name, `hindsight_YYYY-MM-DD_HH-MM-SS[-n].mp4` as `replay` names them,
/// where `-n` counts a second clip saved in the same second.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClipName {
    /// `YYYY-MM-DD_HH-MM-SS`, which sorts as time does.
    stamp: String,
    /// 1 for the first clip of its second.
    attempt: u32,
}

impl ClipName {
    /// The time in `file_name`, or `None` for a name the recorder did not give.
    fn parse(file_name: &str) -> Option<Self> {
        let rest = file_name
            .strip_prefix(CLIP_PREFIX)?
            .strip_suffix(CLIP_EXTENSION)?
            .strip_suffix('.')?;
        let (stamp, attempt) = rest.split_at_checked(STAMP_LENGTH)?;
        if !is_stamp(stamp) {
            return None;
        }
        let attempt = if attempt.is_empty() {
            1
        } else {
            attempt.strip_prefix('-')?.parse().ok()?
        };
        Some(Self {
            stamp: stamp.to_owned(),
            attempt,
        })
    }

    /// The time as the caption shows it: `YYYY-MM-DD HH:MM:SS`.
    fn title(&self) -> String {
        let (date, time) = self.stamp.split_at(10);
        format!("{date} {}", time[1..].replace('-', ":"))
    }
}

/// Whether `stamp` is `YYYY-MM-DD_HH-MM-SS` in digits.
fn is_stamp(stamp: &str) -> bool {
    stamp.len() == STAMP_LENGTH
        && stamp.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 | 13 | 16 => byte == b'-',
            10 => byte == b'_',
            _ => byte.is_ascii_digit(),
        })
}

/// The clips in `folder`, newest first; none when it cannot be read.
fn list_clips(folder: &Path) -> Vec<Clip> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut clips: Vec<Clip> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_file()
                .then(|| Clip::new(entry.path(), metadata.len()))
                .flatten()
        })
        .collect();
    newest_first(&mut clips);
    clips
}

/// Sorts `clips` newest first.
fn newest_first(clips: &mut [Clip]) {
    clips.sort_by(|a, b| b.order().cmp(&a.order()));
}

/// A thumbnail's size in physical pixels.
fn thumbnail_size() -> (i32, i32) {
    (gui::dpi_x(THUMB_WIDTH), gui::dpi_y(THUMB_HEIGHT))
}

/// The position to give each item of a grid, in order, so that its thumbnail lands on the
/// grid that starts at `area`'s top-left corner in cells of `cell`: the list draws the
/// thumbnail `offset` from the position it is given.
fn grid_positions(
    area: w::RECT,
    (cell_width, cell_height): (i32, i32),
    offset: w::POINT,
) -> impl Iterator<Item = w::POINT> {
    (0..).map(move |index: i32| {
        w::POINT::with(
            area.left + (index % COLUMNS) * cell_width - offset.x,
            area.top + (index / COLUMNS) * cell_height - offset.y,
        )
    })
}

/// The rectangle, in the list's client coordinates, that the thumbnails of `count` clips
/// span: [`COLUMNS`] cells of `cell` less the `gutter` beyond the last thumbnail, centred in
/// the client's `width` (less the scroll bar once the rows overflow its `height`), with the
/// same margin above and below as beside. It covers the client at least, so a short list
/// scrolls nowhere.
fn work_area(
    (width, height): (i32, i32),
    (cell_width, cell_height): (i32, i32),
    gutter: i32,
    scroll_bar: i32,
    count: usize,
) -> w::RECT {
    let rows = i32::try_from(count.div_ceil(COLUMNS.unsigned_abs() as usize)).unwrap_or(i32::MAX);
    let span = COLUMNS * cell_width - gutter;
    let margin = |width: i32| ((width - span) / 2).max(0);
    let rows_height = rows.saturating_mul(cell_height);
    let overflow = rows_height.saturating_add(2 * margin(width)) > height;
    let margin = margin(if overflow { width - scroll_bar } else { width });
    w::RECT {
        left: margin,
        top: margin,
        right: margin + span,
        bottom: rows_height.saturating_add(2 * margin).max(height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list's sunken edge, either side, and its vertical scroll bar, in logical pixels, as
    /// Windows draws them; the cells must fit inside what they leave.
    const EDGE: i32 = 2;
    const SCROLL_BAR: i32 = 17;

    fn clip(name: &str) -> Clip {
        Clip::new(PathBuf::from(r"D:\Clips").join(name), 0).unwrap()
    }

    #[test]
    fn a_clips_name_gives_the_time_it_was_taken_and_its_attempt() {
        let name = ClipName::parse("hindsight_2026-10-12_21-14-05.mp4").unwrap();
        assert_eq!(name.title(), "2026-10-12 21:14:05");
        assert_eq!(name.attempt, 1);
        let again = ClipName::parse("hindsight_2026-10-12_21-14-05-2.mp4").unwrap();
        assert_eq!(again.stamp, name.stamp);
        assert_eq!(again.attempt, 2);
        for other in [
            "hindsight_2026-10-12_21-14-05.mp4.partial",
            "hindsight_2026-10-12_21-14.mp4",
            "hindsight_2026-10-12_21-14-05-x.mp4",
            "hindsight_2026-10-12-21-14-05.mp4",
            "boss fight.mp4",
            "hindsight_2026-10-12_21-14-05.mkv",
        ] {
            assert_eq!(ClipName::parse(other), None, "{other}");
        }
    }

    #[test]
    fn only_video_files_are_clips_and_a_hand_named_one_is_captioned_by_its_name() {
        assert!(Clip::new(PathBuf::from(r"D:\Clips\notes.txt"), 1).is_none());
        assert!(
            Clip::new(
                PathBuf::from(r"D:\Clips\hindsight_2026-10-12_21-14-05.partial"),
                1
            )
            .is_none()
        );
        let named = Clip::new(PathBuf::from(r"D:\Clips\Boss fight.MP4"), 1_572_864).unwrap();
        assert_eq!(named.name, None);
        assert_eq!(named.caption(), "Boss fight.MP4 2\u{a0}MB");
        let recorded = clip("hindsight_2026-10-12_21-14-05.mp4");
        assert_eq!(recorded.caption(), "2026-10-12 21:14:05 0\u{a0}MB");
    }

    #[test]
    fn clips_are_listed_newest_first_with_the_recorders_names_before_the_rest() {
        let mut clips = vec![
            clip("zebra.mp4"),
            clip("hindsight_2026-10-12_21-14-05.mp4"),
            clip("alpha.mp4"),
            clip("hindsight_2026-10-12_21-14-05-2.mp4"),
            clip("hindsight_2026-10-11_23-59-59.mp4"),
            clip("hindsight_2026-10-12_09-00-00.mp4"),
        ];
        newest_first(&mut clips);
        let names: Vec<String> = clips.iter().map(Clip::file_name).collect();
        assert_eq!(
            names,
            [
                "hindsight_2026-10-12_21-14-05-2.mp4",
                "hindsight_2026-10-12_21-14-05.mp4",
                "hindsight_2026-10-12_09-00-00.mp4",
                "hindsight_2026-10-11_23-59-59.mp4",
                "zebra.mp4",
                "alpha.mp4",
            ]
        );
    }

    #[test]
    fn items_go_across_then_down_from_the_areas_corner() {
        let area = work_area((426, 360), (134, 116), 6, 17, 4);
        // The list draws a thumbnail one pixel in from the item's position.
        let positions: Vec<_> = grid_positions(area, (134, 116), w::POINT::with(1, 1))
            .take(4)
            .map(|point| (point.x, point.y))
            .collect();
        assert_eq!(positions, [(14, 14), (148, 14), (282, 14), (14, 130)]);
    }

    #[test]
    fn the_grid_is_centred_and_makes_way_for_the_scroll_bar_only_when_the_rows_overflow() {
        let client = (426, 360);
        let cell = (134, 116);
        // Two rows of three thumbnails spanning 396: 30 pixels of slack shared out, and the
        // client is not overflowed.
        let short = work_area(client, cell, 6, 17, 6);
        assert_eq!(
            (short.left, short.top, short.right, short.bottom),
            (15, 15, 411, 360)
        );
        // Four rows overflow: the scroll bar takes its width from the slack first.
        let long = work_area(client, cell, 6, 17, 12);
        assert_eq!(
            (long.left, long.top, long.right, long.bottom),
            (6, 6, 402, 476)
        );
        let empty = work_area(client, cell, 6, 17, 0);
        assert_eq!((empty.left, empty.bottom), (15, 360));
    }

    #[test]
    fn three_cells_fit_across_the_list_and_three_rows_down_it() {
        let inside = GROUP_WIDTH - 2 * EDGE - SCROLL_BAR;
        assert!(
            3 * CELL_WIDTH <= inside,
            "{CELL_WIDTH} by three in {inside}"
        );
        assert!(4 * CELL_WIDTH > inside - SCROLL_BAR, "never four across");
        // Three pixels either side of a thumbnail, and two lines of caption below it.
        assert_eq!(
            (CELL_WIDTH - THUMB_WIDTH, CELL_HEIGHT - THUMB_HEIGHT),
            (6, 2 * LINE + 8)
        );
        assert_eq!((LIST_HEIGHT - 2 * EDGE) / CELL_HEIGHT, 3);
        // Level with the bottom of the Settings tab's last group, so the page needs no more.
        assert_eq!(LIST_Y + LIST_HEIGHT, 370);
    }
}
