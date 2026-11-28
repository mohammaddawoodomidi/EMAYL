//! Small building blocks shared by the screens.

use iced::font::Weight;
use iced::widget::{button, column, container, row, space, text, text::Wrapping};
use iced::{Alignment, Color, Element, Font, Length, Padding};

use crate::app::Message;
use crate::icons::{Icon, icon};
use crate::theme;

pub const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};
pub const SEMIBOLD: Font = Font {
    weight: Weight::Semibold,
    ..Font::DEFAULT
};

pub fn muted<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(13).color(theme::c().muted)
}

pub fn title<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(22).font(SEMIBOLD).color(theme::c().text)
}

pub fn one_line<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).wrapping(Wrapping::None)
}

/// Text that may wrap, and that breaks inside a word when the word is wider
/// than the box (a long web address, a name with no spaces) instead of
/// running out of it.
pub fn wrapped<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).wrapping(Wrapping::WordOrGlyph)
}

/// One line that is cut at the edge of the space it is given.
pub fn clipped<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content).width(Length::Fill).clip(true).into()
}

pub fn fill_space<'a>() -> Element<'a, Message> {
    space().width(Length::Fill).into()
}

pub fn hline<'a>() -> Element<'a, Message> {
    container(space())
        .width(Length::Fill)
        .height(1)
        .style(theme::pane_edge)
        .into()
}

pub fn vline<'a>() -> Element<'a, Message> {
    container(space())
        .width(1)
        .height(Length::Fill)
        .style(theme::pane_edge)
        .into()
}

pub fn primary_button<'a>(label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    button(
        text(label)
            .size(14)
            .font(SEMIBOLD)
            .align_x(Alignment::Center),
    )
    .padding(Padding::from([10, 18]))
    .style(theme::primary)
    .on_press_maybe(on_press)
    .into()
}

pub fn secondary_button<'a>(label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(14).align_x(Alignment::Center))
        .padding(Padding::from([10, 18]))
        .style(theme::secondary)
        .on_press_maybe(on_press)
        .into()
}

pub fn danger_button<'a>(label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    button(
        text(label)
            .size(14)
            .font(SEMIBOLD)
            .align_x(Alignment::Center),
    )
    .padding(Padding::from([10, 18]))
    .style(theme::danger)
    .on_press_maybe(on_press)
    .into()
}

/// A flat button with an icon and a short label.
pub fn tool_button<'a>(
    ic: Icon,
    label: &'a str,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let tint = if on_press.is_some() {
        theme::c().muted
    } else {
        theme::c().faint
    };
    button(
        row![icon(ic, 16.0, tint), text(label).size(13)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding(Padding::from([7, 10]))
    .style(theme::ghost)
    .on_press_maybe(on_press)
    .into()
}

pub fn danger_tool_button<'a>(
    ic: Icon,
    label: &'a str,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    button(
        row![icon(ic, 16.0, theme::c().danger), text(label).size(13)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding(Padding::from([7, 10]))
    .style(theme::danger_ghost)
    .on_press_maybe(on_press)
    .into()
}

pub fn icon_only<'a>(ic: Icon, tint: Color, on_press: Option<Message>) -> Element<'a, Message> {
    button(icon(ic, 18.0, tint))
        .padding(7)
        .style(theme::ghost)
        .on_press_maybe(on_press)
        .into()
}

pub fn labelled<'a>(label: &'a str, input: Element<'a, Message>) -> Element<'a, Message> {
    column![
        text(label).size(13).font(SEMIBOLD).color(theme::c().text),
        input
    ]
    .spacing(6)
    .into()
}

/// A tinted message box with an icon.
pub fn banner<'a>(tone: theme::Tone, ic: Icon, body: Element<'a, Message>) -> Element<'a, Message> {
    container(
        row![icon(ic, 18.0, theme::tone_color(tone)), body]
            .spacing(10)
            .align_y(Alignment::Start),
    )
    .padding(12)
    .width(Length::Fill)
    .style(theme::banner(tone))
    .into()
}

pub fn avatar<'a>(initial: String, addr: &str, size: f32) -> Element<'a, Message> {
    container(
        text(initial)
            .size(size * 0.42)
            .font(SEMIBOLD)
            .color(Color::WHITE),
    )
    .width(size)
    .height(size)
    .center_x(size)
    .center_y(size)
    .style(theme::avatar(crate::util::avatar_color(addr)))
    .into()
}
