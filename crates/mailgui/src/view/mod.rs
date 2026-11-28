//! Drawing the window from the application state.

mod accounts;
mod compose;
mod dialogs;
mod filters;
mod list;
mod outbox;
mod reader;
mod richtext;
mod settings;
mod sidebar;
mod templates;
mod ui;
mod wizard;

pub use sidebar::status_line;

use iced::widget::{button, column, container, row, stack, text};
use iced::{Alignment, Element, Length};

use crate::app::{App, Message, Screen};
use crate::theme;

fn mailbox(app: &App) -> Element<'_, Message> {
    if app.narrow() {
        // One pane at a time: the list, or the open message with a way back.
        return if app.reader.is_some() {
            reader::view(app)
        } else {
            row![sidebar::view(app), ui::vline(), list::view(app)]
                .height(Length::Fill)
                .into()
        };
    }
    row![
        sidebar::view(app),
        ui::vline(),
        list::view(app),
        ui::vline(),
        reader::view(app)
    ]
    .height(Length::Fill)
    .into()
}

fn with_sidebar<'a>(app: &'a App, content: Element<'a, Message>) -> Element<'a, Message> {
    row![
        sidebar::view(app),
        ui::vline(),
        column![back_bar(app), ui::hline(), content]
            .width(Length::Fill)
            .height(Length::Fill)
    ]
    .height(Length::Fill)
    .into()
}

/// The way home, on every screen that is not the mail list itself.
fn back_bar(app: &App) -> Element<'_, Message> {
    let (label, go) = match app.screen {
        Screen::AddAccount { .. } if !app.accounts.is_empty() => {
            ("Back to mail", Message::BackToMailbox)
        }
        Screen::Filters | Screen::Templates => ("Back to settings", Message::ShowSettings),
        _ => ("Back to mail", Message::ShowMailbox),
    };
    container(
        button(
            row![
                crate::icons::icon(crate::icons::Icon::ChevronLeft, 16.0, theme::c().accent),
                text(label)
                    .size(14)
                    .font(ui::SEMIBOLD)
                    .color(theme::c().accent)
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding(iced::Padding::from([6, 12]))
        .style(theme::ghost)
        .on_press(go),
    )
    .padding(iced::Padding::from([6, 12]))
    .width(Length::Fill)
    .into()
}

fn fatal(text_: &str) -> Element<'_, Message> {
    container(
        container(
            column![
                text("EMAYL cannot start").size(24).font(ui::SEMIBOLD),
                text(text_).size(14).color(theme::c().muted),
                text("Your mail on the server is not affected. Closing and reopening the program may help; if it keeps happening, check that your user folder is writable and the disk is not full.").size(14).color(theme::c().muted),
            ]
            .spacing(14)
            .align_x(Alignment::Start),
        )
        .padding(32)
        .width(520)
        .style(theme::card),
    )
    .center(Length::Fill)
    .style(theme::page)
    .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    // Every frame starts by choosing the colours, so a change of theme shows at once.
    theme::use_colors(app.colors());
    if let Some(f) = &app.fatal {
        return fatal(f);
    }
    let base: Element<'_, Message> = match &app.screen {
        Screen::Mailbox => mailbox(app),
        Screen::AddAccount { first: true } => wizard::view(app, true),
        Screen::AddAccount { first: false } => with_sidebar(app, wizard::view(app, false)),
        Screen::Accounts => with_sidebar(app, accounts::view(app)),
        Screen::Compose => with_sidebar(app, compose::view(app)),
        Screen::Outbox => with_sidebar(app, outbox::view(app)),
        Screen::Settings => with_sidebar(app, settings::view(app)),
        Screen::Filters => with_sidebar(app, filters::view(app)),
        Screen::Templates => with_sidebar(app, templates::view(app)),
    };
    let mut layers = stack![
        container(base)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::page)
    ];
    if let Some(d) = &app.dialog {
        layers = layers.push(dialogs::overlay(app, d));
    }
    if let Some(u) = &app.undo {
        layers = layers.push(dialogs::undo_bar(u, app.now));
    }
    if let Some(t) = &app.toast {
        layers = layers.push(dialogs::toast(t));
    }
    layers.into()
}
