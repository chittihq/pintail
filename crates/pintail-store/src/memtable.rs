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
    /// The image scans build of the rows as they are now; replaced when a
    /// write changes rows a scan has read through it or may yet.
    image: Arc<crate::store::MemtableImage>,
}

impl Memtable {
    pub(crate) fn apply(&mut self, row: &StoredRow) -> bool {
        let rows = Arc::make_mut(&mut self.rows);
        if rows
            .get(row.key())
            .is_some_and(|current| current.version() > row.version())
        {
            return false;
        }

        if let Some(previous) = rows.insert(row.key().clone(), row.clone()) {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(previous.estimated_bytes());
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(row.estimated_bytes());
        if Arc::strong_count(&self.image) > 1 || self.image.is_built() {
            self.image = Arc::default();
        }
        self.oldest_version = Some(
            self.oldest_version
                .map_or(row.version(), |oldest| oldest.min(row.version())),
        );
        true
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

    pub(crate) fn clear(&mut self) {
        self.rows = Arc::new(BTreeMap::new());
        self.image = Arc::default();
        self.estimated_bytes = 0;
        self.oldest_version = None;
    }
}
