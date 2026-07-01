use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SeenMarkets {
    pub markets: BTreeSet<String>,
}

impl SeenMarkets {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
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
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
    }
}
