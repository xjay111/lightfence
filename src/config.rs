use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One page of a fence. A fence with a single tab shows its name as the title.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tab {
    pub name: String,
    /// Full paths of desktop items on this tab. The files stay where they are.
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FenceCfg {
    pub id: u64,
    /// Physical pixels, screen coordinates.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    /// Expanded height; a collapsed fence shows only its title bar.
    pub h: i32,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    /// Index of the tab being shown.
    #[serde(default)]
    pub active: usize,
    /// The desktop itself: holds every desktop item no other fence claims.
    #[serde(default)]
    pub catch_all: bool,
    /// Desktop only: grid cell [column, row] per lowercased item path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cells: BTreeMap<String, [i32; 2]>,
    // Pre-tab format: a fence had one title and one item list.
    #[serde(default, skip_serializing)]
    title: String,
    #[serde(default, skip_serializing)]
    items: Vec<String>,
}

impl FenceCfg {
    pub fn new(id: u64, name: &str, x: i32, y: i32, w: i32, h: i32) -> Self {
        FenceCfg {
            id,
            x,
            y,
            w,
            h,
            collapsed: false,
            tabs: vec![Tab { name: name.into(), items: Vec::new() }],
            active: 0,
            catch_all: false,
            cells: BTreeMap::new(),
            title: String::new(),
            items: Vec::new(),
        }
    }

    /// Folds the pre-tab fields into a first tab and keeps `active` in range.
    pub fn normalize(&mut self) {
        if self.tabs.is_empty() {
            let name = if self.title.is_empty() { "新建分区".into() } else { std::mem::take(&mut self.title) };
            self.tabs.push(Tab { name, items: std::mem::take(&mut self.items) });
        }
        self.active = self.active.min(self.tabs.len() - 1);
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Every item on every tab.
    pub fn all_items(&self) -> impl Iterator<Item = &String> {
        self.tabs.iter().flat_map(|t| &t.items)
    }
}

/// User preferences from the settings window.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Fence background colour, RGB.
    pub color: [u8; 3],
    /// Fence background opacity, 0–100 %.
    pub opacity: u8,
    /// Icon edge in pixels at 100 % scaling.
    pub icon_size: u32,
    /// Fences touching a screen edge shrink to their title bar when the mouse leaves.
    pub auto_hide: bool,
    pub auto_hide_delay_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { color: [0, 0, 0], opacity: 30, icon_size: 48, auto_hide: false, auto_hide_delay_ms: 800 }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    #[serde(default)]
    pub fences: Vec<FenceCfg>,
    #[serde(default)]
    pub settings: Settings,
    /// Hide Explorer's own desktop icons while fences run.
    #[serde(default = "yes")]
    pub hide_desktop_icons: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { fences: Vec::new(), settings: Settings::default(), hide_desktop_icons: true }
    }
}

fn yes() -> bool {
    true
}

pub fn dir() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("Lightfence")
}

/// Moves settings from the pre-rename folder (%APPDATA%\fences) on first run.
pub fn migrate_legacy_dir() {
    let new = dir();
    let old = new.with_file_name("fences");
    if !new.exists() && old.join("config.json").exists() {
        let _ = std::fs::rename(&old, &new);
    }
}

fn path() -> PathBuf {
    dir().join("config.json")
}

pub fn load() -> Config {
    let p = path();
    let Ok(bytes) = std::fs::read(&p) else {
        return Config::default();
    };
    match serde_json::from_slice::<Config>(&bytes) {
        Ok(mut cfg) => {
            cfg.fences.iter_mut().for_each(FenceCfg::normalize);
            cfg
        }
        Err(e) => {
            // Keep the unreadable file instead of silently overwriting the user's layout.
            crate::log(&format!("config parse error, moved to config.json.bad: {e}"));
            let _ = std::fs::rename(&p, p.with_extension("json.bad"));
            Config::default()
        }
    }
}

pub fn save(cfg: &Config) {
    let _ = std::fs::create_dir_all(dir());
    let Ok(data) = serde_json::to_vec_pretty(cfg) else { return };
    let tmp = path().with_extension("json.tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(tmp, path());
    }
}
