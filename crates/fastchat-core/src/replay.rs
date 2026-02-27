use std::{fs::File, io::{BufRead, BufReader}, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::ChatMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub at: DateTime<Utc>,
    pub message: ChatMessage,
}

#[derive(Debug, Clone, Copy)]
pub struct ReplayScenario {
    pub sustained_msgs_per_sec: u32,
    pub burst_msgs_per_sec: u32,
}

impl Default for ReplayScenario {
    fn default() -> Self {
        Self {
            sustained_msgs_per_sec: 50,
            burst_msgs_per_sec: 200,
        }
    }
}

#[derive(Debug)]
pub struct ReplaySource {
    records: Vec<ReplayRecord>,
}

impl ReplaySource {
    pub fn from_jsonl(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("failed opening {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record: ReplayRecord = serde_json::from_str(&line)
                .with_context(|| format!("failed parsing replay line in {}", path.display()))?;
            records.push(record);
        }
        Ok(Self { records })
    }

    pub fn records(&self) -> &[ReplayRecord] {
        &self.records
    }
}
