use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories_next::ProjectDirs;

use crate::model::AppConfig;

const QUALIFIER: &str = "com";
const ORGANIZATION: &str = "fastchat";
const APPLICATION: &str = "FastChat";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_file: PathBuf,
    pub logs_dir: PathBuf,
    pub assets_cache_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .context("unable to determine platform app directories")?;

        let config_dir = dirs.config_dir().to_path_buf();
        let data_dir = dirs.data_dir().to_path_buf();
        let cache_dir = dirs.cache_dir().to_path_buf();
        let config_file = config_dir.join("config.json");
        let logs_dir = data_dir.join("logs");
        let assets_cache_dir = cache_dir.join("assets");

        for dir in [
            &config_dir,
            &data_dir,
            &cache_dir,
            &logs_dir,
            &assets_cache_dir,
        ] {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed creating directory {}", dir.display()))?;
        }

        Ok(Self {
            config_dir,
            data_dir,
            cache_dir,
            config_file,
            logs_dir,
            assets_cache_dir,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ConfigRepository {
    config_file: PathBuf,
}

impl ConfigRepository {
    pub fn new(paths: &AppPaths) -> Self {
        Self {
            config_file: paths.config_file.clone(),
        }
    }

    pub fn load_or_default(&self) -> Result<AppConfig> {
        if !self.config_file.exists() {
            return Ok(AppConfig::default());
        }

        let bytes = fs::read(&self.config_file)
            .with_context(|| format!("failed reading {}", self.config_file.display()))?;
        let config = serde_json::from_slice::<AppConfig>(&bytes)
            .with_context(|| format!("failed parsing {}", self.config_file.display()))?;
        Ok(config)
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        save_json_pretty(&self.config_file, config)
    }

    pub fn config_path(&self) -> &Path {
        &self.config_file
    }
}

fn save_json_pretty(path: &Path, config: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config).context("failed serializing config")?;
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed replacing {}", path.display()))?;
    Ok(())
}
