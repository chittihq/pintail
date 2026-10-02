use std::{collections::BTreeMap, sync::Arc};

use pintail_types::{PrimaryKey, StoredRow};

#[derive(Default)]
pub(crate) struct Memtable {
    rows: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    estimated_bytes: usize,
    /// The lowest version applied since the memtable was last cleared: no
    /// row it holds is older. Replacing a row can leave this below the
    /// oldest one still held, never above it.
    oldest_version: Option<u64>,
    /// The highest version applied since the memtable was last cleared.
    newest_version: Option<u64>,
    /// The image scans build of the rows as they are now; replaced when a
    /// write changes rows a scan has read through it or may yet.
    image: Arc<crate::store::MemtableImage>,
}

impl Memtable {
    pub(crate) fn apply(&mut self, row: &StoredRow) -> bool {
        self.insert(row.clone())
    }

    /// [`Self::apply`] for a row the caller is done with: the memtable
    /// keeps the row itself rather than a copy of it.
    pub(crate) fn insert(&mut self, row: StoredRow) -> bool {
        let rows = Arc::make_mut(&mut self.rows);
        let version = row.version();
        let bytes = row.estimated_bytes();
        match rows.entry(row.key().clone()) {
            std::collections::btree_map::Entry::Occupied(mut current) => {
                if current.get().version() > version {
                    return false;
                }
                let previous = current.insert(row);
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_sub(previous.estimated_bytes());
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(row);
            }
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(bytes);
        if Arc::strong_count(&self.image) > 1 || self.image.is_built() {
            self.image = Arc::default();
        }
        self.oldest_version = Some(
            self.oldest_version
                .map_or(version, |oldest| oldest.min(version)),
        );
        self.newest_version = Some(
            self.newest_version
                .map_or(version, |newest| newest.max(version)),
        );
        true
    }

    /// The rows in key order, for a flush. They are moved out when no scan
    /// shares the map, and the memtable then answers as empty until it is
    /// cleared or the rows are handed back with [`Self::restore`]; a shared
    /// map is copied and left in place.
    pub(crate) fn take_rows(&mut self) -> Vec<StoredRow> {
        match Arc::get_mut(&mut self.rows) {
            Some(rows) => std::mem::take(rows).into_values().collect(),
            None => self.rows.values().cloned().collect(),
        }
    }

    /// Puts back rows [`Self::take_rows`] moved out, after a flush that did
    /// not finish. Nothing else about the memtable changed in between.
    pub(crate) fn restore(&mut self, rows: Vec<StoredRow>) {
        if self.rows.is_empty() {
            self.rows = Arc::new(
                rows.into_iter()
                    .map(|row| (row.key().clone(), row))
                    .collect(),
            );
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<BTreeMap<PrimaryKey, StoredRow>> {
        Arc::clone(&self.rows)
    }

    /// The image of the rows [`Self::snapshot`] hands out now.
    pub(crate) fn image(&self) -> Arc<crate::store::MemtableImage> {
        Arc::clone(&self.image)
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub(crate) fn oldest_version(&self) -> Option<u64> {
        self.oldest_version
    }

    pub(crate) fn newest_version(&self) -> Option<u64> {
        self.newest_version
    }

    pub(crate) fn clear(&mut self) {
        self.rows = Arc::new(BTreeMap::new());
        self.image = Arc::default();
        self.estimated_bytes = 0;
        self.oldest_version = None;
        self.newest_version = None;
    }
}
