//! The About tab: a description of hindsight, the build and system details for a bug report,
//! links, and updates.

use super::{
    layout::{
        self, BUTTON_HEIGHT, BUTTON_WIDTH, Frame, INNER_WIDTH, INNER_X, LINE, Painted, TintedLabel,
        VALUE_WIDTH, VALUE_X, button, label, saturating_i32, skin_button,
    },
    pane::{self, FIRST_FRAME, FIRST_ROW, Pane},
    text,
    theme::{Skin, Surface, Tone},
};
use crate::{
    config::Theme,
    gpu, log, storage,
    sys::{icon, release, shell},
    update::{Installation, State},
};
use std::{cell::RefCell, path::Path, rc::Rc};
use winsafe::{self as w, co, guard::DestroyIconGuard, gui, prelude::*};

const HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const COMMIT: &str = env!("HINDSIGHT_COMMIT");
const BUILD_DATE: &str = env!("HINDSIGHT_BUILD_DATE");
const RUSTC: &str = env!("HINDSIGHT_RUSTC");

const LOGO: i32 = 48;
const LOGO_GAP: i32 = 12;
const TEXT_X: i32 = INNER_X + LOGO + LOGO_GAP;
const TEXT_WIDTH: i32 = INNER_WIDTH - LOGO - LOGO_GAP;
const ROW_PITCH: i32 = 20;
const BUTTON_GAP: i32 = 6;

mod rows {
    use super::{BUTTON_HEIGHT, FIRST_FRAME, FIRST_ROW, LINE, ROW_PITCH};

    pub(super) const ABOUT_FRAME: i32 = FIRST_FRAME;
    pub(super) const NAME: i32 = ABOUT_FRAME + FIRST_ROW;
    pub(super) const DESCRIPTION: i32 = NAME + LINE + 2;
    pub(super) const ABOUT_HEIGHT: i32 = DESCRIPTION + 2 * LINE + 4 - ABOUT_FRAME;
    pub(super) const DETAILS_FRAME: i32 = ABOUT_FRAME + ABOUT_HEIGHT + 6;
    pub(super) const BUILD: i32 = DETAILS_FRAME + FIRST_ROW;
    pub(super) const COMPILER: i32 = BUILD + ROW_PITCH;
    pub(super) const WINDOWS: i32 = COMPILER + ROW_PITCH;
    pub(super) const GPU: i32 = WINDOWS + ROW_PITCH;
    pub(super) const DETAILS_HEIGHT: i32 = GPU + LINE + 2 - DETAILS_FRAME;
    pub(super) const LINKS_FRAME: i32 = DETAILS_FRAME + DETAILS_HEIGHT + 6;
    pub(super) const WEBSITE: i32 = LINKS_FRAME + FIRST_ROW - 2;
    pub(super) const LINKS: i32 = WEBSITE + BUTTON_HEIGHT + 6;
    pub(super) const LINKS_HEIGHT: i32 = LINKS + BUTTON_HEIGHT + 8 - LINKS_FRAME;
    pub(super) const UPDATES_FRAME: i32 = LINKS_FRAME + LINKS_HEIGHT + 6;
    pub(super) const UPDATE_STATE: i32 = UPDATES_FRAME + FIRST_ROW;
    pub(super) const UPDATE_BUTTONS: i32 = UPDATE_STATE + 2 * LINE + 4;
    pub(super) const UPDATES_HEIGHT: i32 = UPDATE_BUTTONS + BUTTON_HEIGHT + 8 - UPDATES_FRAME;
}

const DETAIL_ROWS: [i32; 4] = [rows::BUILD, rows::COMPILER, rows::WINDOWS, rows::GPU];

/// The target of each of the Links group's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Website,
    GitHub,
    Report,
    LogFolder,
}

/// What each of the Updates group's buttons asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UpdateAction {
    Check,
    Install,
    Notes,
}

#[derive(Clone)]
pub(super) struct AboutPage(Rc<Inner>);

struct Inner {
    page: gui::TabPage,
    frames: Vec<Frame>,
    description: TintedLabel,
    buttons: Vec<(gui::Button, Link)>,
    update_state: TintedLabel,
    update_buttons: Vec<(gui::Button, UpdateAction)>,
}

impl Painted for Inner {
    fn frames(&self) -> &[Frame] {
        &self.frames
    }

    fn tinted(&self) -> impl Iterator<Item = &TintedLabel> {
        [&self.description, &self.update_state].into_iter()
    }
}

impl Pane for AboutPage {
    fn title(&self) -> &'static str {
        text::TAB_ABOUT
    }

    fn page(&self) -> &gui::TabPage {
        &self.0.page
    }

    fn on_create(&self) {
        layout::fit_titles(&self.0.frames);
    }

    fn frames(&self) -> &[Frame] {
        &self.0.frames
    }
}

impl AboutPage {
    pub(super) fn new(parent: &(impl GuiParent + 'static), skin: &Skin) -> Self {
        let page = layout::page(parent);
        let frames = vec![
            Frame::new(
                &page,
                text::GROUP_ABOUT,
                rows::ABOUT_FRAME,
                rows::ABOUT_HEIGHT,
            ),
            Frame::new(
                &page,
                text::GROUP_DETAILS,
                rows::DETAILS_FRAME,
                rows::DETAILS_HEIGHT,
            ),
            Frame::new(
                &page,
                text::GROUP_LINKS,
                rows::LINKS_FRAME,
                rows::LINKS_HEIGHT,
            ),
            Frame::new(
                &page,
                text::GROUP_UPDATES,
                rows::UPDATES_FRAME,
                rows::UPDATES_HEIGHT,
            ),
        ];
        logo(&page, skin);
        let _ = label(
            &page,
            &text::name_and_version(),
            TEXT_X,
            rows::NAME,
            TEXT_WIDTH,
            1,
        );
        let description = TintedLabel::new(
            label(
                &page,
                text::DESCRIPTION,
                TEXT_X,
                rows::DESCRIPTION,
                TEXT_WIDTH,
                2,
            ),
            Tone::Muted,
        );
        for ((caption, value), y) in details().into_iter().zip(DETAIL_ROWS) {
            let _ = label(&page, caption, INNER_X, y, VALUE_X - INNER_X, 1);
            let _ = label(&page, &value, VALUE_X, y, VALUE_WIDTH, 1);
        }
        let buttons = buttons(&page, skin);
        let update_state = TintedLabel::new(
            label(
                &page,
                text::UPDATE_UNCHECKED,
                INNER_X,
                rows::UPDATE_STATE,
                INNER_WIDTH,
                2,
            ),
            Tone::Muted,
        );
        let update_buttons = update_buttons(&page, skin);
        let inner = Rc::new(Inner {
            page,
            frames,
            description,
            buttons,
            update_state,
            update_buttons,
        });
        pane::paint(&inner.page, skin, Rc::clone(&inner));
        let this = Self(inner);
        this.handle_clicks();
        this
    }

    pub(super) fn on_update_action(&self, act: impl Fn(UpdateAction) + Clone + 'static) {
        for (button, action) in &self.0.update_buttons {
            let action = *action;
            let act = act.clone();
            button.on().bn_clicked(move || {
                act(action);
                Ok(())
            });
        }
    }

    pub(super) fn show_update(&self, state: &State, installation: &Installation) {
        let (message, tone) = match state {
            State::Unchecked => (text::UPDATE_UNCHECKED.to_owned(), Tone::Muted),
            State::Checking => (text::UPDATE_CHECKING.to_owned(), Tone::Muted),
            State::Current => (text::UPDATE_CURRENT.to_owned(), Tone::Muted),
            State::Available(release) => (text::update_available(release.version()), Tone::Good),
            State::Downloading(release) => {
                (text::update_downloading(release.version()), Tone::Muted)
            }
            State::CheckFailed(reason) => (text::update_check_failed(reason), Tone::Error),
            State::UpdateFailed(_, reason) => (text::update_failed(reason), Tone::Error),
        };
        self.0.update_state.set_tone(tone);
        self.0.update_state.set_text(&message);
        let busy = matches!(state, State::Checking | State::Downloading(_));
        let offered = matches!(state, State::Available(_) | State::UpdateFailed(..));
        for (button, action) in &self.0.update_buttons {
            let enabled = match action {
                UpdateAction::Check => !busy,
                UpdateAction::Install => offered,
                UpdateAction::Notes => state.release().is_some(),
            };
            button.hwnd().EnableWindow(enabled);
            if *action == UpdateAction::Install {
                layout::set_text(button, install_caption(installation));
            }
        }
    }

    fn handle_clicks(&self) {
        for (button, link) in &self.0.buttons {
            let link = *link;
            button.on().bn_clicked(move || {
                follow(link);
                Ok(())
            });
        }
    }
}

/// The Details group's captions and values, read once, since none of them changes while the app
/// runs.
fn details() -> [(&'static str, String); 4] {
    [
        (text::BUILD, text::build(COMMIT, BUILD_DATE)),
        (text::COMPILER, text::compiler(RUSTC)),
        (text::WINDOWS, text::windows_release(&release::current())),
        (text::GPU, text::gpu(&gpu::adapter())),
    ]
}

/// The website sits on a row of its own, above the GitHub button.
fn buttons(page: &gui::TabPage, skin: &Skin) -> Vec<(gui::Button, Link)> {
    let website = (text::LINK_WEBSITE, Link::Website, 0, rows::WEBSITE);
    let row = [
        (text::LINK_GITHUB, Link::GitHub),
        (text::LINK_REPORT, Link::Report),
        (text::LINK_LOG_FOLDER, Link::LogFolder),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (caption, link))| (caption, link, index, rows::LINKS));
    std::iter::once(website)
        .chain(row)
        .map(|(caption, link, column, y)| {
            let x = INNER_X + saturating_i32(column) * (BUTTON_WIDTH + BUTTON_GAP);
            let button = button(page, caption, x, y, BUTTON_WIDTH);
            skin_button(&button, skin);
            (button, link)
        })
        .collect()
}

fn update_buttons(page: &gui::TabPage, skin: &Skin) -> Vec<(gui::Button, UpdateAction)> {
    [
        (text::UPDATE_CHECK, UpdateAction::Check),
        (text::UPDATE_INSTALL, UpdateAction::Install),
        (text::UPDATE_NOTES, UpdateAction::Notes),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (caption, action))| {
        let x = INNER_X + saturating_i32(index) * (BUTTON_WIDTH + BUTTON_GAP);
        let button = button(page, caption, x, rows::UPDATE_BUTTONS, BUTTON_WIDTH);
        skin_button(&button, skin);
        (button, action)
    })
    .collect()
}

/// The portable copy is not updated in place, so its button opens the release instead.
fn install_caption(installation: &Installation) -> &'static str {
    match installation {
        Installation::Installed(_) => text::UPDATE_INSTALL,
        Installation::Portable => text::UPDATE_DOWNLOAD,
    }
}

fn follow(link: Link) {
    match link {
        Link::Website => open_link(HOMEPAGE),
        Link::GitHub => open_link(REPOSITORY),
        Link::Report => open_link(&issues_url(REPOSITORY)),
        Link::LogFolder => open_log_folder(),
    }
}

/// Creates the logo label, which paints the theme's owl over the panel it sits on.
fn logo(page: &gui::TabPage, skin: &Skin) {
    let logo = gui::Label::new(
        page,
        gui::LabelOpts {
            text: "",
            position: gui::dpi(INNER_X, rows::NAME),
            size: gui::dpi(LOGO, LOGO),
            ..Default::default()
        },
    );
    let skin = skin.clone();
    let owner = logo.clone();
    let loaded: RefCell<Option<(Theme, DestroyIconGuard)>> = RefCell::default();
    logo.on_subclass().wm_paint(move || {
        let hwnd = owner.hwnd();
        let Ok(paint) = hwnd.BeginPaint() else {
            return Ok(());
        };
        let Ok(client) = hwnd.GetClientRect() else {
            return Ok(());
        };
        let _ = paint.FillRect(client, &skin.label_brush(Surface::Page));
        let theme = skin.theme();
        let mut loaded = loaded.borrow_mut();
        if loaded.as_ref().is_none_or(|(shown, _)| *shown != theme) {
            *loaded = load_icon(theme, client.right).map(|icon| (theme, icon));
        }
        if let Some((_, icon)) = &*loaded {
            draw_icon(&paint, icon, client.right);
        }
        Ok(())
    });
}

/// Loads the icon unshared, because a shared icon keeps the first size it was loaded at.
fn load_icon(theme: Theme, size: i32) -> Option<DestroyIconGuard> {
    w::HINSTANCE::GetModuleHandle(None)
        .ok()?
        .LoadImageIcon(
            w::IdOicStr::Id(icon::resource(theme)),
            w::SIZE::with(size, size),
            co::LR::DEFAULTCOLOR,
        )
        .ok()
}

/// Draws the icon with `DrawIconEx`, which blends its alpha over the existing pixels. `winsafe`
/// has no wrapper for it.
#[expect(unsafe_code, reason = "calls DrawIconEx")]
fn draw_icon(hdc: &w::HDC, icon: &w::HICON, size: i32) {
    use windows::Win32::{
        Graphics::Gdi::HDC,
        UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, HICON},
    };

    // SAFETY: the DC is the live paint target and the icon is alive for the call.
    let _ = unsafe {
        DrawIconEx(
            HDC(hdc.ptr()),
            0,
            0,
            HICON(icon.ptr()),
            size,
            size,
            0,
            None,
            DI_NORMAL,
        )
    };
}

pub(super) fn open_link(url: &str) {
    if let Err(error) = shell::open(Path::new(url)) {
        log::warning(text::link_open_failed(url, error));
    }
}

/// Selects the log file in Explorer, or opens its folder if the file has not been written yet.
fn open_log_folder() {
    let folder = storage::state_directory();
    let file = folder.join(log::FILE_NAME);
    let result = if file.is_file() {
        shell::reveal(&file).map_err(|error| error.to_string())
    } else {
        shell::open(&folder).map_err(|error| error.to_string())
    };
    if let Err(error) = result {
        log::warning(text::link_open_failed(folder.display(), error));
    }
}

/// The issues URL of `repository`, stripped of a trailing slash or `.git`, either of which
/// `Cargo.toml` may include.
fn issues_url(repository: &str) -> String {
    let root = repository.trim_end_matches('/');
    format!("{}/issues", root.strip_suffix(".git").unwrap_or(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_fit_the_page_without_growing_the_window() {
        assert_eq!(
            [rows::NAME, rows::DESCRIPTION, rows::ABOUT_HEIGHT],
            [26, 46, 80]
        );
        assert_eq!(
            [
                rows::DETAILS_FRAME,
                rows::BUILD,
                rows::GPU,
                rows::DETAILS_HEIGHT
            ],
            [92, 112, 172, 100]
        );
        assert_eq!(
            [
                rows::LINKS_FRAME,
                rows::WEBSITE,
                rows::LINKS,
                rows::LINKS_HEIGHT
            ],
            [198, 216, 246, 80]
        );
        assert_eq!(
            [
                rows::UPDATES_FRAME,
                rows::UPDATE_STATE,
                rows::UPDATE_BUTTONS,
                rows::UPDATES_HEIGHT
            ],
            [284, 304, 344, 92]
        );
        const {
            assert!(
                rows::UPDATES_FRAME + rows::UPDATES_HEIGHT <= layout::FILL_Y + layout::FILL_HEIGHT
            );
            assert!(3 * BUTTON_WIDTH + 2 * BUTTON_GAP <= INNER_WIDTH);
        }
    }

    #[test]
    fn the_issues_hang_off_the_repository_however_it_is_written() {
        let repository = "https://github.com/malinoskj2/hindsight";
        let issues = format!("{repository}/issues");
        assert_eq!(issues_url(repository), issues);
        assert_eq!(issues_url(&format!("{repository}/")), issues);
        assert_eq!(issues_url(&format!("{repository}.git")), issues);
        assert_eq!(issues_url(REPOSITORY), format!("{REPOSITORY}/issues"));
    }

    #[test]
    fn the_build_facts_fall_back_to_unknown_rather_than_nothing() {
        for fact in [COMMIT, BUILD_DATE, RUSTC] {
            assert_ne!(fact, "");
        }
        assert!(BUILD_DATE == "unknown" || BUILD_DATE.len() == "2026-10-14".len());
    }
}
