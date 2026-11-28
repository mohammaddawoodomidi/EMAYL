pub mod about;
pub mod accounts_flow;
pub mod app;
pub mod badge;
pub mod compose_flow;
pub mod downloads;
pub mod icons;
pub mod prefs;
pub mod privfile;
pub mod services;
pub mod startup;
pub mod theme;
pub mod userdata_flow;
pub mod util;
pub mod view;
pub mod wizard;

/// The program's icon as raw pixels (64 x 64, RGBA), made from `assets/icon`.
const WINDOW_ICON: &[u8] = include_bytes!("../../../assets/icon/window-64.rgba");
const WINDOW_ICON_SIDE: u32 = 64;

/// The name the desktop uses to match this window with its menu entry and icon
/// (the file `packaging/linux/emayl.desktop`).
#[cfg(target_os = "linux")]
fn platform_settings() -> iced::window::settings::PlatformSpecific {
    iced::window::settings::PlatformSpecific {
        application_id: "emayl".to_owned(),
        ..iced::window::settings::PlatformSpecific::default()
    }
}

#[cfg(not(target_os = "linux"))]
fn platform_settings() -> iced::window::settings::PlatformSpecific {
    iced::window::settings::PlatformSpecific::default()
}

fn window_settings(prefs: &prefs::Prefs) -> iced::window::Settings {
    iced::window::Settings {
        // Where the person left it last time (or a size that fits a small laptop screen).
        size: prefs.window_size(),
        maximized: prefs.maximized,
        min_size: Some(prefs::MIN_SIZE),
        position: iced::window::Position::Centered,
        // A broken icon must never stop the program from opening.
        icon: iced::window::icon::from_rgba(
            WINDOW_ICON.to_vec(),
            WINDOW_ICON_SIDE,
            WINDOW_ICON_SIDE,
        )
        .ok(),
        platform_specific: platform_settings(),
        ..iced::window::Settings::default()
    }
}

fn saved_prefs() -> prefs::Prefs {
    services::Paths::platform_default()
        .map(|p| prefs::Prefs::load(&p.data))
        .unwrap_or_default()
}

/// Starts the program window.
pub fn run() -> iced::Result {
    iced::application(app::App::boot, app::App::update, view::view)
        .title(app::App::title)
        .subscription(app::App::subscription)
        .theme(app_theme)
        .scale_factor(|app: &app::App| f32::from(app.prefs.look.scale) / 100.0)
        .window(window_settings(&saved_prefs()))
        .run()
}

fn app_theme(app: &app::App) -> iced::Theme {
    theme::iced_theme(&app.colors())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_window_icon_is_a_complete_picture() {
        assert_eq!(
            super::WINDOW_ICON.len(),
            (super::WINDOW_ICON_SIDE * super::WINDOW_ICON_SIDE * 4) as usize
        );
        assert!(
            super::window_settings(&crate::prefs::Prefs::default())
                .icon
                .is_some(),
            "the icon bytes were rejected"
        );
        // Not blank: the picture is opaque in the middle and has colour in it.
        let middle = (32 * 64 + 32) * 4;
        assert_eq!(super::WINDOW_ICON[middle + 3], 255);
        assert!(
            super::WINDOW_ICON[middle..middle + 3]
                .iter()
                .any(|b| *b > 0)
        );
    }
}
