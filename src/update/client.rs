//! The thread that checks for and downloads updates, and the hand-over to the installer.

use super::{Check, ForgedInstaller, Release, ReleaseError, Version, installer_parameters};
use crate::{
    storage,
    sys::{
        http::{self, HttpError},
        shell::{self, ElevateError, Elevation},
        thread as sys_thread,
    },
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};
use thiserror::Error;
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

const DOWNLOAD_DIRECTORY: &str = "updates";
const MAX_SIGNATURE_BYTES: usize = 4 << 10;
const MAX_INSTALLER_BYTES: usize = 64 << 20;

enum Job {
    Check,
    Download(Release),
}

pub(crate) enum Done {
    Checked(Result<Check, CheckError>),
    Downloaded(Release, Result<Installer, DownloadError>),
}

#[derive(Debug, Error)]
pub(crate) enum CheckError {
    #[error("the latest release could not be fetched")]
    Fetch(#[from] HttpError),
    #[error("the latest release's signature is not text")]
    NotText,
    #[error(transparent)]
    Release(#[from] ReleaseError),
}

#[derive(Debug, Error)]
pub(crate) enum DownloadError {
    #[error("the installer could not be downloaded")]
    Fetch(#[from] HttpError),
    #[error("the installer could not be saved")]
    Save(#[source] io::Error),
    #[error(transparent)]
    Forged(#[from] ForgedInstaller),
}

/// Dropping this closes the job channel, and the thread ends after its current job.
pub(crate) struct Updater {
    jobs: Sender<Job>,
    done: Receiver<Done>,
}

impl Updater {
    pub(crate) fn start() -> io::Result<Self> {
        let (jobs, jobs_in) = mpsc::channel();
        let (done_out, done) = mpsc::channel();
        thread::Builder::new()
            .name("updates".to_owned())
            .spawn(move || serve(&jobs_in, &done_out))?;
        Ok(Self { jobs, done })
    }

    pub(crate) fn check(&self) {
        let _ = self.jobs.send(Job::Check);
    }

    pub(crate) fn download(&self, release: Release) {
        let _ = self.jobs.send(Job::Download(release));
    }

    /// The next finished job, if any.
    pub(crate) fn finished(&self) -> Option<Done> {
        self.done.try_recv().ok()
    }
}

/// A downloaded installer, verified as read through a handle that keeps it from being changed
/// until it has been started.
pub(crate) struct Installer {
    path: PathBuf,
    _lock: File,
}

impl Installer {
    /// Starts the installer as administrator to update the copy in `folder`. Once it has
    /// started, it waits for this process to exit.
    pub(crate) fn launch(self, folder: &Path) -> Result<Elevation, ElevateError> {
        let folder = folder.to_str().ok_or(ElevateError::Path)?;
        shell::run_elevated(&self.path, &installer_parameters(folder))
    }
}

/// Serves jobs until the sender is dropped.
fn serve(jobs: &Receiver<Job>, done: &Sender<Done>) {
    let _ = sys_thread::enter_background_mode();
    // Installers from earlier updates have done their work. One still running stays locked and
    // goes next time.
    let _ = fs::remove_dir_all(download_directory());
    for job in jobs {
        let finished = match job {
            Job::Check => Done::Checked(check()),
            Job::Download(release) => {
                let installer = download(&release);
                Done::Downloaded(release, installer)
            }
        };
        if done.send(finished).is_err() {
            return;
        }
    }
}

fn check() -> Result<Check, CheckError> {
    let body = http::get(&super::latest_signature_url(), MAX_SIGNATURE_BYTES)?;
    let signature = String::from_utf8(body).map_err(|_| CheckError::NotText)?;
    let latest = Release::announced(&signature)?;
    Ok(Check::of(&Version::for_updates(), latest))
}

fn download(release: &Release) -> Result<Installer, DownloadError> {
    let body = http::get(&release.installer_url(), MAX_INSTALLER_BYTES)?;
    let folder = download_directory();
    fs::create_dir_all(&folder).map_err(DownloadError::Save)?;
    let path = folder.join(format!("hindsight-setup-{}.exe", release.version()));
    fs::write(&path, &body).map_err(DownloadError::Save)?;

    // Sharing only reading keeps anything from writing, renaming or deleting the file while
    // this handle is open, so what is verified here is what the installer runs.
    let mut lock = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(&path)
        .map_err(DownloadError::Save)?;
    let mut saved = Vec::with_capacity(body.len());
    lock.read_to_end(&mut saved).map_err(DownloadError::Save)?;
    release.verify(&saved)?;
    Ok(Installer { path, _lock: lock })
}

fn download_directory() -> PathBuf {
    storage::state_directory().join(DOWNLOAD_DIRECTORY)
}
