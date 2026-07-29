use std::{collections::BTreeMap, sync::Arc};

use pintail_types::{PrimaryKey, StoredRow};

#[derive(Default)]
pub(crate) struct Memtable {
    rows: Arc<BTreeMap<PrimaryKey, StoredRow>>,
    estimated_bytes: usize,
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
        true
    }

    pub(crate) fn snapshot(&self) -> Arc<BTreeMap<PrimaryKey, StoredRow>> {
        Arc::clone(&self.rows)
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub(crate) fn clear(&mut self) {
        self.rows = Arc::new(BTreeMap::new());
        self.estimated_bytes = 0;
    }
}
