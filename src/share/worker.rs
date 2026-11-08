//! One owned background worker, with at most one pending copy. A newer request cancels the
//! current conversion and replaces the pending request, so an old encode cannot win later.

use super::{Copy, ShareError, limit_bytes};
use crate::log;
use anyhow::{Context, Result};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

struct Request {
    path: PathBuf,
    copy: Copy,
    limit: u64,
    revision: u32,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    pending: Option<Request>,
    active: Option<Arc<AtomicBool>>,
    stopped: bool,
}

#[derive(Default)]
struct Mailbox {
    state: Mutex<State>,
    ready: Condvar,
}

type Preparer = Box<dyn Fn(&Path, u64, &AtomicBool) -> Result<PathBuf> + Send>;
type Copier = Box<dyn Fn(&Path) -> Result<()> + Send>;

struct Effects {
    prepare: Preparer,
    copy: Copier,
    revision: fn() -> u32,
}

pub(crate) struct Worker {
    mailbox: Arc<Mailbox>,
    join: Option<JoinHandle<()>>,
    revision: fn() -> u32,
}

impl Worker {
    pub(crate) fn start() -> io::Result<Self> {
        Self::start_with(Effects {
            prepare: Box::new(prepare),
            copy: Box::new(copy),
            revision,
        })
    }

    fn start_with(effects: Effects) -> io::Result<Self> {
        let mailbox = Arc::new(Mailbox::default());
        let incoming = Arc::clone(&mailbox);
        let revision = effects.revision;
        let join = thread::Builder::new()
            .name("share-copy".to_owned())
            .spawn(move || {
                #[cfg(windows)]
                {
                    let _ = crate::sys::thread::enter_background_mode();
                    let _ = crate::sys::thread::prefer_efficiency();
                }
                serve(&incoming, &effects);
            })?;
        Ok(Self {
            mailbox,
            join: Some(join),
            revision,
        })
    }

    pub(crate) fn request(&self, path: &Path, copy: Copy, megabytes: u32) -> Result<()> {
        anyhow::ensure!(
            self.join.as_ref().is_some_and(|join| !join.is_finished()),
            "the clipboard worker has stopped"
        );
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(active) = &state.active {
            active.store(true, Ordering::Relaxed);
        }
        state.pending = Some(Request {
            path: path.to_owned(),
            copy,
            limit: limit_bytes(megabytes),
            revision: (self.revision)(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.mailbox.ready.notify_one();
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        {
            let mut state = self
                .mailbox
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.stopped = true;
            state.pending = None;
            if let Some(active) = &state.active {
                active.store(true, Ordering::Relaxed);
            }
            self.mailbox.ready.notify_one();
        }
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            log::error("Share · the clipboard worker panicked");
        }
    }
}

fn serve(mailbox: &Mailbox, effects: &Effects) {
    loop {
        let request = {
            let mut state = mailbox.state.lock().unwrap_or_else(PoisonError::into_inner);
            while state.pending.is_none() && !state.stopped {
                state = mailbox
                    .ready
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if state.stopped {
                return;
            }
            let Some(request) = state.pending.take() else {
                continue;
            };
            state.active = Some(Arc::clone(&request.cancel));
            request
        };
        let prepared = match request.copy {
            Copy::Original => Ok(request.path.clone()),
            Copy::Shareable => (effects.prepare)(&request.path, request.limit, &request.cancel),
        };
        // Serialize the final copy with incoming requests. A newer request always gets the
        // clipboard last; an external clipboard change while encoding is respected as well.
        let mut state = mailbox.state.lock().unwrap_or_else(PoisonError::into_inner);
        let cancelled = request.cancel.load(Ordering::Relaxed) || state.stopped;
        match prepared {
            Ok(path) if !cancelled && (effects.revision)() == request.revision => {
                match (effects.copy)(&path) {
                    Ok(()) => log::info(format!("Clipboard · copied {}", path.display())),
                    Err(error) => {
                        log::warning(format!("Clipboard · {} · {error:#}", path.display()));
                    }
                }
            }
            Ok(path) => {
                if path != request.path {
                    let _ = fs::remove_file(path);
                }
            }
            Err(error) if !cancelled => {
                log::warning(format!("Share · {} · {error:#}", request.path.display()));
            }
            Err(_) => {}
        }
        state.active = None;
    }
}

fn prepare(path: &Path, limit: u64, cancel: &AtomicBool) -> Result<PathBuf> {
    if fs::metadata(path).context("read the clip's size")?.len() <= limit {
        return Ok(path.to_owned());
    }
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), ShareError::Cancelled);
    log::info(format!(
        "Share · making a copy of {} within {} MB",
        path.display(),
        limit / 1_000_000
    ));
    encode(path, limit, cancel)
}

#[cfg(windows)]
fn encode(path: &Path, limit: u64, cancel: &AtomicBool) -> Result<PathBuf> {
    crate::gpu::share(path, limit, cancel).context("make the shareable copy with NVENC")
}

#[cfg(not(windows))]
fn encode(_: &Path, _: u64, _: &AtomicBool) -> Result<PathBuf> {
    anyhow::bail!("NVENC requires Windows")
}

#[cfg(windows)]
fn copy(path: &Path) -> Result<()> {
    Ok(crate::sys::clipboard::copy_file(path)?)
}

#[cfg(not(windows))]
fn copy(_: &Path) -> Result<()> {
    anyhow::bail!("the clipboard requires Windows")
}

fn revision() -> u32 {
    #[cfg(windows)]
    {
        crate::sys::clipboard::revision()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Barrier, mpsc},
        time::Duration,
    };

    const WAIT: Duration = Duration::from_secs(3);

    fn effects(copy: Copier) -> Effects {
        Effects {
            prepare: Box::new(prepare),
            copy,
            revision: || 0,
        }
    }

    #[test]
    fn small_clips_pass_through_and_original_copy_never_encodes() {
        let directory = tempfile::tempdir().unwrap();
        let small = directory.path().join("small.mp4");
        fs::write(&small, b"saved").unwrap();
        assert_eq!(prepare(&small, 5, &AtomicBool::new(false)).unwrap(), small);
        let giant = directory.path().join("giant.mp4");
        fs::File::create(&giant)
            .unwrap()
            .set_len(30_000_000)
            .unwrap();
        let (sent, copied) = mpsc::channel();
        let worker = Worker::start_with(Effects {
            prepare: Box::new(|_, _, _| panic!("Copy must never encode")),
            ..effects(Box::new(move |path| {
                sent.send(path.to_owned()).unwrap();
                Ok(())
            }))
        })
        .unwrap();
        worker.request(&giant, Copy::Original, 1).unwrap();
        assert_eq!(copied.recv_timeout(WAIT).unwrap(), giant);
    }

    #[test]
    fn a_newer_copy_cancels_an_encode_and_is_the_only_file_copied() {
        let (began, started) = mpsc::channel();
        let release = Arc::new(Barrier::new(2));
        let encoding = Arc::clone(&release);
        let (sent, copied) = mpsc::channel();
        let worker = Worker::start_with(Effects {
            prepare: Box::new(move |_, _, cancel| {
                began.send(()).unwrap();
                encoding.wait();
                assert!(cancel.load(Ordering::Relaxed));
                Err(ShareError::Cancelled.into())
            }),
            ..effects(Box::new(move |path| {
                sent.send(path.to_owned()).unwrap();
                Ok(())
            }))
        })
        .unwrap();
        worker
            .request(Path::new("old.mp4"), Copy::Shareable, 25)
            .unwrap();
        started.recv_timeout(WAIT).unwrap();
        worker
            .request(Path::new("superseded.mp4"), Copy::Original, 25)
            .unwrap();
        worker
            .request(Path::new("new.mp4"), Copy::Original, 25)
            .unwrap();
        release.wait();
        assert_eq!(copied.recv_timeout(WAIT).unwrap(), Path::new("new.mp4"));
        drop(worker);
        assert!(copied.try_recv().is_err());
    }

    #[test]
    fn dropping_the_worker_cancels_and_joins_the_active_encode() {
        let (began, started) = mpsc::channel();
        let (ended, finished) = mpsc::channel();
        let worker = Worker::start_with(Effects {
            prepare: Box::new(move |_, _, cancel| {
                began.send(()).unwrap();
                let deadline = std::time::Instant::now() + WAIT;
                while !cancel.load(Ordering::Relaxed) {
                    assert!(std::time::Instant::now() < deadline);
                    thread::yield_now();
                }
                ended.send(()).unwrap();
                Err(ShareError::Cancelled.into())
            }),
            ..effects(Box::new(|_| panic!("cancelled work must not copy")))
        })
        .unwrap();
        worker
            .request(Path::new("old.mp4"), Copy::Shareable, 25)
            .unwrap();
        started.recv_timeout(WAIT).unwrap();
        drop(worker);
        finished.recv_timeout(WAIT).unwrap();
    }

    #[test]
    fn a_failed_conversion_leaves_the_clipboard_untouched_and_the_worker_recovers() {
        let (failed, failure) = mpsc::channel();
        let (sent, copied) = mpsc::channel();
        let worker = Worker::start_with(Effects {
            prepare: Box::new(move |_, _, _| {
                failed.send(()).unwrap();
                anyhow::bail!("NVENC unavailable")
            }),
            ..effects(Box::new(move |path| {
                sent.send(path.to_owned()).unwrap();
                Ok(())
            }))
        })
        .unwrap();
        worker
            .request(Path::new("bad.mp4"), Copy::Shareable, 25)
            .unwrap();
        failure.recv_timeout(WAIT).unwrap();
        worker
            .request(Path::new("original.mp4"), Copy::Original, 25)
            .unwrap();
        assert_eq!(
            copied.recv_timeout(WAIT).unwrap(),
            Path::new("original.mp4")
        );
        drop(worker);
        assert!(copied.try_recv().is_err());
    }

    #[test]
    fn an_external_clipboard_change_during_encoding_is_respected() {
        static REVISION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("shareable.mp4");
        let encoded = output.clone();
        let (began, started) = mpsc::channel();
        let release = Arc::new(Barrier::new(2));
        let encoding = Arc::clone(&release);
        let (sent, copied) = mpsc::channel();
        let worker = Worker::start_with(Effects {
            prepare: Box::new(move |_, _, _| {
                began.send(()).unwrap();
                encoding.wait();
                fs::write(&encoded, b"complete")?;
                Ok(encoded.clone())
            }),
            copy: Box::new(move |path| {
                sent.send(path.to_owned()).unwrap();
                Ok(())
            }),
            revision: || REVISION.load(Ordering::Relaxed),
        })
        .unwrap();
        worker
            .request(Path::new("original.mp4"), Copy::Shareable, 25)
            .unwrap();
        started.recv_timeout(WAIT).unwrap();
        REVISION.fetch_add(1, Ordering::Relaxed);
        release.wait();
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let state = worker.mailbox.state.lock().unwrap();
            if state.active.is_none() {
                break;
            }
            drop(state);
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(copied.try_recv().is_err());
        assert!(!output.exists());
    }
}
