//! Watches the shared folder and publishes changes as [`Event`]s.
//!
//! `notify` events are only a hint that something changed. One file write
//! can produce many of them, and they differ by OS. So instead of
//! translating them, we wait for things to calm down, rescan the folder,
//! and diff the result against the last scan. That makes the events
//! correct no matter which raw events arrived.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::events::Event;
use crate::files::{self, FileEntry};
use crate::sanitize::is_partial;
use crate::server::AppState;

/// Wait this long without new events before rescanning.
const QUIET: Duration = Duration::from_millis(250);
/// But never longer than this after the first event, so a long copy
/// writing continuously can't postpone updates forever.
const MAX_DELAY: Duration = Duration::from_secs(1);
/// While a file is still being copied, check again this often.
const BUSY_RECHECK: Duration = Duration::from_secs(1);
/// A rescan now and then, in case Windows drops notifications on a busy folder.
const SAFETY_RESCAN: Duration = Duration::from_secs(30);
/// Polling interval when the folder can't be watched (some network drives).
const POLL: Duration = Duration::from_secs(2);

type Snapshot = HashMap<String, FileEntry>;

/// Takes the first snapshot and registers the watcher before returning, so
/// anything that changes afterwards is reported. The task stops on shutdown.
pub async fn start(state: AppState) -> JoinHandle<()> {
    let signal = Arc::new(Notify::new());
    let watcher = watch(&state.dir, signal.clone());
    if watcher.is_none() {
        tracing::warn!(
            "Can't watch the shared folder for changes; checking it every {} seconds instead.",
            POLL.as_secs()
        );
    }
    let known = snapshot(&state).await.unwrap_or_default();
    tokio::spawn(run(state, signal, watcher, known))
}

async fn run(
    state: AppState,
    signal: Arc<Notify>,
    watcher: Option<RecommendedWatcher>,
    mut known: Snapshot,
) {
    let idle = if watcher.is_some() {
        SAFETY_RESCAN
    } else {
        POLL
    };
    let mut shutdown = state.shutdown_signal();
    let mut busy = false;
    loop {
        let wait = if busy { BUSY_RECHECK } else { idle };
        tokio::select! {
            () = signal.notified() => settle(&signal).await,
            () = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => break,
        }
        let dir = state.dir.clone();
        let scan = match tokio::task::spawn_blocking(move || files::scan(&dir)).await {
            Ok(Ok(scan)) => scan,
            Ok(Err(e)) => {
                tracing::debug!("rescan failed: {e}");
                continue;
            }
            Err(e) => {
                tracing::debug!("rescan task failed: {e}");
                continue;
            }
        };
        busy = scan.busy > 0;
        let now: Snapshot = scan
            .files
            .into_iter()
            .map(|f| (f.name.clone(), f))
            .collect();
        for event in diff(&known, &now) {
            // No receivers just means no page is open.
            let _ = state.events.send(event);
        }
        known = now;
    }
    drop(watcher);
}

async fn snapshot(state: &AppState) -> Option<Snapshot> {
    let dir = state.dir.clone();
    let files = tokio::task::spawn_blocking(move || files::list(&dir))
        .await
        .ok()?
        .ok()?;
    Some(files.into_iter().map(|f| (f.name.clone(), f)).collect())
}

/// Returns once events stop for [`QUIET`], or [`MAX_DELAY`] after the first.
async fn settle(signal: &Notify) {
    let deadline = tokio::time::Instant::now() + MAX_DELAY;
    while tokio::time::Instant::now() < deadline {
        if tokio::time::timeout(QUIET, signal.notified())
            .await
            .is_err()
        {
            return;
        }
    }
}

fn watch(dir: &Path, signal: Arc<Notify>) -> Option<RecommendedWatcher> {
    let handler = move |res: notify::Result<notify::Event>| match res {
        Ok(event) if is_relevant(&event) => signal.notify_one(),
        Ok(_) => {}
        Err(e) => {
            // An error can mean lost events, so rescan to be safe.
            tracing::debug!("watch error: {e}");
            signal.notify_one();
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(w) => w,
        Err(e) => {
            tracing::debug!("could not create a watcher: {e}");
            return None;
        }
    };
    if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
        tracing::debug!("could not watch {}: {e}", dir.display());
        return None;
    }
    Some(watcher)
}

/// False for events that can't change the listing: activity on upload
/// temp files (which would otherwise rescan on every chunk), and files
/// being opened or read (our own downloads, on Linux).
pub fn is_relevant(event: &notify::Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    if let EventKind::Access(kind) = event.kind
        && kind != AccessKind::Close(AccessMode::Write)
    {
        return false;
    }
    event.paths.is_empty()
        || event.paths.iter().any(|p| {
            p.file_name()
                .is_none_or(|n| !is_partial(&n.to_string_lossy()))
        })
}

/// Events that turn `before` into `after`: removals first, then additions
/// and changes.
pub fn diff(before: &Snapshot, after: &Snapshot) -> Vec<Event> {
    let mut removed: Vec<&String> = before.keys().filter(|k| !after.contains_key(*k)).collect();
    removed.sort();
    let mut added: Vec<&FileEntry> = after
        .values()
        .filter(|f| before.get(&f.name) != Some(*f))
        .collect();
    added.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.name.cmp(&b.name))
    });

    removed
        .into_iter()
        .map(|name| Event::FileRemoved { name: name.clone() })
        .chain(added.into_iter().map(|f| Event::FileAdded {
            name: f.name.clone(),
            size: f.size,
            modified: f.modified,
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind};
    use std::path::PathBuf;

    fn entry(name: &str, size: u64, modified: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            size,
            modified,
        }
    }

    fn snap(entries: &[FileEntry]) -> Snapshot {
        entries
            .iter()
            .map(|f| (f.name.clone(), f.clone()))
            .collect()
    }

    #[test]
    fn diff_reports_adds_removes_and_changes() {
        let before = snap(&[
            entry("keep", 1, 1),
            entry("gone", 1, 1),
            entry("grows", 1, 1),
        ]);
        let after = snap(&[
            entry("keep", 1, 1),
            entry("grows", 9, 2),
            entry("new", 5, 3),
        ]);
        assert_eq!(
            diff(&before, &after),
            [
                Event::FileRemoved {
                    name: "gone".into()
                },
                Event::FileAdded {
                    name: "grows".into(),
                    size: 9,
                    modified: 2
                },
                Event::FileAdded {
                    name: "new".into(),
                    size: 5,
                    modified: 3
                },
            ]
        );
    }

    #[test]
    fn diff_of_identical_snapshots_is_empty() {
        let s = snap(&[entry("a", 1, 1)]);
        assert!(diff(&s, &s).is_empty());
        assert!(diff(&Snapshot::new(), &Snapshot::new()).is_empty());
    }

    #[test]
    fn a_rename_is_a_removal_and_an_addition() {
        let before = snap(&[entry("old.txt", 3, 1)]);
        let after = snap(&[entry("new.txt", 3, 1)]);
        let events = diff(&before, &after);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::FileRemoved { name } if name == "old.txt"));
        assert!(matches!(&events[1], Event::FileAdded { name, .. } if name == "new.txt"));
    }

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        let mut e = notify::Event::new(kind);
        e.paths = paths.iter().map(PathBuf::from).collect();
        e
    }

    #[test]
    fn upload_temp_files_and_reads_are_ignored() {
        let write = EventKind::Modify(ModifyKind::Any);
        assert!(!is_relevant(&event(
            write,
            &["/s/a.mp4.beam-1234abcd.part"]
        )));
        assert!(is_relevant(&event(write, &["/s/a.mp4"])));
        // Rename from .part to the final name touches a real name.
        assert!(is_relevant(&event(
            EventKind::Modify(ModifyKind::Any),
            &["/s/a.mp4.beam-1234abcd.part", "/s/a.mp4"]
        )));
        assert!(is_relevant(&event(
            EventKind::Create(CreateKind::File),
            &["/s/b.txt"]
        )));
        assert!(!is_relevant(&event(
            EventKind::Access(AccessKind::Read),
            &["/s/b.txt"]
        )));
        assert!(is_relevant(&event(
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            &["/s/b.txt"]
        )));
        assert!(is_relevant(&event(EventKind::Other, &[])));
    }
}
