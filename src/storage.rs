use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum SdMode {
    #[default]
    Container,
    Image,
    Folder,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SdSettings {
    pub mode: SdMode,
    pub path: String,
    pub size_mib: usize,
}
impl Default for SdSettings {
    fn default() -> Self {
        Self {
            mode: SdMode::Container,
            path: ".lilka-emulator/sd.img".into(),
            size_mib: 32,
        }
    }
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temp = path.with_extension(format!("lilka-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("Запис {}", path.display()))
}
