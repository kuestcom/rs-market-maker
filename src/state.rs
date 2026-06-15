use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

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
