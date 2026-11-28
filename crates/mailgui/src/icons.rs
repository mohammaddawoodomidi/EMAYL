//! Small line icons, drawn here as SVG (no icon font, nothing downloaded).
//! They are tinted at draw time, so the files below use plain black.

use iced::widget::{Svg, svg};
use iced::{Color, Length, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Inbox,
    Send,
    Star,
    StarFilled,
    Paperclip,
    Trash,
    Archive,
    Refresh,
    Search,
    Compose,
    Reply,
    Close,
    Check,
    Warning,
    Folder,
    User,
    Mail,
    Shield,
    Draft,
    ChevronDown,
    ChevronRight,
    ChevronLeft,
    Home,
    Plus,
    Unread,
    Forward,
    ReplyAll,
    Download,
    Settings,
    /// An empty tick box.
    Select,
}

fn body(icon: Icon) -> &'static str {
    match icon {
        Icon::Inbox => r#"<path d="M3 13h5l1.5 3h5L16 13h5"/><path d="M5.5 5h13L21 13v6H3v-6z"/>"#,
        Icon::Send => r#"<path d="M21 3L3 11l7 3 3 7z"/><path d="M10 14L21 3"/>"#,
        Icon::Star => {
            r#"<path d="M12 3l2.7 5.6 6.1.9-4.4 4.3 1 6.1L12 17l-5.5 2.9 1-6.1L3.2 9.5l6.1-.9z"/>"#
        }
        Icon::StarFilled => {
            r#"<path fill="black" d="M12 3l2.7 5.6 6.1.9-4.4 4.3 1 6.1L12 17l-5.5 2.9 1-6.1L3.2 9.5l6.1-.9z"/>"#
        }
        Icon::Paperclip => {
            r#"<path d="M20 11l-8.5 8.5a5 5 0 01-7-7L13 4a3.3 3.3 0 014.7 4.7l-8.5 8.5a1.7 1.7 0 01-2.4-2.4L15 7"/>"#
        }
        Icon::Trash => {
            r#"<path d="M4 7h16"/><path d="M9 7V4h6v3"/><path d="M6 7l1 13h10l1-13"/><path d="M10 11v6"/><path d="M14 11v6"/>"#
        }
        Icon::Archive => r#"<path d="M3 4h18v4H3z"/><path d="M5 8v12h14V8"/><path d="M10 12h4"/>"#,
        Icon::Refresh => {
            r#"<path d="M20 11a8 8 0 00-14-4L4 9"/><path d="M4 4v5h5"/><path d="M4 13a8 8 0 0014 4l2-2"/><path d="M20 20v-5h-5"/>"#
        }
        Icon::Search => r#"<circle cx="11" cy="11" r="7"/><path d="M21 21l-5-5"/>"#,
        Icon::Compose => {
            r#"<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z"/>"#
        }
        Icon::Reply => r#"<path d="M9 14L4 9l5-5"/><path d="M4 9h10a6 6 0 016 6v4"/>"#,
        Icon::Close => r#"<path d="M6 6l12 12"/><path d="M18 6L6 18"/>"#,
        Icon::Check => r#"<path d="M5 12l5 5L20 7"/>"#,
        Icon::Warning => r#"<path d="M12 3l10 18H2z"/><path d="M12 10v5"/><path d="M12 18v.5"/>"#,
        Icon::Folder => {
            r#"<path d="M3 6a2 2 0 012-2h4l2 2h8a2 2 0 012 2v10a2 2 0 01-2 2H5a2 2 0 01-2-2z"/>"#
        }
        Icon::User => r#"<circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0116 0"/>"#,
        Icon::Mail => {
            r#"<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M3 7l9 7 9-7"/>"#
        }
        Icon::Shield => r#"<path d="M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z"/>"#,
        Icon::Draft => {
            r#"<path d="M14 3H6a1 1 0 00-1 1v16a1 1 0 001 1h12a1 1 0 001-1V8z"/><path d="M14 3v5h5"/><path d="M9 13h6"/><path d="M9 17h4"/>"#
        }
        Icon::ChevronDown => r#"<path d="M6 9l6 6 6-6"/>"#,
        Icon::ChevronRight => r#"<path d="M9 6l6 6-6 6"/>"#,
        Icon::ChevronLeft => r#"<path d="M15 6l-6 6 6 6"/>"#,
        Icon::Home => r#"<path d="M4 11l8-7 8 7"/><path d="M6 10v10h4v-6h4v6h4V10"/>"#,
        Icon::Plus => r#"<path d="M12 5v14"/><path d="M5 12h14"/>"#,
        Icon::Unread => r#"<circle fill="black" cx="12" cy="12" r="5"/>"#,
        Icon::Forward => r#"<path d="M15 14l5-5-5-5"/><path d="M20 9H10a6 6 0 00-6 6v4"/>"#,
        Icon::ReplyAll => {
            r#"<path d="M7 17l-5-5 5-5"/><path d="M12 17l-5-5 5-5"/><path d="M22 18v-2a4 4 0 00-4-4H7"/>"#
        }
        Icon::Download => r#"<path d="M12 4v11"/><path d="M7 11l5 5 5-5"/><path d="M5 20h14"/>"#,
        Icon::Select => {
            r#"<rect x="4" y="4" width="16" height="16" rx="3.5"/><path d="M8.5 12.2l2.5 2.5 4.5-5"/>"#
        }
        Icon::Settings => {
            r#"<path d="M4 7h8"/><path d="M18 7h2"/><circle cx="15" cy="7" r="2.5"/><path d="M4 17h2"/><path d="M12 17h8"/><circle cx="9" cy="17" r="2.5"/>"#
        }
    }
}

/// The complete SVG document for an icon.
pub fn source(icon: Icon) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{}</svg>"#,
        body(icon)
    )
}

/// An icon of `size` logical pixels in `color`.
pub fn icon<'a>(icon: Icon, size: f32, color: Color) -> Svg<'a, Theme> {
    svg(svg::Handle::from_memory(source(icon).into_bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_, _| svg::Style { color: Some(color) })
}
