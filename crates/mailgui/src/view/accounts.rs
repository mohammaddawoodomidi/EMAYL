use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Length, Padding};

use super::sidebar::status_line;
use super::ui::{
    SEMIBOLD, danger_tool_button, fill_space, muted, primary_button, title, tool_button,
};
use crate::app::{App, Message};
use crate::icons::Icon;
use crate::theme;

/// "Change password", or "Sign in again" for accounts that sign in through a website.
fn sign_in_button<'a>(
    app: &App,
    account: &mailcore::account::Account,
    id: &str,
) -> Element<'a, Message> {
    let waiting = app
        .signing_in
        .as_ref()
        .is_some_and(|s| s.existing && s.account == account.id);
    match account.auth {
        mailcore::account::AuthMethod::Password => tool_button(
            Icon::Shield,
            "Change password",
            Some(Message::AskChangePassword(id.to_owned())),
        ),
        mailcore::account::AuthMethod::OAuth2 { .. } if waiting => row![
            muted("Waiting for you to finish signing in, in your web browser…"),
            tool_button(Icon::Close, "Cancel", Some(Message::WizCancelSignIn)),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
        mailcore::account::AuthMethod::OAuth2 { .. } => tool_button(
            Icon::Shield,
            "Sign in again",
            Some(Message::ResignIn(id.to_owned())),
        ),
    }
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut col = column![
        row![
            title("Accounts"),
            fill_space(),
            primary_button("Add account", Some(Message::ShowAddAccount))
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(10);
    if app.accounts.is_empty() {
        col = col.push(muted("No accounts yet."));
    }
    for a in &app.accounts {
        let id = a.account.id.as_str().to_owned();
        let (line, color) = status_line(&a.status);
        col = col.push(
            container(
                column![
                    row![
                        text(a.account.display_name.clone()).size(16).font(SEMIBOLD),
                        fill_space(),
                        text(line).size(13).color(color)
                    ]
                    .align_y(Alignment::Center),
                    muted(a.account.email.to_string()),
                    muted(format!(
                        "Incoming: {}:{} · Outgoing: {}:{}",
                        a.account.imap.host,
                        a.account.imap.port,
                        a.account.smtp.host,
                        a.account.smtp.port
                    )),
                    row![
                        tool_button(
                            Icon::Check,
                            "Check connection",
                            Some(Message::CheckAccount(id.clone()))
                        ),
                        sign_in_button(app, &a.account, &id),
                        fill_space(),
                        danger_tool_button(
                            Icon::Trash,
                            "Remove",
                            Some(Message::AskRemoveAccount(id))
                        ),
                    ]
                    .spacing(4),
                ]
                .spacing(6),
            )
            .padding(18)
            .width(Length::Fill)
            .style(theme::card),
        );
    }
    container(crate::theme::scrolling(
        container(col.spacing(14).max_width(760)).padding(Padding::from([28, 32])),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::page)
    .into()
}
