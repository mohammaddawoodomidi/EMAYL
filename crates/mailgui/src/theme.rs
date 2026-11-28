//! The bright theme: white surfaces, soft grey panels, one blue accent.

use iced::widget::{button, container, pick_list, text_editor, text_input};
use iced::{Background, Border, Color, Theme, theme::Palette};

/// White, for text that sits on a coloured button or avatar in every theme.
pub const WHITE: Color = Color::WHITE;

/// Every colour the program uses. Read it with [`c`]; never keep a copy across frames, because
/// the choice of theme and accent can change while the program runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    pub dark: bool,
    /// Cards, dialogs, the reading area and the window itself.
    pub surface: Color,
    /// The sidebar, chips and code blocks.
    pub panel: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub accent_hover: Color,
    /// The fill of a main button (white text sits on it), and the same when hovered.
    pub accent_fill: Color,
    pub accent_fill_hover: Color,
    pub accent_soft: Color,
    pub border: Color,
    pub hover: Color,
    pub danger: Color,
    pub danger_soft: Color,
    /// The fill of a destructive button (white text sits on it).
    pub danger_fill: Color,
    pub warn: Color,
    pub warn_soft: Color,
    pub ok: Color,
    pub ok_soft: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::from_rgb(
        ((hex >> 16) & 0xFF) as f32 / 255.0,
        ((hex >> 8) & 0xFF) as f32 / 255.0,
        (hex & 0xFF) as f32 / 255.0,
    )
}

/// Which look the person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    Light,
    Dark,
    /// Follow the operating system (light when it cannot be found out).
    #[default]
    System,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
            ThemeChoice::System => "Match my computer",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
            ThemeChoice::System => "system",
        }
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == k)
    }
}

/// The accent colour choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accent {
    #[default]
    Blue,
    Indigo,
    Teal,
    Green,
    Orange,
    Rose,
}

impl Accent {
    pub const ALL: [Accent; 6] = [
        Accent::Blue,
        Accent::Indigo,
        Accent::Teal,
        Accent::Green,
        Accent::Orange,
        Accent::Rose,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Accent::Blue => "Blue",
            Accent::Indigo => "Indigo",
            Accent::Teal => "Teal",
            Accent::Green => "Green",
            Accent::Orange => "Orange",
            Accent::Rose => "Rose",
        }
    }

    pub fn key(self) -> &'static str {
        self.name()
    }

    pub fn from_key(k: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == k)
    }

    /// The shade used on white (strong enough for text) and on dark surfaces (lighter).
    fn shade(self, dark: bool) -> Color {
        match (self, dark) {
            (Accent::Blue, false) => rgb(0x3561DD),
            (Accent::Blue, true) => rgb(0x6B9BFF),
            (Accent::Indigo, false) => rgb(0x5B4BDB),
            (Accent::Indigo, true) => rgb(0x9A8CFF),
            (Accent::Teal, false) => rgb(0x0E7584),
            (Accent::Teal, true) => rgb(0x3FC8D8),
            (Accent::Green, false) => rgb(0x1B7943),
            (Accent::Green, true) => rgb(0x4CCB86),
            (Accent::Orange, false) => rgb(0xAB4F0A),
            (Accent::Orange, true) => rgb(0xFF9A4D),
            (Accent::Rose, false) => rgb(0xC02F69),
            (Accent::Rose, true) => rgb(0xFF6FA0),
        }
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::from_rgb(
        a.r + (b.r - a.r) * t,
        a.g + (b.g - a.g) * t,
        a.b + (b.b - a.b) * t,
    )
}

impl Colors {
    pub fn build(dark: bool, accent: Accent) -> Self {
        let a = accent.shade(dark);
        if dark {
            let surface = rgb(0x151922);
            Self {
                dark,
                surface,
                panel: rgb(0x0F1218),
                text: rgb(0xE8EBF2),
                muted: rgb(0x9CA5B8),
                faint: rgb(0x6F7A90),
                accent: a,
                accent_hover: mix(a, Color::WHITE, 0.18),
                accent_fill: mix(a, Color::BLACK, 0.40),
                accent_fill_hover: mix(a, Color::BLACK, 0.28),
                accent_soft: mix(surface, a, 0.20),
                border: rgb(0x2A3141),
                hover: rgb(0x1E2431),
                danger: rgb(0xF47272),
                danger_soft: rgb(0x3A2024),
                danger_fill: mix(rgb(0xF47272), Color::BLACK, 0.45),
                warn: rgb(0xE5A94F),
                warn_soft: rgb(0x362C17),
                ok: rgb(0x45C48D),
                ok_soft: rgb(0x14312A),
            }
        } else {
            let surface = Color::WHITE;
            Self {
                dark,
                surface,
                panel: rgb(0xF6F7FB),
                text: rgb(0x1F2430),
                muted: rgb(0x626978),
                faint: rgb(0x9CA3AF),
                accent: a,
                accent_hover: mix(a, Color::BLACK, 0.15),
                accent_fill: a,
                accent_fill_hover: mix(a, Color::BLACK, 0.15),
                accent_soft: mix(surface, a, 0.11),
                border: rgb(0xE5E7EB),
                hover: rgb(0xF0F2F8),
                danger: rgb(0xD64545),
                danger_soft: rgb(0xFEEDED),
                danger_fill: rgb(0xCC3B3B),
                warn: rgb(0xB7791F),
                warn_soft: rgb(0xFFF7E6),
                ok: rgb(0x1F9D6B),
                ok_soft: rgb(0xEAF8F2),
            }
        }
    }
}

thread_local! {
    /// The colours in use on this thread. The window runs on one thread, and each test thread has
    /// its own, so tests can use different themes at the same time.
    static COLORS: std::cell::Cell<Colors> = std::cell::Cell::new(Colors::build(false, Accent::Blue));
}

/// The colours to draw with right now.
pub fn c() -> Colors {
    COLORS.with(std::cell::Cell::get)
}

/// Makes `colors` the ones [`c`] returns. Called at the start of every frame.
pub fn use_colors(colors: Colors) {
    COLORS.with(|cell| cell.set(colors));
}

pub fn iced_theme(colors: &Colors) -> Theme {
    Theme::custom(
        if colors.dark {
            "EMAYL Dark"
        } else {
            "EMAYL Light"
        },
        Palette {
            background: colors.surface,
            text: colors.text,
            primary: colors.accent,
            success: colors.ok,
            warning: colors.warn,
            danger: colors.danger,
        },
    )
}

fn border(color: Color, radius: f32) -> Border {
    Border {
        color,
        width: 1.0,
        radius: radius.into(),
    }
}

/// A fill, or nothing when the colour is fully transparent. iced draws a rounded box even for a
/// transparent fill, and on the processor drawing path that costs about as much as a visible
/// one, so a button that looks "empty" must say `None` instead.
fn fill(color: Color) -> Option<Background> {
    (color.a > 0.0).then_some(Background::Color(color))
}

// ----- containers -----------------------------------------------------------

pub fn page(_: &Theme) -> container::Style {
    container::Style {
        // The window is already cleared to white; painting it again is wasted work.
        background: None,
        text_color: Some(c().text),
        ..Default::default()
    }
}

pub fn sidebar(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().panel)),
        text_color: Some(c().text),
        border: Border {
            color: c().border,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub fn pane_edge(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().border)),
        ..Default::default()
    }
}

// No widget in this program may use a shadow. The processor drawing code in iced 0.14 does not
// clip a shadow to the area being repainted, so every small repaint (a blinking cursor, a hover)
// paints the shadow again and the surface slowly turns grey and then black. The tests/no_shadows.rs
// test keeps this true.
pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().surface)),
        border: border(c().border, 12.0),
        ..Default::default()
    }
}

pub fn dialog(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().surface)),
        border: border(c().border, 14.0),
        ..Default::default()
    }
}

pub fn scrim(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color {
            a: 0.35,
            ..Color::BLACK
        })),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warning,
    Danger,
    Good,
}

pub fn banner(tone: Tone) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let (bg, fg) = match tone {
            Tone::Info => (c().accent_soft, c().accent),
            Tone::Warning => (c().warn_soft, c().warn),
            Tone::Danger => (c().danger_soft, c().danger),
            Tone::Good => (c().ok_soft, c().ok),
        };
        container::Style {
            background: Some(Background::Color(bg)),
            border: border(Color { a: 0.35, ..fg }, 10.0),
            text_color: Some(c().text),
            ..Default::default()
        }
    }
}

pub fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Info => c().accent,
        Tone::Warning => c().warn,
        Tone::Danger => c().danger,
        Tone::Good => c().ok,
    }
}

pub fn chip(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().panel)),
        border: border(c().border, 999.0),
        text_color: Some(c().muted),
        ..Default::default()
    }
}

pub fn avatar(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 999.0.into(),
        },
        text_color: Some(WHITE),
        ..Default::default()
    }
}

pub fn dot(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 999.0.into(),
        },
        ..Default::default()
    }
}

/// The tick box in front of a list row.
pub fn tick_box(on: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: if on {
            fill(c().accent_fill)
        } else {
            fill(c().surface)
        },
        border: Border {
            color: if on { c().accent_fill } else { c().faint },
            width: 1.5,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

pub fn code_block(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().panel)),
        border: border(c().border, 8.0),
        ..Default::default()
    }
}

pub fn quote_bar(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c().border)),
        ..Default::default()
    }
}

// ----- buttons --------------------------------------------------------------

pub fn primary(_: &Theme, status: button::Status) -> button::Style {
    let (bg, text) = match status {
        button::Status::Active => (c().accent_fill, WHITE),
        button::Status::Hovered => (c().accent_fill_hover, WHITE),
        button::Status::Pressed => (mix(c().accent_fill_hover, Color::BLACK, 0.1), WHITE),
        button::Status::Disabled => (mix(c().accent_fill, c().surface, 0.6), WHITE),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: text,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

pub fn secondary(_: &Theme, status: button::Status) -> button::Style {
    let (bg, text) = match status {
        button::Status::Active => (c().surface, c().text),
        button::Status::Hovered => (c().hover, c().text),
        button::Status::Pressed => (c().border, c().text),
        button::Status::Disabled => (c().surface, c().faint),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: text,
        border: border(c().border, 10.0),
        ..Default::default()
    }
}

pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    let (bg, text) = match status {
        button::Status::Active => (Color::TRANSPARENT, c().muted),
        button::Status::Hovered => (c().hover, c().text),
        button::Status::Pressed => (c().border, c().text),
        button::Status::Disabled => (Color::TRANSPARENT, c().faint),
    };
    button::Style {
        background: fill(bg),
        text_color: text,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}

pub fn danger_ghost(_: &Theme, status: button::Status) -> button::Style {
    let (bg, text) = match status {
        button::Status::Active => (Color::TRANSPARENT, c().danger),
        button::Status::Hovered | button::Status::Pressed => (c().danger_soft, c().danger),
        button::Status::Disabled => (Color::TRANSPARENT, c().faint),
    };
    button::Style {
        background: fill(bg),
        text_color: text,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}

pub fn danger(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Active => c().danger_fill,
        button::Status::Hovered | button::Status::Pressed => {
            mix(c().danger_fill, Color::BLACK, 0.15)
        }
        button::Status::Disabled => mix(c().danger_fill, c().surface, 0.6),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: WHITE,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

/// A list row or sidebar entry: transparent, tinted when selected or hovered.
pub fn row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = if selected {
            c().accent_soft
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => c().hover,
                _ => Color::TRANSPARENT,
            }
        };
        button::Style {
            background: fill(bg),
            text_color: c().text,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 10.0.into(),
            },
            ..Default::default()
        }
    }
}

/// A button that looks like a link inside a message.
pub fn link(_: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: None,
        text_color: if matches!(status, button::Status::Hovered) {
            c().accent_hover
        } else {
            c().accent
        },
        ..Default::default()
    }
}

// ----- inputs ---------------------------------------------------------------

pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let (color, width) = match status {
        text_input::Status::Focused { .. } => (c().accent, 1.5),
        text_input::Status::Hovered => (c().faint, 1.0),
        _ => (c().border, 1.0),
    };
    text_input::Style {
        background: Background::Color(c().surface),
        border: Border {
            color,
            width,
            radius: 10.0.into(),
        },
        icon: c().muted,
        placeholder: c().faint,
        value: c().text,
        selection: Color {
            a: 0.25,
            ..c().accent
        },
    }
}

/// The look of drop-down lists, like the text boxes.
pub fn pick(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let border = match status {
        pick_list::Status::Opened { .. } => c().accent,
        pick_list::Status::Hovered => c().faint,
        pick_list::Status::Active => c().border,
    };
    pick_list::Style {
        text_color: c().text,
        placeholder_color: c().muted,
        handle_color: c().muted,
        background: Background::Color(c().surface),
        border: Border {
            color: border,
            width: 1.0,
            radius: 10.0.into(),
        },
    }
}

pub fn search_input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let (bg, color) = match status {
        text_input::Status::Focused { .. } => (c().surface, c().accent),
        _ => (c().panel, Color::TRANSPARENT),
    };
    text_input::Style {
        background: Background::Color(bg),
        border: Border {
            color,
            width: 1.5,
            radius: 10.0.into(),
        },
        icon: c().muted,
        placeholder: c().faint,
        value: c().text,
        selection: Color {
            a: 0.25,
            ..c().accent
        },
    }
}

pub fn toast(bad: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(if bad {
            Color::from_rgb(0.55, 0.14, 0.14)
        } else {
            Color::from_rgb(0.12, 0.14, 0.19)
        })),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 12.0.into(),
        },
        text_color: Some(WHITE),
        ..Default::default()
    }
}

/// A field without a box, for rows that are separated by thin lines (the address lines).
pub fn bare_input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let _ = status;
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        icon: c().muted,
        placeholder: c().faint,
        value: c().text,
        selection: Color {
            a: 0.25,
            ..c().accent
        },
    }
}

/// The big writing area.
pub fn editor(_: &Theme, _status: text_editor::Status) -> text_editor::Style {
    text_editor::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 0.0.into(),
        },
        placeholder: c().faint,
        value: c().text,
        selection: Color {
            a: 0.25,
            ..c().accent
        },
    }
}

/// The scroll area style: iced's own, minus the shadow it puts under the round marker shown
/// after a middle-click (see the note above [`card`]).
pub fn scroll(
    theme: &Theme,
    status: iced::widget::scrollable::Status,
) -> iced::widget::scrollable::Style {
    let mut style = iced::widget::scrollable::default(theme, status);
    style.auto_scroll.shadow = iced::Shadow::default();
    style
}

/// A scroll area with the program's style. Use this instead of `iced::widget::scrollable`.
pub fn scrolling<'a, M: 'a>(
    content: impl Into<iced::Element<'a, M>>,
) -> iced::widget::Scrollable<'a, M> {
    iced::widget::scrollable(content).style(scroll)
}

#[cfg(test)]
mod contrast_tests {
    use super::*;

    fn lum(c: Color) -> f32 {
        let f = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
    }

    fn ratio(a: Color, b: Color) -> f32 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_is_readable_on_every_surface_in_every_theme_and_accent() {
        for dark in [false, true] {
            for accent in Accent::ALL {
                let k = Colors::build(dark, accent);
                for bg in [k.surface, k.panel, k.hover, k.accent_soft] {
                    // Body text: WCAG AA for normal text.
                    assert!(ratio(k.text, bg) >= 7.0, "text {dark} {accent:?}");
                    assert!(ratio(k.muted, bg) >= 4.5, "muted {dark} {accent:?}");
                    // Accent coloured text and icons.
                    assert!(
                        ratio(k.accent, bg) >= 4.5,
                        "accent {dark} {accent:?} {bg:?}"
                    );
                }
                // White text on a button of the accent colour.
                assert!(
                    ratio(WHITE, k.accent_fill) >= 4.5,
                    "button {dark} {accent:?}"
                );
                assert!(ratio(WHITE, k.danger_fill) >= 4.5, "danger button {dark}");
                for (fg, bg) in [
                    (k.danger, k.danger_soft),
                    (k.warn, k.warn_soft),
                    (k.ok, k.ok_soft),
                ] {
                    assert!(ratio(fg, bg) >= 3.0, "banner {dark}");
                }
            }
        }
    }
}
