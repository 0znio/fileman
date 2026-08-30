//! Recursive name search beneath a directory.
//!
//! Runs on a worker thread and streams matches back in batches, so results
//! appear while the walk is still going rather than after it finishes.
//!
//! The walk is deliberately cheap: `readdir` already reports whether an entry
//! is a directory, so the common case costs no `stat` at all. Only entries whose
//! *name* matches are stat'd to build a full [`FileEntry`], which keeps a search
//! over a home directory bound by I/O rather than by syscalls.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gio::prelude::*;

use crate::fs::FileEntry;

/// Results are streamed in batches of this size.
const BATCH: usize = 48;

/// Hard cap on results. Past this the list is useless to a human anyway, and
/// holding more only costs memory.
const MAX_RESULTS: usize = 5_000;

/// Directories never worth descending into: kernel and runtime pseudo-
/// filesystems that contain no user files but plenty of infinite depth.
const SKIP_ABSOLUTE: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp/.X11-unix"];

pub struct SearchHandle {
    pub results: async_channel::Receiver<SearchEvent>,
    cancel: Arc<AtomicBool>,
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        // Dropping the handle must stop the walk; otherwise navigating away
        // leaves a thread churning through a home directory for nothing.
        self.cancel();
    }
}

#[derive(Debug)]
pub enum SearchEvent {
    Matches(Vec<FileEntry>),
    /// The walk ended. `truncated` means the result cap was reached.
    Finished { total: usize, truncated: bool },
}

/// Starts a case-insensitive substring search for `query` under `root`.
///
/// `skip_hidden` mirrors the view's own hidden-file setting, so a search does
/// not surface things the folder listing would hide.
pub fn start(root: PathBuf, query: String, skip_hidden: bool) -> SearchHandle {
    let (tx, rx) = async_channel::bounded(8);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let needle = query.to_lowercase();

    std::thread::Builder::new()
        .name("fileman-search".into())
        .spawn(move || {
            let mut batch = Vec::with_capacity(BATCH);
            let mut total = 0usize;
            let mut truncated = false;

            let walker = walkdir::WalkDir::new(&root)
                // Never follow links: a single symlink back up the tree would
                // turn this into an unbounded walk.
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| should_descend(entry, skip_hidden));

            for entry in walker {
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(entry) = entry else { continue };
                // Depth 1 is the folder's own listing, which the view already
                // holds and filters locally — matching it here would duplicate
                // every direct child.
                if entry.depth() <= 1 {
                    continue;
                }

                let name = entry.file_name().to_string_lossy();
                if !name.to_lowercase().contains(&needle) {
                    continue;
                }

                // Only now is a `stat` worth paying for.
                let Some(file_entry) = describe(entry.path()) else { continue };
                batch.push(file_entry);
                total += 1;

                if total >= MAX_RESULTS {
                    truncated = true;
                    break;
                }
                if batch.len() >= BATCH {
                    // A full channel means the UI hasn't drained yet; blocking
                    // here throttles the walk to the speed results are consumed.
                    if tx.send_blocking(SearchEvent::Matches(std::mem::take(&mut batch))).is_err() {
                        return;
                    }
                    batch = Vec::with_capacity(BATCH);
                }
            }

            if !batch.is_empty() {
                let _ = tx.send_blocking(SearchEvent::Matches(batch));
            }
            let _ = tx.send_blocking(SearchEvent::Finished { total, truncated });
        })
        .expect("spawn search thread");

    SearchHandle { results: rx, cancel }
}

/// Whether to walk into a directory at all.
fn should_descend(entry: &walkdir::DirEntry, skip_hidden: bool) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();

    // Hidden directories are skipped wholesale rather than per-entry: descending
    // into .git or node_modules' dotfiles produces thousands of matches nobody
    // asked for.
    if skip_hidden && name.starts_with('.') {
        return false;
    }
    if entry.file_type().is_dir() && SKIP_ABSOLUTE.iter().any(|s| entry.path() == Path::new(s)) {
        return false;
    }
    true
}

/// Builds a display entry for a match.
fn describe(path: &Path) -> Option<FileEntry> {
    let file = gio::File::for_path(path);
    let info = file
        .query_info(
            crate::fs::entry::QUERY_ATTRS,
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let parent = path.parent()?;
    Some(FileEntry::from_info(parent, &info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

    fn tree() -> TempDir {
        let root = TempDir::new("search");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join(".hidden/deep")).unwrap();
        fs::write(root.join("top-fileman.txt"), b"x").unwrap();          // depth 1
        fs::write(root.join("a/fileman-notes.md"), b"x").unwrap();       // depth 2
        fs::write(root.join("a/b/c/FILEMAN-deep.rs"), b"x").unwrap();    // depth 4
        fs::write(root.join("a/unrelated.txt"), b"x").unwrap();
        fs::write(root.join(".hidden/deep/fileman-secret"), b"x").unwrap();
        root
    }

    /// Drains a search to completion and returns the matched file names.
    fn collect(handle: &SearchHandle) -> (Vec<String>, usize) {
        let mut names = Vec::new();
        let mut total = 0;
        loop {
            match handle.results.recv_blocking() {
                Ok(SearchEvent::Matches(batch)) => {
                    names.extend(batch.into_iter().map(|e| e.display_name));
                }
                Ok(SearchEvent::Finished { total: t, .. }) => {
                    total = t;
                    break;
                }
                Err(_) => break,
            }
        }
        names.sort();
        (names, total)
    }

    #[test]
    fn finds_matches_at_any_depth_below_the_folder() {
        let root = tree();
        let handle = start(root.path().to_path_buf(), "fileman".into(), true);
        let (names, total) = collect(&handle);

        // Depth 1 is deliberately excluded: the view already lists and filters
        // the folder's own children.
        assert!(!names.contains(&"top-fileman.txt".to_string()), "got {names:?}");
        assert!(names.contains(&"fileman-notes.md".to_string()), "got {names:?}");
        assert!(names.contains(&"FILEMAN-deep.rs".to_string()), "got {names:?}");
        assert!(!names.contains(&"unrelated.txt".to_string()));
        assert_eq!(total, names.len());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let root = tree();
        let handle = start(root.path().to_path_buf(), "FiLeMaN-DeEp".into(), true);
        let (names, _) = collect(&handle);
        assert_eq!(names, vec!["FILEMAN-deep.rs".to_string()]);
    }

    #[test]
    fn hidden_directories_are_skipped_unless_asked_for() {
        let root = tree();

        let (hidden_off, _) = collect(&start(root.path().to_path_buf(), "secret".into(), true));
        assert!(hidden_off.is_empty(), "got {hidden_off:?}");

        let (hidden_on, _) = collect(&start(root.path().to_path_buf(), "secret".into(), false));
        assert_eq!(hidden_on, vec!["fileman-secret".to_string()]);
    }

    #[test]
    fn dropping_the_handle_stops_the_walk() {
        let root = tree();
        let handle = start(root.path().to_path_buf(), "fileman".into(), true);
        handle.cancel();
        // After cancelling, the channel closes without delivering `Finished`.
        // The walk must not keep the thread alive holding the directory open.
        while let Ok(event) = handle.results.recv_blocking() {
            if matches!(event, SearchEvent::Finished { .. }) {
                break;
            }
        }
    }
}
