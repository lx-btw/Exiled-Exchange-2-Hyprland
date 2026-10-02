//! `Client.txt` tail → `MAIN->CLIENT::game-log`. Ports
//! `main/src/host-files/GameLogWatcher.ts` (consumer: `OverlayWindow.vue` →
//! `client-log.ts::handleLine`, which feeds the leveling/XP widgets and the
//! `client-log-event` stream into `file_writer.rs`).
//!
//! Differences from upstream, all deliberate:
//! - Event-driven (inotify on the log's *directory*, filtered to the file's
//!   name) instead of `watchFile`'s 450 ms stat polling. Watching the
//!   directory is what lets rotation (rename + new file) and
//!   truncate/recreate be seen at all — a watch on the file itself dies with
//!   its inode. On a change the path is re-stat'ed: new inode → the old
//!   handle is drained, then the new file is read from the start; size below
//!   our offset → truncated, restart from 0.
//! - Chunks are cut at the last newline and the incomplete tail is carried
//!   into the next read, so a line is never split across two `game-log`
//!   events (upstream splits at the 64 KiB read boundary).
//! - Re-enabling after disabling with an unchanged path works (upstream's
//!   `_wantedPath` guard made that a silent no-op until the path changed).
//! - A `re-parse-log` that arrives while a read is running is dropped, as
//!   upstream's `isReading` guard does.
//!
//! The one non-event wait is backpressure while re-parsing a large file: the
//! shared broadcast channel is small and a lagging WS client *drops* events,
//! so the reader pauses briefly whenever the channel is backed up.

use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

use crate::file_writer::FileWriter;
use crate::logger::ee_log;

const READ_CHUNK: usize = 64 * 1024;
/// Pause reading while this many events sit unconsumed in the channel.
const BACKLOG_LIMIT: usize = 64;

/// Upstream's `POSSIBLE_PATH` for linux.
fn possible_paths() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    vec![
        home.join(
            ".wine/drive_c/Program Files (x86)/Grinding Gear Games/Path of Exile 2/logs/Client.txt",
        ),
        home.join(".local/share/Steam/steamapps/common/Path of Exile 2/logs/Client.txt"),
    ]
}

enum Guess {
    Found(PathBuf),
    /// two readable candidates (standalone + Steam) — upstream refuses to pick
    Ambiguous(Vec<PathBuf>),
    None,
}

fn guess_file_location(candidates: Vec<PathBuf>) -> Guess {
    let readable: Vec<_> = candidates
        .iter()
        .filter(|p| std::fs::File::open(p).is_ok())
        .cloned()
        .collect();
    match readable.len() {
        0 => Guess::None,
        1 => Guess::Found(readable.into_iter().next().unwrap()),
        _ => Guess::Ambiguous(candidates),
    }
}

#[derive(Default)]
pub struct GameLogWatcher {
    wanted_path: Option<String>,
    task: Option<JoinHandle<()>>,
    reparse: Arc<Notify>,
}

impl GameLogWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// `CLIENT->MAIN::re-parse-log`. `file_writer` flush happens in the task,
    /// right before re-reading from 0.
    pub fn reparse(&self) {
        if self.task.is_some() {
            self.reparse.notify_one();
        }
    }

    /// `HostConfig.clientLog` / `readClientLog`. Cheap and non-blocking
    /// (called under `Overlay`'s lock): file access happens in the task.
    pub fn restart(
        &mut self,
        log_file: &str,
        read_log: bool,
        events_tx: &broadcast::Sender<Value>,
        file_writer: &Arc<FileWriter>,
    ) {
        if !read_log {
            if let Some(task) = self.task.take() {
                task.abort();
                ee_log!("info [GameLogWatcher] disabled");
            }
            self.wanted_path = None;
            return;
        }
        if self.wanted_path.as_deref() == Some(log_file) {
            return;
        }
        self.wanted_path = Some(log_file.to_string());
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let path = (!log_file.is_empty()).then(|| PathBuf::from(log_file));
        let (tx, fw, reparse) = (events_tx.clone(), file_writer.clone(), self.reparse.clone());
        self.task = Some(tokio::spawn(async move {
            let path = match path {
                Some(p) => p,
                None => match tokio::task::spawn_blocking(|| guess_file_location(possible_paths()))
                    .await
                {
                    Ok(Guess::Found(p)) => p,
                    Ok(Guess::Ambiguous(all)) => {
                        ee_log!("error [GameLogWatcher] Found 2 log files, please enter one into settings:");
                        for p in all {
                            ee_log!(" {}", p.display());
                        }
                        return;
                    }
                    _ => {
                        ee_log!("error [GameLogWatcher] No log file found");
                        return;
                    }
                },
            };
            if let Err(err) = run(&path, &tx, &fw, &reparse).await {
                ee_log!("error [GameLogWatcher] Failed to watch file: {err}");
            }
        }));
    }
}

struct Tail {
    file: Arc<std::fs::File>,
    ino: (u64, u64),
    offset: u64,
    /// bytes after the last newline read so far
    carry: Vec<u8>,
}

fn open_tail(path: &Path, from_end: bool) -> io::Result<Tail> {
    let file = std::fs::File::open(path)?;
    let md = file.metadata()?;
    Ok(Tail {
        ino: (md.dev(), md.ino()),
        offset: if from_end { md.len() } else { 0 },
        carry: Vec::new(),
        file: Arc::new(file),
    })
}

async fn run(
    path: &Path,
    tx: &broadcast::Sender<Value>,
    file_writer: &Arc<FileWriter>,
    reparse: &Notify,
) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_owned()).unwrap_or_default();

    let mut current: Option<Tail> = Some(open_tail(path, true)?);
    let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)?;
    inotify::add_watch(
        &fd,
        dir,
        WatchFlags::MODIFY
            | WatchFlags::CREATE
            | WatchFlags::MOVED_TO
            | WatchFlags::MOVED_FROM
            | WatchFlags::DELETE
            | WatchFlags::ATTRIB,
    )?;
    let afd = AsyncFd::new(fd)?;
    ee_log!("info [GameLogWatcher] watching {}", path.display());

    loop {
        tokio::select! {
            guard = afd.readable() => {
                let mut guard = guard?;
                let mut relevant = false;
                let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
                let mut reader = inotify::Reader::new(afd.get_ref(), &mut buf);
                loop {
                    match reader.next() {
                        Ok(ev) => {
                            if ev.events().contains(ReadFlags::QUEUE_OVERFLOW)
                                || ev.file_name().is_some_and(|n| n.to_bytes() == name.as_encoded_bytes())
                            {
                                relevant = true;
                            }
                        }
                        Err(rustix::io::Errno::AGAIN) => {
                            guard.clear_ready();
                            break;
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
                if relevant {
                    sync(path, &mut current, tx).await;
                }
            }
            _ = reparse.notified() => {
                if let Some(t) = current.as_mut() {
                    file_writer.flush_client_log_file();
                    t.offset = 0;
                    t.carry.clear();
                    read_to_eof(t, tx).await;
                }
                // a request that came in while reading is dropped, like upstream's `isReading`
                let _ = reparse.notified().now_or_never();
            }
        }
    }
}

/// React to a change of `path`: rotation, truncation, or plain growth.
async fn sync(path: &Path, current: &mut Option<Tail>, tx: &broadcast::Sender<Value>) {
    let md = match std::fs::metadata(path) {
        Ok(md) => md,
        // gone (mid-rotation): finish the old handle, wait for the new file
        Err(_) => {
            if let Some(t) = current.as_mut() {
                read_to_eof(t, tx).await;
            }
            return;
        }
    };
    let ino = (md.dev(), md.ino());
    match current.as_mut() {
        Some(t) if t.ino == ino => {
            if md.len() < t.offset {
                ee_log!("info [GameLogWatcher] log truncated, reading from start");
                t.offset = 0;
                t.carry.clear();
            }
            read_to_eof(t, tx).await;
        }
        _ => {
            if let Some(old) = current.as_mut() {
                read_to_eof(old, tx).await;
                ee_log!("info [GameLogWatcher] log rotated, reading the new file from start");
            }
            match open_tail(path, false) {
                Ok(mut t) => {
                    read_to_eof(&mut t, tx).await;
                    *current = Some(t);
                }
                Err(err) => ee_log!("error [GameLogWatcher] Failed to reopen log: {err}"),
            }
        }
    }
}

async fn read_to_eof(tail: &mut Tail, tx: &broadcast::Sender<Value>) {
    loop {
        let (file, offset) = (tail.file.clone(), tail.offset);
        let res = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            file.read_at(&mut buf, offset).map(|n| {
                buf.truncate(n);
                buf
            })
        })
        .await;
        let chunk = match res {
            Ok(Ok(chunk)) if !chunk.is_empty() => chunk,
            Ok(Ok(_)) => return,
            Ok(Err(err)) => {
                ee_log!("error [GameLogWatcher] read failed: {err}");
                return;
            }
            Err(_) => return,
        };
        tail.offset += chunk.len() as u64;
        tail.carry.extend_from_slice(&chunk);
        let Some(last_nl) = tail.carry.iter().rposition(|&b| b == b'\n') else {
            continue;
        };
        let rest = tail.carry.split_off(last_nl + 1);
        let complete = std::mem::replace(&mut tail.carry, rest);
        let lines: Vec<String> = String::from_utf8_lossy(&complete)
            .split('\n')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        while tx.len() > BACKLOG_LIMIT {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let _ = tx.send(json!({
            "name": "MAIN->CLIENT::game-log",
            "payload": {"lines": lines},
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    async fn next_lines(rx: &mut broadcast::Receiver<Value>) -> Vec<String> {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no game-log event")
            .unwrap();
        assert_eq!(ev["name"], "MAIN->CLIENT::game-log");
        ev["payload"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn tails_growth_truncation_rotation_and_reparse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Client.txt");
        std::fs::write(&path, "old line\n").unwrap();

        let (tx, mut rx) = broadcast::channel(64);
        let fw = FileWriter::new();
        let reparse = Arc::new(Notify::new());
        let task = {
            let (path, tx, fw, reparse) = (path.clone(), tx.clone(), fw.clone(), reparse.clone());
            tokio::spawn(async move { run(&path, &tx, &fw, &reparse).await })
        };
        // the watch is established inside the task; wait until it logs nothing
        // observable, so poll by appending until the first event shows up
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let mut got = None;
        for _ in 0..50 {
            writeln!(f, "first").unwrap();
            if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                got = Some(ev);
                break;
            }
        }
        let ev = got.expect("watcher never saw the append");
        // existing content ("old line") is not replayed
        assert!(!ev.to_string().contains("old line"));
        while rx.try_recv().is_ok() {}

        // partial line is held back until its newline arrives
        write!(f, "  half").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(rx.try_recv().is_err());
        writeln!(f, " done  ").unwrap();
        assert_eq!(next_lines(&mut rx).await, ["half done"]);

        // truncation: restarts from 0
        std::fs::write(&path, "after truncate\n").unwrap();
        assert_eq!(next_lines(&mut rx).await, ["after truncate"]);

        // rotation: rename away, create anew
        std::fs::rename(&path, dir.path().join("Client.txt.1")).unwrap();
        std::fs::write(&path, "rotated a\nrotated b\n").unwrap();
        assert_eq!(next_lines(&mut rx).await, ["rotated a", "rotated b"]);

        // re-parse: whole file again
        reparse.notify_one();
        assert_eq!(next_lines(&mut rx).await, ["rotated a", "rotated b"]);

        task.abort();
    }

    #[test]
    fn ambiguous_guess_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        assert!(matches!(
            guess_file_location(vec![a.clone(), dir.path().join("none")]),
            Guess::Found(p) if p == a
        ));
        assert!(matches!(
            guess_file_location(vec![a, b]),
            Guess::Ambiguous(_)
        ));
        assert!(matches!(
            guess_file_location(vec![dir.path().join("none")]),
            Guess::None
        ));
    }
}
