//! The Clips tab: the clip folder shown as thumbnails, newest first. The folder is read and the
//! shell's thumbnails are made on a dedicated thread.

use super::{
    layout::{self, FILL_HEIGHT, FILL_Y, GROUP_WIDTH, GROUP_X, LINE, saturating_i32},
    open,
    pane::{self, Pane},
    text,
    theme::Skin,
};
use crate::{
    log,
    replay::ClipName,
    service::Status,
    sys::{
        com::Apartment,
        shell::{self, Thumbnail, ThumbnailError},
        thread as sys_thread,
    },
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};
use winsafe::{self as w, co, gui, prelude::*};

const THUMB_WIDTH: i32 = 128;
const THUMB_HEIGHT: i32 = 72;
const CELL_WIDTH: i32 = 134;
const CELL_HEIGHT: i32 = THUMB_HEIGHT + 2 * LINE + 8;
const COLUMNS: usize = 3;

const CARD: w::COLORREF = w::COLORREF::from_rgb(0xE4, 0xE4, 0xE4);
const MARK: w::COLORREF = w::COLORREF::from_rgb(0xA6, 0xA6, 0xA6);
/// How long a refresh waits for the folder listing before leaving it to the next tick. A local
/// folder answers well within this time, and a slow network folder no longer freezes the window.
const LISTING_WAIT: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub(super) struct ClipsPage(Rc<Inner>);

struct Inner {
    page: gui::TabPage,
    list: gui::ListView,
    state: RefCell<State>,
}

impl Pane for ClipsPage {
    fn title(&self) -> &'static str {
        text::TAB_CLIPS
    }

    fn page(&self) -> &gui::TabPage {
        &self.0.page
    }

    fn on_create(&self) {
        // The image list is set first, because setting it resets the icon spacing.
        self.install_image_list();
        self.set_icon_spacing();
    }

    fn grow(&self, dy: i32) {
        layout::resize_height_by(self.0.list.hwnd(), dy);
    }
}

impl ClipsPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = layout::page(parent);
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
        pane::paint(&page, skin, ());

        let page = Self(Rc::new(Inner {
            page,
            list,
            state: RefCell::default(),
        }));
        page.events();
        page
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

    /// Rereads the folder when the tab comes into view, the folder changes or a clip was saved.
    /// Finished thumbnails are collected in either case.
    pub(super) fn update(&self, status: &Status, folder: &Path, shown: bool) {
        self.receive(None);
        let rebuild = {
            let mut state = self.0.state.borrow_mut();
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
            self.request_listing(folder);
        }
    }

    fn events(&self) {
        let page = self.clone();
        self.0.list.on().nm_click(move |click| {
            if let Some(path) = page.clip_path(click.iItem) {
                open(&path);
            }
            Ok(())
        });
        let page = self.clone();
        self.0.list.on().nm_r_click(move |click| {
            page.on_right_click(click.iItem);
            // Returning 1 suppresses WM_CONTEXTMENU.
            Ok(1)
        });
        let page = self.clone();
        self.0.list.on().lvn_get_empty_markup(move |markup| {
            // The "no clips" text is not shown while the folder is still being read.
            if page.0.state.borrow().listings_out > 0 {
                return Ok(false);
            }
            markup.dwFlags = co::EMF::CENTERED;
            markup.set_szMarkup(text::NO_CLIPS);
            Ok(true)
        });
    }

    fn clip_path(&self, index: i32) -> Option<PathBuf> {
        let index = usize::try_from(index).ok()?;
        let state = self.0.state.borrow();
        state.clips.get(index).map(|clip| clip.path.clone())
    }

    fn on_right_click(&self, index: i32) {
        let Some(path) = self.clip_path(index) else {
            return;
        };
        if let Ok(item) = u32::try_from(index) {
            let _ = self.0.list.items().get(item).select(true);
        }
        // TrackPopupMenu pumps messages, so the timer may rebuild the list in the meantime, which
        // is why `path` was resolved first.
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
                    self.0.list.hwnd(),
                )
            });
        let _ = menu.DestroyMenu();
        Ok(chosen?.and_then(ClipCommand::from_id))
    }

    fn delete(&self, path: &Path) {
        if let Err(error) = shell::recycle(path, self.0.list.hwnd()) {
            log::warning(text::clip_delete_failed(path, error));
            return;
        }
        log::info(text::clip_deleted(path));
        let folder = {
            let mut state = self.0.state.borrow_mut();
            state.thumbnails.forget(path);
            state.folder.clone()
        };
        if let Some(folder) = folder {
            self.request_listing(&folder);
        }
    }

    /// Reads the folder on the worker and waits briefly for the result. The folder is read on the
    /// UI thread only when the worker cannot start.
    fn request_listing(&self, folder: &Path) {
        let sent = {
            let mut state = self.0.state.borrow_mut();
            let sent = state
                .worker()
                .is_some_and(|worker| worker.jobs.send(Job::List(folder.to_owned())).is_ok());
            if sent {
                state.listings_out += 1;
            }
            sent
        };
        if sent {
            self.receive(Some(Instant::now() + LISTING_WAIT));
        } else {
            self.rebuild(list_clips(folder));
        }
    }

    /// Collects the worker's finished jobs. With a deadline, waits until then for a listing.
    fn receive(&self, deadline: Option<Instant>) {
        loop {
            let done = {
                let state = self.0.state.borrow();
                let Some(worker) = &state.worker else {
                    return;
                };
                match deadline {
                    Some(deadline) => worker
                        .done
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .ok(),
                    None => worker.done.try_recv().ok(),
                }
            };
            match done {
                Some(Done::Thumbnail(path, result)) => self.receive_thumbnail(path, result),
                Some(Done::List(folder, clips)) => {
                    self.receive_listing(&folder, clips);
                    if deadline.is_some() {
                        return;
                    }
                }
                None => return,
            }
        }
    }

    /// Rebuilds the list from a listing. A listing of a folder that is no longer the current one
    /// is dropped.
    fn receive_listing(&self, folder: &Path, clips: Vec<Clip>) {
        let current = {
            let mut state = self.0.state.borrow_mut();
            state.listings_out = state.listings_out.saturating_sub(1);
            state.folder.as_deref() == Some(folder)
        };
        if current {
            self.rebuild(clips);
        }
    }

    /// The list's messages re-enter the window procedure with notifications, so the state is not
    /// borrowed while they run. No clip is listed in the meantime, so no item resolves to a stale
    /// path.
    fn rebuild(&self, clips: Vec<Clip>) {
        let images: Vec<Option<u32>> = {
            let mut state = self.0.state.borrow_mut();
            state.clips.clear();
            clips
                .iter()
                .map(|clip| state.thumbnails.image(&clip.path))
                .collect()
        };
        self.0.list.set_redraw(false);
        let _ = self.0.list.items().delete_all();
        for (clip, image) in clips.iter().zip(images) {
            if let Err(error) = self.insert_item(&clip.caption(), image) {
                log::warning(text::clip_not_listed(&clip.path, error));
            }
        }
        self.place_items(clips.len());
        self.0.list.set_redraw(true);
        let _ = self.0.list.hwnd().InvalidateRect(None, true);
        let mut state = self.0.state.borrow_mut();
        state.clips = clips;
        let State {
            clips,
            thumbnails,
            worker,
            ..
        } = &mut *state;
        if let Some(worker) = worker {
            thumbnails.request_missing(clips, worker);
        }
    }

    /// Places the items on a centred grid. Left to its own layout, the list keeps its slack on the
    /// right, even when given a work area.
    #[expect(unsafe_code, reason = "sends LVM_SETITEMPOSITION32")]
    fn place_items(&self, count: usize) {
        let hwnd = self.0.list.hwnd();
        let Ok(client) = hwnd.GetClientRect() else {
            return;
        };
        let scroll_bar = layout::scroll_bar_width();
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

    /// The thumbnail's offset from an item's position, determined by placing item 0 at the origin.
    #[expect(unsafe_code, reason = "sends LVM_SETITEMPOSITION32")]
    fn icon_offset(&self) -> w::POINT {
        let hwnd = self.0.list.hwnd();
        let origin = w::POINT::with(0, 0);
        // SAFETY: LVM_SETITEMPOSITION32 reads the point during the call.
        unsafe {
            hwnd.SendMessage(w::msg::LvmSetItemPosition32 {
                index: 0,
                position: origin,
            });
        }
        let Ok(icon) = self.0.list.items().get(0).rect(co::LVIR::ICON) else {
            return origin;
        };
        // The rectangle is the padded icon area, and the thumbnail is centred in it.
        let (width, height) = thumbnail_size();
        w::POINT::with(
            icon.left + (icon.right - icon.left - width) / 2,
            icon.top + (icon.bottom - icon.top - height) / 2,
        )
    }

    /// The borrow ends before the list is updated, because `LVM_SETITEM` re-enters the window
    /// procedure.
    fn receive_thumbnail(&self, path: PathBuf, result: Result<Thumbnail, ThumbnailError>) {
        // The clip was deleted while its thumbnail was being made.
        if !self.0.state.borrow().thumbnails.requested.contains(&path) {
            return;
        }
        match result.and_then(|thumbnail| self.add_image(&thumbnail)) {
            Ok(image) => {
                let index = {
                    let mut state = self.0.state.borrow_mut();
                    let index = state.clips.iter().position(|clip| clip.path == path);
                    state.thumbnails.images.insert(path, image);
                    index
                };
                if let Some(index) = index
                    && let Ok(index) = u32::try_from(index)
                {
                    let _ = self.0.list.items().get(index).set_icon_index(Some(image));
                }
            }
            Err(error) => log::warning(text::thumbnail_failed(&path, error)),
        }
    }

    fn add_image(&self, thumbnail: &Thumbnail) -> Result<u32, ThumbnailError> {
        self.0
            .list
            .image_list(co::LVSIL::NORMAL)
            .and_then(|images| images.Add(thumbnail.bitmap(), None))
            .map_err(ThumbnailError::ImageList)
    }

    #[expect(unsafe_code, reason = "sends LVM_SETIMAGELIST")]
    fn install_image_list(&self) {
        let size = thumbnail_size();
        let images = w::HIMAGELIST::Create(w::SIZE::with(size.0, size.1), co::ILC::COLOR32, 16, 16)
            .inspect_err(|error| log::warning(text::thumbnails_unavailable(error)));
        let Ok(mut images) = images else {
            return;
        };
        let placeholder = shell::placeholder(size, CARD, MARK).and_then(|placeholder| {
            images
                .Add(placeholder.bitmap(), None)
                .map_err(ThumbnailError::ImageList)
        });
        match placeholder {
            Ok(index) => self.0.state.borrow_mut().thumbnails.placeholder = Some(index),
            Err(error) => log::warning(text::thumbnails_unavailable(error)),
        }
        // SAFETY: LVM_SETIMAGELIST takes the handle by value, and the control owns it from then on.
        unsafe {
            self.0.list.hwnd().SendMessage(w::msg::LvmSetImageList {
                kind: co::LVSIL::NORMAL,
                himagelist: Some(images.leak()),
            });
        }
    }

    #[expect(unsafe_code, reason = "sends LVM_SETICONSPACING")]
    fn set_icon_spacing(&self) {
        // SAFETY: LVM_SETICONSPACING takes both lengths packed by value.
        unsafe {
            self.0.list.hwnd().SendMessage(w::msg::LvmSetIconSpacing {
                size: w::SIZE::with(gui::dpi_x(CELL_WIDTH), gui::dpi_y(CELL_HEIGHT)),
            });
        }
    }

    #[expect(unsafe_code, reason = "sends LVM_SETBKCOLOR and LVM_SETTEXTCOLOR")]
    fn set_colours(&self, background: w::COLORREF, text: w::COLORREF) {
        let hwnd = self.0.list.hwnd();
        // SAFETY: LVM_SETBKCOLOR takes the colour by value and nothing by pointer.
        let _ = unsafe {
            hwnd.SendMessage(w::msg::LvmSetBkColor {
                color: Some(background),
            })
        };
        // SAFETY: LVM_SETTEXTBKCOLOR takes the colour by value and nothing by pointer.
        let _ = unsafe {
            hwnd.SendMessage(w::msg::LvmSetTextBkColor {
                color: Some(background),
            })
        };
        // SAFETY: LVM_SETTEXTCOLOR takes the colour by value and nothing by pointer.
        let _ = unsafe { hwnd.SendMessage(w::msg::LvmSetTextColor { color: Some(text) }) };
    }

    /// Sends `LVM_INSERTITEM` directly, because the control's helper requires a column header,
    /// which an icon view lacks.
    #[expect(unsafe_code, reason = "sends LVM_INSERTITEM")]
    fn insert_item(&self, caption: &str, image: Option<u32>) -> w::SysResult<u32> {
        let mut item = w::LVITEM::default();
        item.iItem = i32::MAX;
        item.mask = co::LVIF::TEXT | co::LVIF::IMAGE;
        let mut text = w::WString::from_str(caption);
        item.set_pszText(Some(&mut text));
        item.iImage = image.map_or(-1, |index| i32::try_from(index).unwrap_or(-1));
        // SAFETY: LVM_INSERTITEM reads the item and its text, which both outlive the call.
        unsafe {
            self.0
                .list
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
    /// Listings requested from the worker and not yet collected.
    listings_out: usize,
    thumbnails: Thumbnails,
    worker: Option<Worker>,
}

impl State {
    /// The worker, started on first use.
    fn worker(&mut self) -> Option<&Worker> {
        if self.worker.is_none() {
            self.worker = Worker::start(thumbnail_size());
        }
        self.worker.as_ref()
    }
}

/// The clips' thumbnails in the list's image list, and the outstanding requests for the rest.
#[derive(Default)]
struct Thumbnails {
    placeholder: Option<u32>,
    images: HashMap<PathBuf, u32>,
    requested: HashSet<PathBuf>,
}

impl Thumbnails {
    fn image(&self, path: &Path) -> Option<u32> {
        self.images.get(path).copied().or(self.placeholder)
    }

    fn request_missing(&mut self, clips: &[Clip], worker: &Worker) {
        let missing: Vec<PathBuf> = clips
            .iter()
            .map(|clip| clip.path.clone())
            .filter(|path| !self.images.contains_key(path) && !self.requested.contains(path))
            .collect();
        for path in missing {
            if worker.jobs.send(Job::Thumbnail(path.clone())).is_ok() {
                self.requested.insert(path);
            }
        }
    }

    /// Forgets the clip's thumbnail. The image stays in the image list, because removing one
    /// renumbers those after it.
    fn forget(&mut self, path: &Path) {
        self.images.remove(path);
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

    /// The menu command id, counted from 1, because the menu reports no choice as 0.
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

/// Work that can block on the disk or the shell, kept off the UI thread.
enum Job {
    List(PathBuf),
    Thumbnail(PathBuf),
}

enum Done {
    List(PathBuf, Vec<Clip>),
    Thumbnail(PathBuf, Result<Thumbnail, ThumbnailError>),
}

/// Dropping this closes the job channel and ends the thread.
struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Done>,
}

impl Worker {
    fn start(size: (i32, i32)) -> Option<Self> {
        let (jobs, jobs_in) = mpsc::channel();
        let (done_out, done) = mpsc::channel();
        thread::Builder::new()
            .name("clips".to_owned())
            .spawn(move || serve(size, &jobs_in, &done_out))
            .inspect_err(|error| log::warning(text::thumbnails_unavailable(error)))
            .ok()?;
        Some(Self { jobs, done })
    }
}

/// Serves jobs until the sender is dropped.
fn serve(size: (i32, i32), jobs: &Receiver<Job>, done: &Sender<Done>) {
    let _ = sys_thread::enter_background_mode();
    let com = Apartment::single_threaded();
    for job in jobs {
        let finished = match job {
            Job::List(folder) => {
                let clips = list_clips(&folder);
                Done::List(folder, clips)
            }
            Job::Thumbnail(path) => {
                let made = match &com {
                    Ok(_) => shell::thumbnail(&path, size),
                    Err(error) => Err(ThumbnailError::Com(error.clone())),
                };
                Done::Thumbnail(path, made)
            }
        };
        if done.send(finished).is_err() {
            return;
        }
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
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case(ClipName::EXTENSION))
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

    /// The sort key: the recorder's names before the rest, then by name. The key is borrowed, so
    /// sorting allocates nothing. `new` admits only UTF-8 names, which order as their text does.
    fn order(&self) -> (Option<&ClipName>, Option<&OsStr>) {
        (self.name.as_ref(), self.path.file_name())
    }
}

fn list_clips(folder: &Path) -> Vec<Clip> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut clips: Vec<Clip> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Clip::new(entry.path(), metadata.len())
        })
        .collect();
    newest_first(&mut clips);
    clips
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
    (0..).map(move |index: usize| {
        let (row, column) = (
            saturating_i32(index / COLUMNS),
            saturating_i32(index % COLUMNS),
        );
        w::POINT::with(
            area.left + column * cell_width - offset.x,
            area.top + row * cell_height - offset.y,
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
    let rows = saturating_i32(count.div_ceil(COLUMNS));
    let span = saturating_i32(COLUMNS) * cell_width - gutter;
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
        let mut thumbnails = Thumbnails::default();
        for (path, image) in [(&gone, 1), (&kept, 2)] {
            thumbnails.images.insert(path.clone(), image);
            thumbnails.requested.insert(path.clone());
        }
        thumbnails.forget(&gone);
        assert!(!thumbnails.images.contains_key(&gone));
        assert!(!thumbnails.requested.contains(&gone));
        assert_eq!(thumbnails.image(&kept), Some(2));
        assert!(thumbnails.requested.contains(&kept));
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
