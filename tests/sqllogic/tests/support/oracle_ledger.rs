//! Reviewed known failures: fixed-corpus cases where Pintail diverges from
//! `MySQL` through a limitation recorded in `docs/limitations.md`. A listed
//! case that fails is reported as a warning rather than a failure. A listed
//! case that passes is stale and fails the run, as does an entry that names
//! no corpus case, disagrees with the case's SQL, or cites a limitation the
//! document does not record.

use std::collections::BTreeMap;

const LEDGER: &str = include_str!("oracle_known_failures.json");
const LIMITATIONS: &str = include_str!("../../../../docs/limitations.md");

pub struct KnownFailure {
    pub sql: String,
    pub limitation: String,
    pub reason: String,
}

/// The ledger by case id, validated against `docs/limitations.md`.
pub fn load() -> Result<BTreeMap<String, KnownFailure>, String> {
    let document: serde_json::Value =
        serde_json::from_str(LEDGER).map_err(|error| format!("known-failure ledger: {error}"))?;
    let entries = document["entries"]
        .as_array()
        .ok_or("known-failure ledger has no entries array")?;
    let mut ledger = BTreeMap::new();
    for entry in entries {
        let field = |name: &str| {
            entry[name]
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("known-failure entry lacks {name}: {entry}"))
        };
        let id = field("id")?;
        let known = KnownFailure {
            sql: field("sql")?,
            limitation: field("limitation")?,
            reason: field("reason")?,
        };
        if !LIMITATIONS.contains(&known.limitation) {
            return Err(format!(
                "known failure {id} cites a limitation docs/limitations.md does not record: {}",
                known.limitation
            ));
        }
        if ledger.insert(id.clone(), known).is_some() {
            return Err(format!("known failure {id} is listed twice"));
        }
    }
    Ok(ledger)
}
