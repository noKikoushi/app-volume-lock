// settings.json in %APPDATA%\App Volume Lock
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Not locked: the slider sets the volume once.
    #[default]
    Off,
    /// Always put back to exactly this volume.
    Lock,
    /// May go lower, never above this volume.
    Max,
    /// Always muted.
    Mute,
}

/// Windows' own sounds ("System sounds" in the Volume Mixer).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SystemSoundsCfg {
    pub volume: u32,
    #[serde(default)]
    pub mode: Mode,
}

impl Default for SystemSoundsCfg {
    fn default() -> Self {
        SystemSoundsCfg { volume: 50, mode: Mode::Off }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppCfg {
    /// Start of the program name, lower case, without ".exe" ("medal" also covers MedalEncoder.exe).
    pub exe: String,
    pub name: String,
    pub volume: u32,
    #[serde(default)]
    pub mode: Mode,
    /// last seen program file, for its icon while the app isn't running
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub apps: Vec<AppCfg>,
    #[serde(default, rename = "systemSounds")]
    pub system_sounds: SystemSoundsCfg,
    #[serde(default = "yes")]
    pub start_with_windows: bool,
    #[serde(default = "yes")]
    pub show_system_sounds: bool,
    /// lock on every output device (true) or only on the default one
    #[serde(default = "yes")]
    pub all_devices: bool,
    /// programs seen playing sound, with when (unix seconds), so "Choose an app" can offer
    /// the ones from the last few days
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub recent: std::collections::HashMap<String, u64>,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    /// A fresh install starts with an empty list.
    fn default() -> Self {
        Settings {
            apps: vec![],
            system_sounds: SystemSoundsCfg::default(),
            start_with_windows: true,
            all_devices: true,
            show_system_sounds: true,
            recent: Default::default(),
        }
    }
}

pub fn dir() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("App Volume Lock")
}

fn file() -> PathBuf {
    dir().join("settings.json")
}

pub fn load() -> Settings {
    let Ok(text) = std::fs::read_to_string(file()) else { return Settings::default() };
    match serde_json::from_str::<Settings>(text.trim_start_matches('\u{feff}')) {
        Ok(s) => normalize(s),
        Err(_) => {
            // keep the broken file for the user instead of overwriting it silently
            let _ = std::fs::rename(file(), dir().join("settings.broken.json"));
            Settings::default()
        }
    }
}

pub fn save(s: &Settings) {
    let _ = std::fs::create_dir_all(dir());
    let Ok(json) = serde_json::to_string_pretty(s) else { return };
    let tmp = dir().join("settings.json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, file());
    }
}

pub fn clean_exe(exe: &str) -> String {
    let e = exe.trim().to_lowercase();
    e.strip_suffix(".exe").unwrap_or(&e).to_string()
}

/// "discord" -> "Discord"
pub fn display_name(exe: &str) -> String {
    let mut c = exe.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn normalize(mut s: Settings) -> Settings {
    s.system_sounds.volume = s.system_sounds.volume.min(100);
    let mut seen = std::collections::HashSet::new();
    s.apps.retain_mut(|a| {
        a.exe = clean_exe(&a.exe);
        a.volume = a.volume.min(100);
        if a.name.trim().is_empty() {
            a.name = display_name(&a.exe);
        }
        a.name = a.name.chars().take(40).collect();
        !a.exe.is_empty() && seen.insert(a.exe.clone())
    });
    s
}
