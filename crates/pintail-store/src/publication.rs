//! What the table writers in this process have published.
//!
//! A reader that keeps a table's snapshot has to learn when the table's
//! files change. Walking the directory answers that from any process, at a
//! `stat` per file on every check. A process that holds the table's writer
//! lock can answer it for nothing: no other process can change those files
//! while the lock is held, and every change this process makes goes through
//! a writer that records it after making it.
//!
//! So each open [`TableStore`](crate::TableStore) claims its directory here
//! and moves the directory's generation strictly after every change to the
//! files a reader opens. [`published_generation`] hands a reader that
//! generation while this process holds the table's lock, and nothing
//! otherwise, when the reader has to walk the files as before.
//!
//! By default the lock goes with the writer that took it. Replication opens
//! its writers for one cycle at a time, though, and a table between cycles
//! would be walked again on every query. A process that is its data
//! directory's only writer - the server - calls [`retain_writer_locks`]:
//! the lock then stays with the process as a lease when a writer closes,
//! the next writer in the process adopts it, and the generation stays good
//! between writers because nothing can have changed the files.
//!
//! Generations come from one process-wide counter and are never reused, so
//! a generation a reader recorded can never be mistaken for one handed out
//! after the table's lock was let go and taken again.

use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::StoreError;

/// Leases one process keeps at most. Each is an open file; past this the
/// least recently released goes, and its table is walked until a writer
/// takes it again.
const MAX_LEASES: usize = 512;

#[derive(Default)]
struct Entry {
    writers: usize,
    generation: u64,
    /// The table's writer lock, kept by the process while no writer is
    /// open, with when it was released for choosing which lease goes first.
    lease: Option<(File, u64)>,
}

static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Entry>>> = LazyLock::new(Mutex::default);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static RETAIN_LOCKS: AtomicBool = AtomicBool::new(false);

fn registry() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Entry>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Keeps every table's writer lock with this process when its writer
/// closes, so a table between writers is still proven current by its
/// generation. For a process that is the only writer of its data
/// directory: another process can no longer open a writer on a table this
/// one has written, until this one exits.
pub fn retain_writer_locks() {
    RETAIN_LOCKS.store(true, Ordering::Relaxed);
}

/// The generation this process has published for the table at `directory`,
/// or `None` when this process does not hold that table's lock.
///
/// `directory` must be spelled the way the writer opened it: canonical, as
/// [`TableStore::open`](crate::TableStore::open) records it. Any other
/// spelling finds nothing, which sends the caller to the files - correct,
/// only slower.
///
/// A reader takes the generation before it opens the table, and the table
/// is unchanged for as long as the generation is. A change in progress when
/// the generation was taken moves it once the change is complete.
#[must_use]
pub fn published_generation(directory: &Path) -> Option<u64> {
    registry()
        .get(directory)
        .filter(|entry| entry.writers > 0 || entry.lease.is_some())
        .map(|entry| entry.generation)
}

/// Records a change made to table files without their writer: a directory
/// removed or replaced wholesale. Moves the generation of every table at or
/// below `path`, and lets go of the lease on any that has no writer open,
/// since the files it vouched for are gone.
pub fn publish_changes_under(path: &Path) {
    let path = std::fs::canonicalize(path)
        .ok()
        .or_else(|| {
            let parent = std::fs::canonicalize(path.parent()?).ok()?;
            Some(parent.join(path.file_name()?))
        })
        .unwrap_or_else(|| path.to_path_buf());
    registry().retain(|directory, entry| {
        if !directory.starts_with(&path) {
            return true;
        }
        entry.generation = next_generation();
        entry.writers > 0
    });
}

/// A writer's claim on one table: it holds the table's writer lock for as
/// long as the writer is open.
pub(crate) struct Publisher {
    directory: Arc<Path>,
    lock: Option<File>,
}

impl Publisher {
    /// Claims the table at `directory` for a writer: adopts the process's
    /// lease when it holds one on the same lock file, and otherwise takes
    /// the lock with `lock`.
    ///
    /// An adopted lease keeps the generation - nothing changed the files
    /// while the process held them. A lease whose lock file was replaced
    /// underneath it (the directory recreated) vouches for nothing and is
    /// dropped; the fresh lock starts a fresh generation.
    pub(crate) fn claim(
        directory: &Path,
        lock_path: &Path,
        lock: impl FnOnce() -> Result<File, StoreError>,
    ) -> Result<Self, StoreError> {
        let directory: Arc<Path> = Arc::from(directory);
        {
            let mut registry = registry();
            if let Some(entry) = registry.get_mut(directory.as_ref())
                && let Some((leased, _)) = entry.lease.take()
            {
                if same_file(&leased, lock_path) {
                    entry.writers += 1;
                    return Ok(Self {
                        directory,
                        lock: Some(leased),
                    });
                }
                if entry.writers == 0 {
                    registry.remove(directory.as_ref());
                }
            }
        }
        let locked = lock()?;
        let mut registry = registry();
        let entry = registry.entry(directory.to_path_buf()).or_default();
        entry.writers += 1;
        entry.generation = next_generation();
        Ok(Self {
            directory,
            lock: Some(locked),
        })
    }

    /// A guard that publishes when it goes out of scope, however the scope
    /// ends. Take it before the first change to the files, so the change is
    /// published after it lands - including a change an error or a panic
    /// interrupted part-way.
    pub(crate) fn publishing(&self) -> Publishing {
        Publishing {
            directory: Some(Arc::clone(&self.directory)),
        }
    }

    /// Follows the table to `directory` after its files, lock file included,
    /// moved there.
    pub(crate) fn relocate(&mut self, directory: &Path) {
        let mut registry = registry();
        if let Some(entry) = registry.get_mut(self.directory.as_ref()) {
            entry.writers = entry.writers.saturating_sub(1);
            entry.generation = next_generation();
            if entry.writers == 0 {
                registry.remove(self.directory.as_ref());
            }
        }
        let entry = registry.entry(directory.to_path_buf()).or_default();
        entry.writers += 1;
        entry.generation = next_generation();
        self.directory = Arc::from(directory);
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        let mut registry = registry();
        let Some(entry) = registry.get_mut(self.directory.as_ref()) else {
            return;
        };
        entry.writers = entry.writers.saturating_sub(1);
        if entry.writers > 0 {
            return;
        }
        match self.lock.take() {
            // Closing changes no file, so the generation stands.
            Some(lock) if RETAIN_LOCKS.load(Ordering::Relaxed) => {
                entry.lease = Some((lock, next_generation()));
                evict_leases(&mut registry);
            }
            _ => {
                entry.generation = next_generation();
                registry.remove(self.directory.as_ref());
            }
        }
    }
}

/// Lets go of the leases released longest ago until at most [`MAX_LEASES`]
/// remain.
fn evict_leases(registry: &mut HashMap<PathBuf, Entry>) {
    let leased = registry
        .values()
        .filter(|entry| entry.lease.is_some())
        .count();
    if leased <= MAX_LEASES {
        return;
    }
    let mut released = registry
        .iter()
        .filter_map(|(directory, entry)| {
            let (_, since) = entry.lease.as_ref()?;
            (entry.writers == 0).then(|| (*since, directory.clone()))
        })
        .collect::<Vec<_>>();
    released.sort_unstable();
    for (_, directory) in released.into_iter().take(leased - MAX_LEASES) {
        registry.remove(&directory);
    }
}

/// Whether `file` is still the file at `path`.
#[cfg(unix)]
fn same_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(held), Ok(current)) => held.dev() == current.dev() && held.ino() == current.ino(),
        _ => false,
    }
}

/// Without file identities there is no telling a replaced lock file from
/// the original, so a lease is never adopted.
#[cfg(not(unix))]
fn same_file(_: &File, _: &Path) -> bool {
    false
}

/// Publishes a change to one table's files when dropped.
pub(crate) struct Publishing {
    directory: Option<Arc<Path>>,
}

impl Publishing {
    /// Ends the scope without publishing: nothing changed after all.
    pub(crate) fn unchanged(mut self) {
        self.directory = None;
    }
}

impl Drop for Publishing {
    fn drop(&mut self) {
        if let Some(directory) = &self.directory
            && let Some(entry) = registry().get_mut(directory.as_ref())
        {
            entry.generation = next_generation();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(directory: &Path) -> Publisher {
        let lock_path = directory.join(".writer.lock");
        Publisher::claim(directory, &lock_path, || {
            File::create(&lock_path).map_err(|error| StoreError::io("create test lock", error))
        })
        .expect("claim")
    }

    #[test]
    fn a_generation_is_published_only_while_a_writer_holds_the_table() {
        let scratch = tempfile::tempdir().expect("scratch");
        let directory = scratch.path();
        assert_eq!(published_generation(directory), None);
        let writer = claim(directory);
        let opened = published_generation(directory).expect("registered");
        {
            let _change = writer.publishing();
            assert_eq!(
                published_generation(directory),
                Some(opened),
                "a change in progress is not yet published"
            );
        }
        let changed = published_generation(directory).expect("registered");
        assert!(changed > opened);
        writer.publishing().unchanged();
        assert_eq!(published_generation(directory), Some(changed));
        drop(writer);
        assert_eq!(published_generation(directory), None);
    }

    #[test]
    fn a_reopened_writer_never_repeats_a_generation() {
        let scratch = tempfile::tempdir().expect("scratch");
        let first = claim(scratch.path());
        let before = published_generation(scratch.path()).expect("registered");
        drop(first);
        let second = claim(scratch.path());
        assert!(published_generation(scratch.path()).expect("registered") > before);
        drop(second);
    }

    #[test]
    fn relocation_and_outside_changes_move_the_generation() {
        let scratch = tempfile::tempdir().expect("scratch");
        let old = scratch.path().join("old");
        let new = scratch.path().join("new");
        std::fs::create_dir_all(&old).expect("old");
        let mut writer = claim(&old);
        let before = published_generation(&old).expect("registered");
        writer.relocate(&new);
        assert_eq!(published_generation(&old), None);
        let moved = published_generation(&new).expect("followed the table");
        assert!(moved > before);
        publish_changes_under(scratch.path());
        assert!(published_generation(&new).expect("registered") > moved);
        drop(writer);
        assert_eq!(published_generation(&new), None);
    }
}
