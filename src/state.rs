use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use kuest_client_sdk::types::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn load_or_default<T: Default + DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(load_json(path)?.unwrap_or_default())
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let raw = serde_json::to_string_pretty(value)?;
    fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SeenMarkets {
    pub markets: BTreeSet<String>,
}

impl SeenMarkets {
    pub fn load(path: &Path) -> Result<Self> {
        load_or_default(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_pretty(path, self)
    }

    pub fn mark_new(&mut self, market_key: String) -> bool {
        self.markets.insert(market_key)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PauseState {
    pub reason: String,
    pub created_at_unix_secs: u64,
}

impl PauseState {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        load_json(path)
    }

    pub fn save_reason(path: &Path, reason: impl Into<String>) -> Result<Self> {
        let pause = Self {
            reason: reason.into(),
            created_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        pause.save(path)?;
        Ok(pause)
    }

    pub fn clear(path: &Path) -> Result<bool> {
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove {}", path.display()))
            }
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        write_json_pretty(path, self)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FillRecord {
    pub id: String,
    pub token_id: String,
    pub market: String,
    pub side: String,
    pub size: Decimal,
    pub price: Decimal,
    pub status: String,
    pub matched_at_unix_secs: i64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FillLedger {
    pub trades: BTreeMap<String, FillRecord>,
}

impl FillLedger {
    pub fn load(path: &Path) -> Result<Self> {
        load_or_default(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_pretty(path, self)
    }

    pub fn upsert(&mut self, record: FillRecord) -> bool {
        let changed = self.trades.get(&record.id) != Some(&record);
        if changed {
            self.trades.insert(record.id.clone(), record);
        }
        changed
    }

    pub fn records_for_token<'a>(&'a self, token_id: &str) -> Vec<&'a FillRecord> {
        let mut records = self
            .trades
            .values()
            .filter(|record| record.token_id == token_id)
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.matched_at_unix_secs
                .cmp(&right.matched_at_unix_secs)
                .then_with(|| left.id.cmp(&right.id))
        });
        records
    }

    pub fn latest_matched_at_unix_secs(&self, token_id: &str) -> Option<i64> {
        self.trades
            .values()
            .filter(|record| record.token_id == token_id)
            .map(|record| record.matched_at_unix_secs)
            .max()
    }

    pub fn prune_to_max_records(&mut self, max_records: usize) -> bool {
        if self.trades.len() <= max_records {
            return false;
        }

        let mut records = self
            .trades
            .values()
            .map(|record| (record.matched_at_unix_secs, record.id.clone()))
            .collect::<Vec<_>>();
        records.sort();

        for (_, id) in records.into_iter().take(self.trades.len() - max_records) {
            self.trades.remove(&id);
        }

        true
    }
}
