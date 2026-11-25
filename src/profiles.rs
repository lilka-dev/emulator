use crate::{
    preferences::{Settings, user_dir},
    storage::atomic_write,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub settings: Settings,
    #[serde(default)]
    pub firmware_name: String,
    #[serde(default)]
    managed_root: Option<String>,
}

impl Profile {
    pub fn directory(&self) -> std::path::PathBuf {
        user_dir().join("profiles").join(&self.id)
    }

    pub fn install_firmware(&mut self, source: &Path, base: Option<&Path>) -> Result<()> {
        for path in [Some(source), base].into_iter().flatten() {
            let metadata = std::fs::metadata(path)?;
            ensure!(
                metadata.is_file() && metadata.len() <= crate::firmware::MAX_FLASH_BYTES as u64,
                "Оберіть файл прошивки розміром до 16 MiB"
            );
        }
        let mut flash = crate::firmware::normalize(&std::fs::read(base.unwrap_or(source))?)?;
        if base.is_some() {
            crate::firmware::install_app(&mut flash, &std::fs::read(source)?)?;
        }
        ensure!(
            flash.get(0x8000..0x8002) == Some(&[0xaa, 0x50]),
            "Для окремого застосунку оберіть базову прошивку"
        );
        let path = self.directory().join("firmware.bin");
        atomic_write(&path, &flash)?;
        self.settings.firmware = path.display().to_string();
        self.firmware_name = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.settings.installed_state.clear();
        self.settings.installed_sd_snapshot = false;
        Ok(())
    }
    fn rebase_managed_paths(&mut self) {
        let Some(old) = self.managed_root.as_deref() else {
            return;
        };
        if old.is_empty() {
            return;
        }
        let new = user_dir().join("profiles").join(&self.id);
        fn moved(path: &mut String, old: &str, new: &Path) {
            let Some(suffix) = path.strip_prefix(old) else {
                return;
            };
            if !suffix.is_empty() && !suffix.starts_with(['/', '\\']) {
                return;
            }
            let mut value = new.to_path_buf();
            for part in suffix.split(['/', '\\']).filter(|part| !part.is_empty()) {
                if part == "." || part == ".." {
                    return;
                }
                value.push(part);
            }
            *path = value.display().to_string();
        }
        moved(&mut self.settings.state, old, &new);
        moved(&mut self.settings.firmware, old, &new);
        moved(&mut self.settings.sd.path, old, &new);
        moved(&mut self.settings.installed_state, old, &new);
        self.managed_root = Some(new.display().to_string());
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Profiles {
    pub active: Option<String>,
    pub profiles: Vec<Profile>,
}

impl Profiles {
    pub fn load() -> Result<Self> {
        let path = user_dir().join("profiles.json");
        if path.exists() {
            let bytes = std::fs::read(&path).context("Не вдалося прочитати профілі")?;
            let mut profiles: Self =
                serde_json::from_slice(&bytes).context("Пошкоджений файл профілів")?;
            profiles.validate()?;
            for profile in &mut profiles.profiles {
                profile.rebase_managed_paths();
            }
            return Ok(profiles);
        }
        let legacy = user_dir().join("settings.json");
        if legacy.exists() {
            let settings: Settings = serde_json::from_slice(&std::fs::read(&legacy)?)
                .context("Пошкоджений старий файл налаштувань")?;
            let mut profiles = Self::default();
            profiles.profiles.push(Profile {
                id: "default".into(),
                name: "Основний".into(),
                settings,
                firmware_name: String::new(),
                managed_root: None,
            });
            profiles.active = Some("default".into());
            profiles.save()?;
            return Ok(profiles);
        }
        Ok(Self::default())
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.active
                .as_ref()
                .is_none_or(|id| self.profiles.iter().any(|p| &p.id == id)),
            "Активний профіль відсутній"
        );
        for (i, profile) in self.profiles.iter().enumerate() {
            ensure!(
                !profile.id.is_empty()
                    && profile
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    && !profile.name.trim().is_empty(),
                "Некоректний профіль"
            );
            ensure!(
                !self.profiles[..i]
                    .iter()
                    .any(|p| p.id == profile.id || p.name == profile.name),
                "Дубльований профіль"
            );
        }
        Ok(())
    }

    pub fn create(&mut self, name: &str, mut settings: Settings) -> Result<()> {
        let name = name.trim();
        ensure!(!name.is_empty(), "Введіть назву профілю");
        ensure!(
            !self
                .profiles
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(name)),
            "Профіль з такою назвою вже існує"
        );
        ensure!(!settings.sd.path.trim().is_empty(), "Оберіть шлях SD-карти");
        ensure!(
            settings.sd.mode == crate::storage::SdMode::Folder,
            "Для профілю оберіть теку SD-карти"
        );
        let root = user_dir().join("profiles");
        std::fs::create_dir_all(&root)?;
        let mut n = 1;
        let (id, dir) = loop {
            let id = format!("profile-{n}");
            let dir = root.join(&id);
            if self.profiles.iter().any(|p| p.id == id) {
                n += 1;
                continue;
            }
            match std::fs::create_dir(&dir) {
                Ok(()) => break (id, dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(error) => return Err(error.into()),
            }
        };
        settings.state = dir.join("state").display().to_string();
        if settings.sd.mode == crate::storage::SdMode::Folder
            && Path::new(&settings.sd.path) == user_dir().join("sd-folder")
        {
            settings.sd.path = dir.join("sd").display().to_string();
        }
        settings.installed_state.clear();
        settings.installed_sd_snapshot = false;
        let mut profile = Profile {
            id: id.clone(),
            name: name.into(),
            settings,
            firmware_name: String::new(),
            managed_root: Some(dir.display().to_string()),
        };
        std::fs::create_dir_all(dir.join("state"))?;
        if profile.settings.sd.mode == crate::storage::SdMode::Folder {
            std::fs::create_dir_all(&profile.settings.sd.path)?;
        }
        if !profile.settings.firmware.is_empty() {
            let source = std::path::PathBuf::from(&profile.settings.firmware);
            if let Err(error) = profile.install_firmware(&source, None) {
                std::fs::remove_dir_all(&dir).ok();
                return Err(error);
            }
        }
        self.profiles.push(profile);
        self.active = Some(id);
        Ok(())
    }
    pub fn active(&self) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| Some(&p.id) == self.active.as_ref())
    }
    pub fn update_active(&mut self, settings: &Settings) {
        if let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|p| Some(&p.id) == self.active.as_ref())
        {
            profile.settings = settings.clone();
        }
    }
    pub fn select(&mut self, name: &str) -> Result<Settings> {
        let profile = self
            .profiles
            .iter()
            .find(|p| p.name == name)
            .context("Профіль не знайдено")?;
        self.active = Some(profile.id.clone());
        Ok(profile.settings.clone())
    }
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        atomic_write(
            &user_dir().join("profiles.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_is_owned_and_failed_replacement_preserves_installation() -> Result<()> {
        let mut profiles = Profiles::default();
        profiles.create("Owned firmware", Settings::default())?;
        let profile = profiles.profiles.last_mut().unwrap();
        let directory = profile.directory();
        assert!(directory.join("sd").is_dir());
        assert!(directory.join("state").is_dir());
        let source = directory.join("source.bin");
        let mut bytes = vec![0xff; 0x10001];
        bytes[..24].fill(0);
        bytes[0] = 0xe9;
        bytes[12] = 9;
        bytes[0x8000..0x8004].copy_from_slice(&[0xaa, 0x50, 0, 0]);
        bytes[0x8004..0x8008].copy_from_slice(&0x10000u32.to_le_bytes());
        bytes[0x8008..0x800c].copy_from_slice(&0x10000u32.to_le_bytes());
        bytes[0x10000] = 0xe9;
        std::fs::write(&source, &bytes)?;
        profile.install_firmware(&source, None)?;
        let installed = std::fs::read(&profile.settings.firmware)?;
        assert_eq!(std::fs::read(&source)?, bytes);
        assert_eq!(&installed[..bytes.len()], bytes);
        std::fs::write(&source, b"invalid firmware")?;
        profile.settings.installed_state = "previous session".into();
        assert!(profile.install_firmware(&source, None).is_err());
        assert_eq!(std::fs::read(&profile.settings.firmware)?, installed);
        assert_eq!(profile.settings.installed_state, "previous session");
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn profiles_isolate_storage_and_keep_settings_when_switched() {
        let mut profiles = Profiles::default();
        let mut first = Settings::default();
        first.wifi.ssid = "first-ap".into();
        profiles.create("One", first).unwrap();
        let first = profiles.active().unwrap().settings.clone();
        let mut second = Settings::default();
        second.wifi.ssid = "second-ap".into();
        profiles.create("Two", second).unwrap();
        let second = profiles.active().unwrap().settings.clone();
        assert_ne!(first.state, second.state);
        assert_ne!(first.sd.path, second.sd.path);
        assert_eq!(profiles.select("One").unwrap().wifi.ssid, "first-ap");
        assert_eq!(profiles.select("Two").unwrap().wifi.ssid, "second-ap");
        assert!(profiles.create("one", Settings::default()).is_err());
        let encoded = serde_json::to_vec(&profiles).unwrap();
        let decoded: Profiles = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.active().unwrap().name, "Two");
    }

    #[test]
    fn legacy_settings_get_default_keyboard_bindings() {
        let settings: Settings = serde_json::from_str(r#"{"muted":true}"#).unwrap();
        assert!(settings.muted);
        assert_eq!(settings.keys[0], "ArrowUp");
        assert_eq!(settings.keys[4], "z");
    }
    #[test]
    fn moved_portable_profile_rebases_only_managed_paths() {
        let mut profiles = Profiles::default();
        profiles.create("Portable", Settings::default()).unwrap();
        let profile = profiles.profiles.first_mut().unwrap();
        profile.managed_root = Some("C:\\old\\profiles\\profile-1".into());
        profile.settings.state = "C:\\old\\profiles\\profile-1\\state".into();
        profile.settings.installed_state = "C:\\old\\profiles\\profile-1\\state\\abcdef".into();
        profile.settings.sd.path = "D:\\external\\card.img".into();
        profile.rebase_managed_paths();
        let new = user_dir().join("profiles").join(&profile.id);
        assert_eq!(
            profile.settings.state,
            new.join("state").display().to_string()
        );
        assert_eq!(
            profile.settings.installed_state,
            new.join("state/abcdef").display().to_string()
        );
        assert_eq!(profile.settings.sd.path, "D:\\external\\card.img");
    }
}
