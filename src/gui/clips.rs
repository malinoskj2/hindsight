//! The Clips tab: the clip folder as thumbnails, newest first, with the shell's thumbnails made
//! on a thread of their own.

use super::{
    layout::{self, BLACK, FILL_HEIGHT, FILL_Y, GROUP_WIDTH, GROUP_X, LINE},
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

const THUMB_WIDTH: i32 = 128;
const THUMB_HEIGHT: i32 = 72;
const CELL_WIDTH: i32 = 134;
const CELL_HEIGHT: i32 = THUMB_HEIGHT + 2 * LINE + 8;
const COLUMNS: i32 = 3;
const CLIP_PREFIX: &str = "hindsight_";
const CLIP_EXTENSION: &str = "mp4";
const STAMP_LENGTH: usize = 19;
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
                position: gui::dpi(GROUP_X, FILL_Y),
                size: gui::dpi(GROUP_WIDTH, FILL_HEIGHT),
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

    pub(super) fn page(&self) -> &gui::TabPage {
        &self.page
    }

    pub(super) fn list(&self) -> &gui::ListView {
        &self.list
    }

    pub(super) fn on_create(&self) {
        // Image list first: setting it resets the icon spacing.
        self.install_image_list();
        self.set_icon_spacing();
    }

    pub(super) fn apply_look(&self, skin: &Skin) {
        let (background, text) = skin.list_colours().unwrap_or_else(|| {
            (
                w::GetSysColor(co::COLOR::WINDOW),
                w::GetSysColor(co::COLOR::WINDOWTEXT),
            )
        });
        self.set_colours(background, text);
    }

    /// Rereads the folder when the tab comes into view, the folder changes or a clip was saved;
    /// finished thumbnails are taken in either way.
    pub(super) fn update(&self, status: &Status, folder: &Path, shown: bool) {
        self.receive_thumbnails();
        let rebuild = {
            let mut state = self.state.borrow_mut();
            if state.folder.as_deref() != Some(folder) || state.last_clip != status.last_clip {
                state.folder = Some(folder.to_owned());
                state.last_clip = status.last_clip.clone();
                state.stale = true;
            }
            if shown && !state.in_view {
                state.stale = true;
            }
            state.in_view = shown;
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

    fn events(&self) {
        let me = self.clone();
        self.list.on().nm_click(move |click| {
            if let Some(path) = me.clip_path(click.iItem) {
                open(&path);
            }
            Ok(())
        });
        let me = self.clone();
        self.list.on().nm_r_click(move |click| {
            me.on_right_click(click.iItem);
            // Returning 1 suppresses WM_CONTEXTMENU.
            Ok(1)
        });
        self.list.on().lvn_get_empty_markup(|markup| {
            markup.dwFlags = co::EMF::CENTERED;
            markup.set_szMarkup(text::NO_CLIPS);
            Ok(true)
        });
    }

    fn clip_path(&self, index: i32) -> Option<PathBuf> {
        let index = usize::try_from(index).ok()?;
        let state = self.state.borrow();
        state.clips.get(index).map(|clip| clip.path.clone())
    }

    fn on_right_click(&self, index: i32) {
        let Some(path) = self.clip_path(index) else {
            return;
        };
        if let Ok(item) = u32::try_from(index) {
            let _ = self.list.items().get(item).select(true);
        }
        // TrackPopupMenu pumps messages; the timer may rebuild the list meanwhile, hence `path`.
        match self.choose_command() {
            Ok(Some(ClipCommand::Open)) => open(&path),
            Ok(Some(ClipCommand::Reveal)) => {
                if let Err(error) = shell::reveal(&path) {
                    log::warning(text::clip_reveal_failed(&path, error));
                }
            }
            Ok(Some(ClipCommand::Delete)) => self.delete(&path),
            Ok(None) => {}
            Err(error) => log::warning(text::clip_menu_failed(error)),
        }
    }

    fn choose_command(&self) -> w::SysResult<Option<ClipCommand>> {
        let mut menu = w::HMENU::CreatePopupMenu()?;
        let items: Vec<w::MenuItem> = ClipCommand::ALL
            .iter()
            .map(|command| w::MenuItem::Entry {
                cmd_id: command.id(),
                text: command.text(),
            })
            .collect();
        let chosen = menu
            .append_item(&items)
            .and_then(|()| menu.SetMenuDefaultItem(w::IdPos::Id(ClipCommand::Open.id())))
            .and_then(|()| w::GetCursorPos())
            .and_then(|at| {
                menu.TrackPopupMenu(
                    co::TPM::RETURNCMD | co::TPM::NONOTIFY | co::TPM::RIGHTBUTTON,
                    at,
                    self.list.hwnd(),
                )
            });
        let _ = menu.DestroyMenu();
        Ok(chosen?.and_then(ClipCommand::from_id))
    }

    fn delete(&self, path: &Path) {
        if let Err(error) = shell::recycle(path, self.list.hwnd()) {
            log::warning(text::clip_delete_failed(path, error));
            return;
        }
        log::info(text::clip_deleted(path));
        let folder = {
            let mut state = self.state.borrow_mut();
            state.forget(path);
            state.folder.clone()
        };
        if let Some(folder) = folder {
            self.rebuild(list_clips(&folder));
        }
    }

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

    /// Places the items on a centred grid: left to itself the list keeps its slack on the
    /// right, even when given a work area.
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

    /// The thumbnail's offset from an item's position, found by placing item 0 at the origin.
    #[expect(unsafe_code)]
    fn icon_offset(&self) -> w::POINT {
        let hwnd = self.list.hwnd();
        let origin = w::POINT::with(0, 0);
        // SAFETY: LVM_SETITEMPOSITION32 reads the point during the call.
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
        // The rectangle is the padded icon area; the thumbnail is centred in it.
        let (width, height) = thumbnail_size();
        w::POINT::with(
            icon.left + (icon.right - icon.left - width) / 2,
            icon.top + (icon.bottom - icon.top - height) / 2,
        )
    }

    fn receive_thumbnails(&self) {
        let mut state = self.state.borrow_mut();
        let made: Vec<_> = match &state.fetcher {
            Some(fetcher) => fetcher.results.try_iter().collect(),
            None => return,
        };
        for (path, result) in made {
            // Deleted while its thumbnail was being made.
            if !state.requested.contains(&path) {
                continue;
            }
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

    fn add_image(&self, thumbnail: &Thumbnail) -> Result<u32, ThumbnailError> {
        self.list
            .image_list(co::LVSIL::NORMAL)
            .and_then(|images| images.Add(thumbnail.bitmap(), None))
            .map_err(ThumbnailError::ImageList)
    }

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
        // SAFETY: LVM_SETIMAGELIST takes the handle by value; the control owns it from here.
        unsafe {
            self.list.hwnd().SendMessage(w::msg::LvmSetImageList {
                kind: co::LVSIL::NORMAL,
                himagelist: Some(images.leak()),
            });
        }
    }

    #[expect(unsafe_code)]
    fn set_icon_spacing(&self) {
        // SAFETY: LVM_SETICONSPACING takes both lengths packed by value.
        unsafe {
            self.list.hwnd().SendMessage(w::msg::LvmSetIconSpacing {
                size: w::SIZE::with(gui::dpi_x(CELL_WIDTH), gui::dpi_y(CELL_HEIGHT)),
            });
        }
    }

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

    /// Raw `LVM_INSERTITEM`: the control's helper wants a column header, which an icon view lacks.
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

#[derive(Default)]
struct State {
    folder: Option<PathBuf>,
    last_clip: Option<PathBuf>,
    stale: bool,
    in_view: bool,
    clips: Vec<Clip>,
    placeholder: Option<u32>,
    thumbnails: HashMap<PathBuf, u32>,
    requested: HashSet<PathBuf>,
    fetcher: Option<Fetcher>,
}

impl State {
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

    /// The image stays in the image list: removing one renumbers those after it.
    fn forget(&mut self, path: &Path) {
        self.thumbnails.remove(path);
        self.requested.remove(path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipCommand {
    Open,
    Reveal,
    Delete,
}

impl ClipCommand {
    const ALL: [Self; 3] = [Self::Open, Self::Reveal, Self::Delete];

    /// From 1: the menu reports no choice as 0.
    fn id(self) -> u16 {
        match self {
            Self::Open => 1,
            Self::Reveal => 2,
            Self::Delete => 3,
        }
    }

    fn from_id(id: i32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| i32::from(command.id()) == id)
    }

    fn text(self) -> &'static str {
        match self {
            Self::Open => text::CLIP_MENU_OPEN,
            Self::Reveal => text::CLIP_MENU_REVEAL,
            Self::Delete => text::CLIP_MENU_DELETE,
        }
    }
}

/// Dropping this closes the request channel and ends the thread.
struct Fetcher {
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Result<Thumbnail, shell::ThumbnailError>)>,
}

impl Fetcher {
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

#[derive(Debug, PartialEq, Eq)]
struct Clip {
    path: PathBuf,
    name: Option<ClipName>,
    size: u64,
}

impl Clip {
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

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn caption(&self) -> String {
        let title = self
            .name
            .as_ref()
            .map_or_else(|| self.file_name(), ClipName::title);
        text::clip_caption(&title, self.size)
    }

    /// Sort key: the recorder's names before the rest, then by name.
    fn order(&self) -> (Option<&ClipName>, String) {
        (self.name.as_ref(), self.file_name())
    }
}

/// The time in `hindsight_YYYY-MM-DD_HH-MM-SS[-n].mp4`; `-n` counts a second clip in one second.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClipName {
    stamp: String,
    attempt: u32,
}

impl ClipName {
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

    fn title(&self) -> String {
        let (date, time) = self.stamp.split_at(10);
        format!("{date} {}", time[1..].replace('-', ":"))
    }
}

fn is_stamp(stamp: &str) -> bool {
    stamp.len() == STAMP_LENGTH
        && stamp.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 | 13 | 16 => byte == b'-',
            10 => byte == b'_',
            _ => byte.is_ascii_digit(),
        })
}

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

fn open(path: &Path) {
    if let Err(error) = shell::open(path) {
        log::warning(text::clip_open_failed(path, error));
    }
}

fn newest_first(clips: &mut [Clip]) {
    clips.sort_by(|a, b| b.order().cmp(&a.order()));
}

fn thumbnail_size() -> (i32, i32) {
    (gui::dpi_x(THUMB_WIDTH), gui::dpi_y(THUMB_HEIGHT))
}

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

/// The thumbnails' rectangle, centred in the client (less the scroll bar once the rows
/// overflow) and never shorter than the client.
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
    fn the_menu_offers_open_reveal_and_delete_each_under_an_id_of_its_own() {
        for command in ClipCommand::ALL {
            assert_ne!(command.id(), 0, "{command:?} would read as no choice");
            assert_eq!(ClipCommand::from_id(command.id().into()), Some(command));
        }
        assert_eq!(ClipCommand::from_id(0), None);
        assert_eq!(ClipCommand::from_id(4), None);
        assert_eq!(ClipCommand::ALL[0], ClipCommand::Open);
    }

    #[test]
    fn a_deleted_clip_is_forgotten_and_the_others_keep_their_pictures() {
        let gone = PathBuf::from(r"D:\Clips\hindsight_2026-10-12_21-14-05.mp4");
        let kept = PathBuf::from(r"D:\Clips\hindsight_2026-10-12_21-15-00.mp4");
        let mut state = State::default();
        for (path, image) in [(&gone, 1), (&kept, 2)] {
            state.thumbnails.insert(path.clone(), image);
            state.requested.insert(path.clone());
        }
        state.forget(&gone);
        assert!(!state.thumbnails.contains_key(&gone));
        assert!(!state.requested.contains(&gone));
        assert_eq!(state.thumbnails.get(&kept), Some(&2));
        assert!(state.requested.contains(&kept));
    }

    #[test]
    fn items_go_across_then_down_from_the_areas_corner() {
        let area = work_area((426, 360), (134, 116), 6, 17, 4);
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
        let short = work_area(client, cell, 6, 17, 6);
        assert_eq!(
            (short.left, short.top, short.right, short.bottom),
            (15, 15, 411, 360)
        );
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
        assert_eq!(
            (CELL_WIDTH - THUMB_WIDTH, CELL_HEIGHT - THUMB_HEIGHT),
            (6, 2 * LINE + 8)
        );
        assert_eq!((FILL_HEIGHT - 2 * EDGE) / CELL_HEIGHT, 3);
    }
}
