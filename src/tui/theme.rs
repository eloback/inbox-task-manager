//! The switcher's colors (DESIGN.md section 3, visual principles): one
//! [`Theme`] of roles, never a color named where it is drawn.
//!
//! Mocha and Latte are Catppuccin's, in truecolor, with the colors that
//! fell short moved until every role meets its contrast floor (the tests
//! pin it). "terminal" keeps the terminal's colors but never uses ANSI
//! colors 8–15: base16 themes remap them, color8 among them, which once
//! drew most of the text at 1.8:1.

use std::cell::Cell;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::config::ThemeName;

/// The roles. Grounds carry a background (and the text color, so plain
/// text drawn on them reads); the others are foregrounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// The screen.
    pub bg: Style,
    /// A dialog.
    pub panel: Style,
    /// A text field: the search, a form's fields.
    pub field: Style,
    /// The highlighted row of a list.
    pub sel: Style,
    /// Dividers and borders.
    pub rule: Style,
    /// What a dialog covers.
    pub dim: Style,
    /// What is read: titles, descriptions, notes, values.
    pub text: Style,
    /// Context: statuses, paths, key labels.
    pub sub: Style,
    /// Hints, counts and section labels, on the screen or a panel only.
    pub faint: Style,
    /// Focus.
    pub accent: Style,
    /// Keys in hints.
    pub key: Style,
    pub project: Style,
    /// Needs you, errors, deleting.
    pub flag: Style,
    /// Snoozed, warnings, archiving.
    pub snooze: Style,
    /// Working, open, done.
    pub ok: Style,
}

/// One palette's colors, by role.
struct Palette {
    bg: u32,
    panel: u32,
    field: u32,
    sel: u32,
    rule: u32,
    dim: u32,
    text: u32,
    sub: u32,
    faint: u32,
    accent: u32,
    key: u32,
    project: u32,
    flag: u32,
    snooze: u32,
    ok: u32,
}

const MOCHA: Palette = Palette {
    bg: 0x181825,
    panel: 0x1e1e2e,
    field: 0x313244,
    sel: 0x313244,
    rule: 0x45475a,
    dim: 0x585b70,
    text: 0xcdd6f4,
    sub: 0xa6adc8,
    faint: 0x82869e,
    accent: 0xb4befe,
    key: 0xfab387,
    project: 0xcba6f7,
    flag: 0xf38ba8,
    snooze: 0xf9e2af,
    ok: 0xa6e3a1,
};

const LATTE: Palette = Palette {
    bg: 0xeff1f5,
    panel: 0xe6e9ef,
    field: 0xdce0e8,
    sel: 0xdce0e8,
    rule: 0xbcc0cc,
    dim: 0x9ca0b0,
    text: 0x4c4f69,
    sub: 0x5c5f77,
    faint: 0x64677c,
    accent: 0x4f5daf,
    key: 0xaa4307,
    project: 0x7e35de,
    flag: 0xc50e36,
    snooze: 0x8a5812,
    ok: 0x2d701e,
};

impl Theme {
    /// `name`'s theme; without `truecolor`, its colors as their nearest of
    /// xterm's 240 fixed ones (16–255, which themes leave alone).
    pub fn new(name: ThemeName, truecolor: bool) -> Theme {
        let palette = match name {
            ThemeName::Mocha => &MOCHA,
            ThemeName::Latte => &LATTE,
            ThemeName::Terminal => return Theme::terminal(),
        };
        let color = |rgb: u32| {
            let [_, r, g, b] = rgb.to_be_bytes();
            if truecolor { Color::Rgb(r, g, b) } else { Color::Indexed(nearest(r, g, b)) }
        };
        let fg = |rgb| Style::new().fg(color(rgb));
        let ground = |rgb| Style::new().bg(color(rgb)).fg(color(palette.text));
        Theme {
            bg: ground(palette.bg),
            panel: ground(palette.panel),
            field: ground(palette.field),
            sel: ground(palette.sel),
            rule: fg(palette.rule),
            dim: fg(palette.dim),
            text: fg(palette.text),
            sub: fg(palette.sub),
            faint: fg(palette.faint),
            accent: fg(palette.accent),
            key: fg(palette.key),
            project: fg(palette.project),
            flag: fg(palette.flag),
            snooze: fg(palette.snooze),
            ok: fg(palette.ok),
        }
    }

    /// The terminal's own colors: text in its foreground, the faint tone
    /// dimmed, no grounds of its own, and only colors 1–6.
    fn terminal() -> Theme {
        let plain = Style::new().fg(Color::Reset);
        Theme {
            bg: Style::new().bg(Color::Reset).fg(Color::Reset),
            panel: Style::new().bg(Color::Reset).fg(Color::Reset),
            field: Style::new().bg(Color::Reset).fg(Color::Reset).add_modifier(Modifier::UNDERLINED),
            sel: Style::new().bg(Color::Reset).fg(Color::Reset).add_modifier(Modifier::BOLD),
            rule: plain.add_modifier(Modifier::DIM),
            dim: plain.add_modifier(Modifier::DIM),
            text: plain,
            sub: plain,
            faint: plain.add_modifier(Modifier::DIM),
            accent: Style::new().fg(Color::Cyan),
            key: Style::new().fg(Color::Cyan),
            project: Style::new().fg(Color::Magenta),
            flag: Style::new().fg(Color::Red),
            snooze: Style::new().fg(Color::Yellow),
            ok: Style::new().fg(Color::Green),
        }
    }
}

thread_local! {
    /// The theme drawing uses. Until [`start`] reads the environment, Mocha
    /// in truecolor whatever the terminal, so tests draw alike everywhere.
    static CURRENT: Cell<Current> = const { Cell::new(Current { name: ThemeName::Mocha, truecolor: true, env: None }) };
}

#[derive(Clone, Copy)]
struct Current {
    name: ThemeName,
    /// Whether the terminal says it draws 24-bit color (Herdr's panes do).
    truecolor: bool,
    /// The theme `ITM_THEME` names, which outranks the setting.
    env: Option<ThemeName>,
}

/// The theme drawing uses now.
pub fn theme() -> Theme {
    let current = CURRENT.get();
    Theme::new(current.name, current.truecolor)
}

/// Before the first frame: what the terminal (`COLORTERM`) and `ITM_THEME`
/// say, and the theme `setting` names unless `ITM_THEME` names another.
pub fn start(setting: ThemeName) {
    let var = |name| std::env::var(name).unwrap_or_default();
    let truecolor = matches!(var("COLORTERM").as_str(), "truecolor" | "24bit");
    let env = ThemeName::named(&var("ITM_THEME"));
    CURRENT.set(Current { name: env.unwrap_or(setting), truecolor, env });
}

/// Draw with the theme `setting` names from now on, unless `ITM_THEME` names another.
pub fn set(setting: ThemeName) {
    let current = CURRENT.get();
    CURRENT.set(Current { name: current.env.unwrap_or(setting), ..current });
}

/// The xterm color (16–255) nearest to `r g b`: the 6×6×6 cube's or the grays'.
fn nearest(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |c: u8| (0..6).min_by_key(|&i| (LEVELS[i] as i32 - c as i32).abs()).unwrap_or(0);
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (16 + 36 * ri + 6 * gi + bi) as u8;
    let gray_step = ((r as u32 + g as u32 + b as u32) / 3).saturating_sub(3).min(230) / 10;
    let gray = 232 + gray_step as u8;
    let distance = |(x, y, z): (u8, u8, u8)| {
        [(x, r), (y, g), (z, b)].iter().map(|&(a, c)| (a as i32 - c as i32).pow(2)).sum::<i32>()
    };
    let gray_level = 8 + 10 * gray_step as u8;
    let cube_rgb = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    if distance((gray_level, gray_level, gray_level)) < distance(cube_rgb) { gray } else { cube }
}

/// Everything already drawn in `area` set back to the dim tone on the
/// screen's ground: what a dialog covers (visual principle V5).
pub fn dim(buffer: &mut Buffer, area: Rect) {
    let theme = theme();
    let area = area.intersection(buffer.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.modifier = Modifier::empty();
            cell.set_style(theme.bg.patch(theme.dim));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else { panic!("{color:?} is not truecolor") };
        let channel = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast(fg: Style, ground: Style) -> f64 {
        let (a, b) = (luminance(fg.fg.unwrap()), luminance(ground.bg.unwrap()));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Visual principle V1: everything read meets 4.5:1 on every ground it
    /// is drawn on; the faint tone on the screen and on a panel.
    #[test]
    fn every_role_meets_its_contrast_floor() {
        for name in [ThemeName::Mocha, ThemeName::Latte] {
            let t = Theme::new(name, true);
            let read = [
                ("text", t.text),
                ("sub", t.sub),
                ("accent", t.accent),
                ("key", t.key),
                ("project", t.project),
                ("flag", t.flag),
                ("snooze", t.snooze),
                ("ok", t.ok),
            ];
            let grounds = [("bg", t.bg), ("panel", t.panel), ("field", t.field), ("sel", t.sel)];
            for ((role, fg), (ground, bg)) in read.iter().flat_map(|r| grounds.iter().map(move |g| (r, g))) {
                let ratio = contrast(*fg, *bg);
                assert!(ratio >= 4.5, "{name:?}: {role} on {ground} is {ratio:.2}:1");
            }
            for (ground, bg) in [("bg", t.bg), ("panel", t.panel)] {
                let ratio = contrast(t.faint, bg);
                assert!(ratio >= 4.5, "{name:?}: faint on {ground} is {ratio:.2}:1");
            }
            for (ground, bg) in grounds {
                assert_eq!(bg.fg, t.text.fg, "{name:?}: plain text on {ground} is drawn in the text tone");
            }
        }
    }

    /// The terminal theme never uses ANSI 8–15: base16 themes remap them.
    #[test]
    fn the_terminal_theme_uses_no_bright_colors() {
        let t = Theme::new(ThemeName::Terminal, true);
        let styles = [
            t.bg, t.panel, t.field, t.sel, t.rule, t.dim, t.text, t.sub, t.faint, t.accent, t.key, t.project, t.flag,
            t.snooze, t.ok,
        ];
        for color in styles.iter().flat_map(|s| [s.fg, s.bg]).flatten() {
            assert!(
                matches!(
                    color,
                    Color::Reset | Color::Red | Color::Green | Color::Yellow | Color::Magenta | Color::Cyan
                ),
                "{color:?}"
            );
        }
    }

    #[test]
    fn without_truecolor_colors_are_xterms_fixed_ones() {
        let t = Theme::new(ThemeName::Mocha, false);
        assert!(matches!(t.text.fg, Some(Color::Indexed(16..=255))));
        assert_eq!(nearest(0, 0, 0), 16);
        assert_eq!(nearest(255, 255, 255), 231);
        assert_eq!(nearest(0x80, 0x80, 0x80), 244);
        assert_eq!(nearest(0xff, 0x00, 0x00), 196);
    }

    /// Drawing never reads the environment, so a test's screen is the same
    /// in any terminal, and `ITM_THEME` outlasts a setting changed meanwhile.
    #[test]
    fn only_start_reads_the_terminal_and_itm_theme() {
        set(ThemeName::Latte);
        assert_eq!(theme(), Theme::new(ThemeName::Latte, true));
        CURRENT.set(Current { name: ThemeName::Terminal, truecolor: false, env: Some(ThemeName::Terminal) });
        set(ThemeName::Mocha);
        assert_eq!(theme(), Theme::new(ThemeName::Terminal, false));
    }
}
