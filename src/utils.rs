use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn default_dict_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("could not determine user config directory")?;
    Ok(config_dir.join("wayime/dict.tsv"))
}

pub fn default_cache_path() -> Result<PathBuf> {
    let cache_dir = dirs::cache_dir().context("could not determine user cache directory")?;
    Ok(cache_dir.join("wayime/dict.sqlite"))
}
