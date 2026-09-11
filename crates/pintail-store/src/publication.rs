//! What the table writers in this process have published.
//!
//! A reader that keeps a table's snapshot has to learn when the table's
//! files change. Walking the directory answers that from any process, at a
//! `stat` per file on every check. A writer that holds the table's lock can
//! answer it for nothing: no other process can change those files while
//! the lock is held, and the writer records each change after making it.
//!
//! So each open [`TableStore`](crate::TableStore) registers its directory
//! here and moves the directory's generation strictly after every change to
//! the files a reader opens. [`published_generation`] hands a reader that
//! generation while a writer in this process holds the lock, and nothing
//! otherwise, when the reader has to walk the files as before.
//!
//! Generations come from one process-wide counter and are never reused. A
//! directory whose writer closes and reopens starts from a fresh value, so
//! a generation a reader recorded under one writer can never match one
//! handed out under the next.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Default)]
struct Entry {
    writers: usize,
    generation: u64,
}

static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Entry>>> = LazyLock::new(Mutex::default);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn registry() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Entry>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// The generation a writer in this process has published for the table at
/// `directory`, or `None` when no writer here holds that table.
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
        .filter(|entry| entry.writers > 0)
        .map(|entry| entry.generation)
}

/// Records a change made to table files without their writer: a directory
/// removed or replaced wholesale. Moves the generation of every table
/// registered at or below `path`, after the change.
///
/// Such changes are made with no writer open, when there is nothing
/// registered to move; this keeps a reader from trusting a writer that
/// somehow still is.
pub fn publish_changes_under(path: &Path) {
    let path = std::fs::canonicalize(path)
        .ok()
        .or_else(|| {
            let parent = std::fs::canonicalize(path.parent()?).ok()?;
            Some(parent.join(path.file_name()?))
        })
        .unwrap_or_else(|| path.to_path_buf());
    for (directory, entry) in registry().iter_mut() {
        if directory.starts_with(&path) {
            entry.generation = next_generation();
        }
    }
}

/// A writer's registration: held for as long as the writer holds the
/// table's lock, and released before the lock is.
pub(crate) struct Publisher {
    directory: Arc<Path>,
}

impl Publisher {
    /// Registers the writer that has just locked `directory`.
    pub(crate) fn register(directory: &Path) -> Self {
        let directory: Arc<Path> = Arc::from(directory);
        let mut registry = registry();
        let entry = registry.entry(directory.to_path_buf()).or_default();
        entry.writers += 1;
        entry.generation = next_generation();
        Self { directory }
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

    /// Follows the table to `directory` after its files moved there.
    pub(crate) fn relocate(&mut self, directory: &Path) {
        let mut registry = registry();
        release(&mut registry, &self.directory);
        let entry = registry.entry(directory.to_path_buf()).or_default();
        entry.writers += 1;
        entry.generation = next_generation();
        self.directory = Arc::from(directory);
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        release(&mut registry(), &self.directory);
    }
}

fn release(registry: &mut HashMap<PathBuf, Entry>, directory: &Path) {
    let Some(entry) = registry.get_mut(directory) else {
        return;
    };
    entry.writers = entry.writers.saturating_sub(1);
    entry.generation = next_generation();
    if entry.writers == 0 {
        registry.remove(directory);
    }
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

    #[test]
    fn a_generation_is_published_only_while_a_writer_holds_the_table() {
        let directory = Path::new("/publication-test/held");
        assert_eq!(published_generation(directory), None);
        let writer = Publisher::register(directory);
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
        let directory = Path::new("/publication-test/reopened");
        let first = Publisher::register(directory);
        let before = published_generation(directory).expect("registered");
        drop(first);
        let second = Publisher::register(directory);
        assert!(published_generation(directory).expect("registered") > before);
        drop(second);
    }

    #[test]
    fn relocation_and_outside_changes_move_the_generation() {
        let old = Path::new("/publication-test/old");
        let new = Path::new("/publication-test/new");
        let mut writer = Publisher::register(old);
        let before = published_generation(old).expect("registered");
        writer.relocate(new);
        assert_eq!(published_generation(old), None);
        let moved = published_generation(new).expect("followed the table");
        assert!(moved > before);
        publish_changes_under(Path::new("/publication-test"));
        assert!(published_generation(new).expect("registered") > moved);
        drop(writer);
        assert_eq!(published_generation(new), None);
    }
}
