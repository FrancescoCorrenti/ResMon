//! Themes (background tones) and accent palettes (the colours of the measures), switchable at run time.

use std::sync::atomic::{AtomicU8, Ordering};

use egui::Color32;

const fn c(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub struct Theme {
    pub name: &'static str,
    pub light: bool,
    pub bg: Color32,       // full view background
    pub card: Color32,     // cards, bars, inputs
    pub line: Color32,     // hairlines, empty tracks
    pub sel_bg: Color32,   // the selected card
    pub sel_line: Color32, // and its border
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub pill_on: Color32,
    pub panel_bg: Color32,  // hover panel
    pub tm_bg: Color32,     // treemap well
    pub raised: Color32,    // tooltips, "Other" / "Files" tiles
    pub free: Color32,      // "Free" / "Idle" tiles
    pub unscanned: Color32, // "Not scanned yet"
    pub tab_bg: Color32,
    pub tab_line: Color32,
    pub row_sel: Color32,
}

pub const THEMES: [Theme; 3] = [
    Theme {
        name: "Dark",
        light: false,
        bg: c(0x0B0D11),
        card: c(0x13161C),
        line: c(0x1F232C),
        sel_bg: c(0x1A1F29),
        sel_line: c(0x4A5468),
        text: c(0xE6E8EE),
        muted: c(0x8B91A0),
        faint: c(0x566070),
        pill_on: c(0x2A3040),
        panel_bg: c(0x0E1014),
        tm_bg: c(0x12151B),
        raised: c(0x1C2029),
        free: c(0x15181F),
        unscanned: c(0x191C24),
        tab_bg: c(0x222631),
        tab_line: c(0x3A4150),
        row_sel: c(0x243049),
    },
    Theme {
        name: "Black",
        light: false,
        bg: c(0x000000),
        card: c(0x0C0D10),
        line: c(0x1A1C21),
        sel_bg: c(0x14171D),
        sel_line: c(0x454E60),
        text: c(0xEDEEF2),
        muted: c(0x8A8F9B),
        faint: c(0x4F5562),
        pill_on: c(0x232733),
        panel_bg: c(0x050506),
        tm_bg: c(0x08090B),
        raised: c(0x17191F),
        free: c(0x0E0F12),
        unscanned: c(0x121418),
        tab_bg: c(0x15171D),
        tab_line: c(0x30343F),
        row_sel: c(0x1E2840),
    },
    Theme {
        name: "Light",
        light: true,
        bg: c(0xF3F4F7),
        card: c(0xFFFFFF),
        line: c(0xE2E5EA),
        sel_bg: c(0xEAF0F9),
        sel_line: c(0x93A4C2),
        text: c(0x1A1D23),
        muted: c(0x5C6371),
        faint: c(0x9AA0AC),
        pill_on: c(0xDCE3EE),
        panel_bg: c(0xF7F8FA),
        tm_bg: c(0xE8EAEF),
        raised: c(0xFFFFFF),
        free: c(0xDDE0E6),
        unscanned: c(0xD3D7DE),
        tab_bg: c(0xFFFFFF),
        tab_line: c(0xC9CED7),
        row_sel: c(0xD9E4F5),
    },
];

pub struct Accents {
    pub name: &'static str,
    pub cpu: Color32,
    pub ram: Color32,
    pub commit: Color32,
    pub gpu: Color32,
    pub vram: Color32,
    pub disk: Color32,
    pub disk2: Color32,
    pub net: Color32,
    pub net2: Color32,
}

pub const ACCENTS: [Accents; 3] = [
    Accents {
        name: "Default",
        cpu: c(0x6CB6F5),
        ram: c(0xA993E8),
        commit: c(0x8A80C8),
        gpu: c(0x5CCB98),
        vram: c(0xE0BC62),
        disk: c(0xE8957A),
        disk2: c(0xF2C7B6),
        net: c(0x62C6D4),
        net2: c(0xB6E6EC),
    },
    Accents {
        name: "Vivid",
        cpu: c(0x3D9BFF),
        ram: c(0xB26BFF),
        commit: c(0x8C6BFF),
        gpu: c(0x24D17E),
        vram: c(0xFFC21F),
        disk: c(0xFF6B4A),
        disk2: c(0xFFB199),
        net: c(0x1FD1E0),
        net2: c(0x9BF0F7),
    },
    Accents {
        name: "Mono",
        cpu: c(0xC9D3E3),
        ram: c(0xB7C2D4),
        commit: c(0xA4AFC2),
        gpu: c(0xC9D3E3),
        vram: c(0xB7C2D4),
        disk: c(0xC9D3E3),
        disk2: c(0x8F9AAE),
        net: c(0xC9D3E3),
        net2: c(0x8F9AAE),
    },
];

static THEME: AtomicU8 = AtomicU8::new(0);
static ACCENT: AtomicU8 = AtomicU8::new(0);

pub fn th() -> &'static Theme {
    &THEMES[THEME.load(Ordering::Relaxed) as usize % THEMES.len()]
}

pub fn ac() -> &'static Accents {
    &ACCENTS[ACCENT.load(Ordering::Relaxed) as usize % ACCENTS.len()]
}

pub fn set(theme: &str, accents: &str) {
    THEME.store(THEMES.iter().position(|t| t.name == theme).unwrap_or(0) as u8, Ordering::Relaxed);
    ACCENT.store(ACCENTS.iter().position(|a| a.name == accents).unwrap_or(0) as u8, Ordering::Relaxed);
}
