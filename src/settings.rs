//! What ResMon remembers between runs: `%APPDATA%\ResMon\settings.txt`, one `key=value` per line.

use std::path::PathBuf;

/// Cards of the full view / panel that can be switched off (DRIVES covers the per-drive cards).
pub const CARDS: [(&str, &str); 7] =
    [("cpu", "CPU"), ("ram", "Memory"), ("gpu", "GPU"), ("vram", "VRAM"), ("net", "Network"), ("disk", "Disk activity"), ("drives", "Drives")];
/// Measures the docked tab can show as bars.
pub const TAB_BARS: [(&str, &str); 6] = [("cpu", "CPU"), ("ram", "Memory"), ("gpu", "GPU"), ("vram", "VRAM"), ("net", "Network"), ("disk", "Disk")];

#[derive(Clone)]
pub struct Settings {
    pub anchor: Option<(i32, i32)>,
    pub metric: String,
    pub heat: bool,
    pub relative: bool,
    pub per_core: bool,
    pub table_open: bool,
    pub hidden: Vec<String>, // cards switched off
    pub tab: Vec<String>,    // bars on the docked tab, in order
    pub theme: String,
    pub accents: String,
    pub hover: f32, // seconds resting on the tab before the panel opens
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            anchor: None,
            metric: "ws".into(),
            heat: false,
            relative: false,
            per_core: false,
            table_open: true,
            hidden: Vec::new(),
            tab: vec!["cpu".into(), "ram".into(), "gpu".into()],
            theme: "Dark".into(),
            accents: "Default".into(),
            hover: 0.6,
        }
    }
}

fn path() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("ResMon").join("settings.txt")
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Ok(text) = std::fs::read_to_string(path()) else {
            s.anchor = crate::sys::load_anchor(); // the first versions kept only the tab position, in settings.json
            return s;
        };
        let list = |v: &str| v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "true";
            match k {
                "anchor" => {
                    let xy: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    if let [x, y] = xy[..]
                        && crate::sys::monitor_exists(x, y)
                    {
                        s.anchor = Some((x, y));
                    }
                }
                "metric" => s.metric = v.into(),
                "heat" => s.heat = b,
                "relative" => s.relative = b,
                "per_core" => s.per_core = b,
                "table_open" => s.table_open = b,
                "hidden" => s.hidden = list(v),
                "tab" => s.tab = list(v),
                "theme" => s.theme = v.into(),
                "accents" => s.accents = v.into(),
                "hover" => s.hover = v.parse().unwrap_or(0.6_f32).clamp(0.2, 3.0),
                _ => {}
            }
        }
        if s.tab.is_empty() {
            s.tab = Settings::default().tab;
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let anchor = self.anchor.map(|(x, y)| format!("{x},{y}")).unwrap_or_default();
        let text = format!(
            "anchor={anchor}\nmetric={}\nheat={}\nrelative={}\nper_core={}\ntable_open={}\nhidden={}\ntab={}\ntheme={}\naccents={}\nhover={}\n",
            self.metric,
            self.heat,
            self.relative,
            self.per_core,
            self.table_open,
            self.hidden.join(","),
            self.tab.join(","),
            self.theme,
            self.accents,
            self.hover
        );
        std::fs::write(p, text)
    }

    pub fn shows(&self, card: &str) -> bool {
        !self.hidden.iter().any(|h| h == card)
    }
}
