//! Shareable copies made from the Clips tab, one at a time on a thread of their own, the rest
//! waiting their turn.

use crate::{log, share, sys::thread as sys_thread};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
};

/// How far a clip's copy has come, for its thumbnail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Progress {
    Waiting,
    /// Of the current attempt, which starts over when a copy came out too large.
    Percent(u32),
}

/// A copy that ended, made or not.
pub(super) struct Ended {
    pub(super) clip: PathBuf,
    pub(super) made: anyhow::Result<PathBuf>,
}

#[derive(Default)]
pub(super) struct Copies {
    making: Option<Making>,
    /// Clips waiting, with the limit each was asked for at, in bytes.
    waiting: VecDeque<(PathBuf, u64)>,
}

struct Making {
    clip: PathBuf,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    done: Receiver<anyhow::Result<PathBuf>>,
}

impl Copies {
    /// Queues a copy of `clip` within `limit` bytes, unless one is already queued or being made.
    pub(super) fn add(&mut self, clip: &Path, limit: u64) {
        if self.progress(clip).is_none() {
            self.waiting.push_back((clip.to_owned(), limit));
        }
        if self.making.is_none() {
            self.start_next();
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.making.is_some() || !self.waiting.is_empty()
    }

    pub(super) fn progress(&self, clip: &Path) -> Option<Progress> {
        if let Some(making) = self.making.as_ref().filter(|making| making.clip == clip) {
            let permille = making.progress.load(Ordering::Relaxed);
            return Some(Progress::Percent(
                (permille * 100 / crate::gpu::PROGRESS_SCALE).min(100),
            ));
        }
        self.waiting
            .iter()
            .any(|(waiting, _)| waiting == clip)
            .then_some(Progress::Waiting)
    }

    /// Stops the copy of `clip`, which is then not made.
    pub(super) fn cancel(&mut self, clip: &Path) {
        self.waiting.retain(|(waiting, _)| waiting != clip);
        if let Some(making) = self.making.as_ref().filter(|making| making.clip == clip) {
            making.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Collects the copy that ended, if one did, and starts the next.
    pub(super) fn collect(&mut self) -> Option<Ended> {
        let making = self.making.as_ref()?;
        let made = match making.done.try_recv() {
            Err(TryRecvError::Empty) => return None,
            Ok(made) => made,
            Err(TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("the copy stopped unexpectedly"))
            }
        };
        let clip = self.making.take().map(|making| making.clip)?;
        self.start_next();
        Some(Ended { clip, made })
    }

    fn start_next(&mut self) {
        while let Some((clip, limit)) = self.waiting.pop_front() {
            let progress = Arc::new(AtomicU32::new(0));
            let cancel = Arc::new(AtomicBool::new(false));
            let (finished, done) = mpsc::channel();
            let (path, theirs, stop) = (clip.clone(), Arc::clone(&progress), Arc::clone(&cancel));
            let started = thread::Builder::new()
                .name("shareable".to_owned())
                .spawn(move || {
                    let _ = sys_thread::enter_background_mode();
                    let _ = sys_thread::prefer_efficiency();
                    let _ = finished.send(share::prepare(&path, limit, &theirs, &stop));
                });
            match started {
                Ok(_) => {
                    self.making = Some(Making {
                        clip,
                        progress,
                        cancel,
                        done,
                    });
                    return;
                }
                Err(error) => log::warning(format!(
                    "Share · {} · the copy did not start ({error})",
                    clip.display()
                )),
            }
        }
    }
}

impl Drop for Copies {
    fn drop(&mut self) {
        if let Some(making) = &self.making {
            making.cancel.store(true, Ordering::Relaxed);
        }
    }
}
