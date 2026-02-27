use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::{config::AppPaths, model::ChatMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogRecord {
    pub ts: DateTime<Utc>,
    pub channel_login: String,
    pub message: ChatMessage,
}

impl BacklogRecord {
    pub fn from_message(message: ChatMessage) -> Self {
        Self {
            ts: message.timestamp,
            channel_login: message.channel_login.clone(),
            message,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BacklogRetention {
    pub max_days: u32,
    pub max_total_bytes: u64,
}

impl Default for BacklogRetention {
    fn default() -> Self {
        Self {
            max_days: 7,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct BacklogWriter {
    tx: Sender<Command>,
}

#[derive(Debug)]
enum Command {
    Append(BacklogRecord),
    Flush,
    Shutdown,
}

impl BacklogWriter {
    pub fn spawn(paths: &AppPaths, retention: BacklogRetention) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<Command>(8192);
        let logs_dir = paths.logs_dir.clone();
        thread::spawn(move || worker_loop(logs_dir, retention, rx));
        Self { tx }
    }

    pub fn append(&self, record: BacklogRecord) {
        if let Err(err) = self.tx.try_send(Command::Append(record)) {
            warn!(?err, "backlog append dropped due to full queue");
        }
    }

    pub fn flush(&self) {
        let _ = self.tx.try_send(Command::Flush);
    }
}

impl Drop for BacklogWriter {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Command::Shutdown);
    }
}

fn worker_loop(logs_dir: PathBuf, retention: BacklogRetention, rx: Receiver<Command>) {
    let mut current_path: Option<PathBuf> = None;
    let mut current_writer: Option<BufWriter<File>> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Command::Append(record)) => {
                let path = record_path(&logs_dir, &record.channel_login, record.ts);
                if current_path.as_ref() != Some(&path) {
                    if let Some(mut writer) = current_writer.take() {
                        let _ = writer.flush();
                    }
                    if let Err(err) = fs::create_dir_all(path.parent().unwrap_or(&logs_dir)) {
                        error!(?err, path = %path.display(), "failed to create backlog directory");
                        continue;
                    }
                    match open_writer(&path) {
                        Ok(writer) => {
                            current_path = Some(path.clone());
                            current_writer = Some(writer);
                            if let Err(err) = prune_logs(&logs_dir, retention) {
                                warn!(?err, "backlog prune failed");
                            }
                        }
                        Err(err) => {
                            error!(?err, path = %path.display(), "failed to open backlog file");
                            continue;
                        }
                    }
                }

                if let Some(writer) = current_writer.as_mut() {
                    if let Err(err) = write_record(writer, &record) {
                        error!(?err, "failed writing backlog record");
                    }
                }
            }
            Ok(Command::Flush) => {
                if let Some(writer) = current_writer.as_mut() {
                    let _ = writer.flush();
                }
            }
            Ok(Command::Shutdown) => {
                if let Some(mut writer) = current_writer.take() {
                    let _ = writer.flush();
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(writer) = current_writer.as_mut() {
                    let _ = writer.flush();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn open_writer(path: &Path) -> Result<BufWriter<File>> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed opening {}", path.display()))?;
    Ok(BufWriter::new(file))
}

fn write_record(writer: &mut BufWriter<File>, record: &BacklogRecord) -> Result<()> {
    serde_json::to_writer(&mut *writer, record).context("failed to serialize backlog record")?;
    writer.write_all(b"\n").context("failed to write newline")?;
    Ok(())
}

pub fn record_path(logs_dir: &Path, channel_login: &str, ts: DateTime<Utc>) -> PathBuf {
    logs_dir
        .join(channel_login)
        .join(format!("{:04}-{:02}-{:02}.jsonl", ts.year(), ts.month(), ts.day()))
}

fn prune_logs(logs_dir: &Path, retention: BacklogRetention) -> Result<()> {
    let now = Utc::now().date_naive();
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();

    if !logs_dir.exists() {
        return Ok(());
    }

    for channel_dir in fs::read_dir(logs_dir).with_context(|| format!("read_dir {}", logs_dir.display()))? {
        let channel_dir = channel_dir?;
        if !channel_dir.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(channel_dir.path())? {
            let file = file?;
            if !file.file_type()?.is_file() {
                continue;
            }
            let metadata = file.metadata()?;
            let modified = metadata.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let path = file.path();

            let stale_by_age = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
                .map(|date| (now - date).num_days() > retention.max_days as i64)
                .unwrap_or(false);
            if stale_by_age {
                let _ = fs::remove_file(&path);
                continue;
            }

            files.push((path, metadata.len(), modified));
        }
    }

    let mut total_bytes: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total_bytes <= retention.max_total_bytes {
        return Ok(());
    }

    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, len, _) in files {
        if total_bytes <= retention.max_total_bytes {
            break;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                total_bytes = total_bytes.saturating_sub(len);
            }
            Err(err) => warn!(?err, path = %path.display(), "failed to prune backlog file"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::record_path;

    #[test]
    fn path_is_rotated_by_day() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 2, 26, 12, 0, 0).unwrap();
        let path = record_path(std::path::Path::new("/tmp/logs"), "foo", ts);
        assert!(path.ends_with("foo/2026-02-26.jsonl"));
    }
}
