use crate::storage::{SdSettings, atomic_write};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn user_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("LILKA_DATA_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(directory) = exe.parent()
        && (directory.join("portable.txt").is_file()
            || std::env::args_os().any(|arg| arg == "--portable"))
    {
        return directory.join("data");
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")));
    base.unwrap_or_else(std::env::temp_dir)
        .join("lilka-emulator")
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum Scale {
    Integer,
    #[default]
    Fit,
    X1,
    X2,
    X3,
}
impl Scale {
    pub fn label(self) -> &'static str {
        match self {
            Self::Integer => "Цілі пікселі",
            Self::Fit => "Вписати у вікно",
            Self::X1 => "1×",
            Self::X2 => "2×",
            Self::X3 => "3×",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum TerminalMode {
    #[default]
    #[serde(alias = "External")]
    Embedded,
    Window,
}
impl TerminalMode {
    pub const ALL: [Self; 2] = [Self::Embedded, Self::Window];
}
impl std::fmt::Display for TerminalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Embedded => "Вбудований",
            Self::Window => "Окреме вікно",
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub terminal: TerminalMode,
    pub firmware: String,
    pub state: String,
    pub sd: SdSettings,
    pub scale: Scale,
    pub muted: bool,
    pub controls: bool,
    pub wifi: crate::radio::WifiConfig,
    pub keys: [String; 10],
    pub developer_mode: bool,
    pub installed_state: String,
    pub installed_sd_snapshot: bool,
}
impl Default for Settings {
    fn default() -> Self {
        let sd = SdSettings {
            mode: crate::storage::SdMode::Folder,
            path: user_dir().join("sd-folder").display().to_string(),
            ..Default::default()
        };
        Self {
            terminal: TerminalMode::default(),
            firmware: String::new(),
            state: user_dir().join("state").display().to_string(),
            sd,
            scale: Scale::Fit,
            muted: false,
            controls: true,
            wifi: Default::default(),
            keys: crate::controls::DEFAULT_KEYS.map(String::from),
            developer_mode: false,
            installed_state: String::new(),
            installed_sd_snapshot: false,
        }
    }
}
impl Settings {
    pub fn load() -> Self {
        std::fs::read(user_dir().join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> anyhow::Result<()> {
        atomic_write(
            &user_dir().join("settings.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

impl std::fmt::Display for Scale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::TerminalMode;

    #[test]
    fn old_external_mode_falls_back_to_embedded() {
        assert_eq!(
            serde_json::from_str::<TerminalMode>(r#""External""#).unwrap(),
            TerminalMode::Embedded
        );
    }
}
