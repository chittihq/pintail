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

use crate::{StoreError, store::WRITER_LOCK_FILE};

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
/// Moves whenever the set of table directories this process knows of may
/// have changed: see [`directory_epoch`].
static DIRECTORY_EPOCH: AtomicU64 = AtomicU64::new(1);
/// Moves whenever anything a table's published generation says may have
/// changed: see [`publication_epoch`].
static PUBLICATION_EPOCH: AtomicU64 = AtomicU64::new(1);

/// The registry, to change it. Moves [`publication_epoch`] while the lock is
/// held, so whoever reads the epoch and then the registry either sees this
/// change or holds an epoch from before it.
fn registry() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Entry>> {
    let registry = registry_read();
    PUBLICATION_EPOCH.fetch_add(1, Ordering::AcqRel);
    registry
}

/// The registry, to read it.
fn registry_read() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Entry>> {
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

/// Whether this process has declared itself the only writer of its data
/// directory ([`retain_writer_locks`]).
#[must_use]
pub fn writer_locks_retained() -> bool {
    RETAIN_LOCKS.load(Ordering::Relaxed)
}

/// A number that moves whenever a table directory this process had not
/// seen is claimed by a writer or leased for a reader, a table moves to
/// another directory, or directories are removed or replaced without their
/// writers ([`publish_changes_under`]).
///
/// In a process that is its data directory's only writer, a table directory
/// comes to hold rows only through a writer opened here, and goes away only
/// through a caller that publishes the removal. So a listing of a tables
/// directory taken after reading this number is still the whole set of
/// tables for as long as the number has not moved - without asking the file
/// system on every query whether the directory changed. A listing may also
/// be retaken when nothing changed; it is never kept past a change.
#[must_use]
pub fn directory_epoch() -> u64 {
    DIRECTORY_EPOCH.load(Ordering::Acquire)
}

/// A number that moves whenever the registry of published generations is
/// changed in any way: a writer claims or lets go of a table, a change is
/// published, a lease is taken, adopted or dropped.
///
/// A reader that takes this number and then reads every table's generation
/// has, for as long as the number has not moved, what reading them all
/// again would give it - which is one load instead of a lookup per table
/// for every query. The number also moves for changes that leave every
/// generation as it was; it never stays for one that does not.
#[must_use]
pub fn publication_epoch() -> u64 {
    PUBLICATION_EPOCH.load(Ordering::Acquire)
}

fn directories_changed() {
    DIRECTORY_EPOCH.fetch_add(1, Ordering::AcqRel);
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
    registry_read()
        .get(directory)
        .filter(|entry| entry.writers > 0 || entry.lease.is_some())
        .map(|entry| entry.generation)
}

/// Proves the table at `directory` current for a reader when no writer in
/// this process has opened it yet, by taking the lease its writer would
/// have left: the generation that answer carries then holds until a writer
/// here adopts the lease and changes the files.
///
/// Leases used to come only from a closed writer, so after a restart every
/// table replication had not yet written was walked on every query - a
/// `stat` per file of every such table, which on a replica of a hundred
/// quiet tables was most of what a trivial query cost.
///
/// Only a process that retains writer locks leases, and only a table whose
/// lock file is already there and free: a reader creates no file, and a
/// lock another process or writer holds answers `None`, which sends the
/// caller to the files as before. `directory` is spelled as for
/// [`published_generation`].
#[must_use]
pub fn lease_unwritten_table(directory: &Path) -> Option<u64> {
    if !RETAIN_LOCKS.load(Ordering::Relaxed) {
        return None;
    }
    lease_unwritten(directory)
}

fn lease_unwritten(directory: &Path) -> Option<u64> {
    // Held across the lock attempt, so a writer's claim either sees the
    // lease or takes the lock first and makes this attempt fail.
    let mut registry = registry_read();
    if let Some(entry) = registry.get(directory) {
        return (entry.writers > 0 || entry.lease.is_some()).then_some(entry.generation);
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join(WRITER_LOCK_FILE))
        .ok()?;
    fs2::FileExt::try_lock_exclusive(&lock).ok()?;
    let generation = next_generation();
    registry.insert(
        directory.to_path_buf(),
        Entry {
            writers: 0,
            generation,
            lease: Some((lock, generation)),
        },
    );
    PUBLICATION_EPOCH.fetch_add(1, Ordering::AcqRel);
    directories_changed();
    evict_leases(&mut registry);
    registry
        .get(directory)
        .and_then(|entry| entry.lease.as_ref().map(|_| entry.generation))
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
    directories_changed();
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
        let locked = match lock() {
            Ok(locked) => locked,
            // A reader may have leased the table between the look above and
            // the lock; the lease is registered before its lock is let go
            // of anywhere, so it is there to adopt by now.
            Err(error) => {
                let mut registry = registry();
                let Some(entry) = registry.get_mut(directory.as_ref()) else {
                    return Err(error);
                };
                let Some((leased, _)) = entry.lease.take() else {
                    return Err(error);
                };
                if !same_file(&leased, lock_path) {
                    if entry.writers == 0 {
                        registry.remove(directory.as_ref());
                    }
                    return Err(error);
                }
                entry.writers += 1;
                return Ok(Self {
                    directory,
                    lock: Some(leased),
                });
            }
        };
        let mut registry = registry();
        if !registry.contains_key(directory.as_ref()) {
            directories_changed();
        }
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
        directories_changed();
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

    /// The epoch may also move for another test's tables - it is the
    /// process's - so only that it does move is asserted.
    #[test]
    fn the_directory_epoch_moves_whenever_the_set_of_table_directories_may_have() {
        let scratch = tempfile::tempdir().expect("scratch");
        let first = scratch.path().join("first");
        std::fs::create_dir(&first).expect("first");

        let before = directory_epoch();
        let mut writer = claim(&first);
        let claimed = directory_epoch();
        assert!(claimed > before, "a writer on a directory not seen before");

        let second = scratch.path().join("second");
        std::fs::rename(&first, &second).expect("move");
        writer.relocate(&second);
        let moved = directory_epoch();
        assert!(moved > claimed, "a table moved to another directory");
        drop(writer);

        let leased = scratch.path().join("leased");
        std::fs::create_dir(&leased).expect("leased");
        File::create(leased.join(WRITER_LOCK_FILE)).expect("lock file");
        lease_unwritten(&leased).expect("leased");
        let seen = directory_epoch();
        assert!(seen > moved, "a directory first seen by a reader");

        publish_changes_under(scratch.path());
        assert!(directory_epoch() > seen, "directories removed or replaced");
    }

    /// As with the directory epoch, only that it moves is asserted: the
    /// number is the process's.
    #[test]
    fn the_publication_epoch_moves_with_every_change_to_what_is_published() {
        let scratch = tempfile::tempdir().expect("scratch");
        let directory = scratch.path();
        let mut epoch = publication_epoch();
        let mut moved = |what: &str| {
            let now = publication_epoch();
            assert!(now > epoch, "{what}");
            epoch = now;
        };
        let writer = claim(directory);
        moved("a writer claims the table");
        drop(writer.publishing());
        moved("a change is published");
        drop(writer);
        moved("the writer lets go");
        let writer = claim(directory);
        moved("another writer claims it");
        drop(writer);
        publish_changes_under(directory);
        moved("the directory is replaced");

        let leased = scratch.path().join("leased");
        std::fs::create_dir(&leased).expect("leased");
        File::create(leased.join(WRITER_LOCK_FILE)).expect("lock file");
        lease_unwritten(&leased).expect("leased");
        moved("a reader leases a table");
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
    fn a_reader_leases_a_free_table_and_its_writer_adopts_the_lease() {
        let scratch = tempfile::tempdir().expect("scratch");
        let directory = scratch.path();
        assert_eq!(lease_unwritten(directory), None, "no lock file, no lease");
        let lock_path = directory.join(WRITER_LOCK_FILE);
        let held = File::create(&lock_path).expect("lock file");
        fs2::FileExt::try_lock_exclusive(&held).expect("held elsewhere");
        assert_eq!(
            lease_unwritten(directory),
            None,
            "a held lock is not leased"
        );
        fs2::FileExt::unlock(&held).expect("release");

        let leased = lease_unwritten(directory).expect("leased");
        assert_eq!(published_generation(directory), Some(leased));
        assert_eq!(lease_unwritten(directory), Some(leased), "one lease");
        assert!(
            fs2::FileExt::try_lock_exclusive(&held).is_err(),
            "the lease holds the table's lock"
        );
        let writer = Publisher::claim(directory, &lock_path, || {
            Err(StoreError::io(
                "lock",
                std::io::ErrorKind::WouldBlock.into(),
            ))
        })
        .expect("the writer adopts the lease");
        assert_eq!(published_generation(directory), Some(leased));
        drop(writer.publishing());
        assert!(published_generation(directory).expect("published") > leased);
        drop(writer);
        assert_eq!(published_generation(directory), None);
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
