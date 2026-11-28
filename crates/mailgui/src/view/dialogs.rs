use iced::widget::{column, container, mouse_area, opaque, row, stack, text, text_input};
use iced::{Element, Font, Length};
use mailcore::render::LinkFlag;

use super::ui::{
    SEMIBOLD, banner, danger_button, fill_space, muted, primary_button, secondary_button,
};
use crate::app::{App, Dialog, Message};
use crate::compose_flow::{ComposeMsg, SendWarning};
use crate::icons::Icon;
use crate::theme::{self, Tone};
use crate::util::clip;

fn flag_text(f: &LinkFlag) -> String {
    match f {
        LinkFlag::TextShowsOtherSite { shown } => format!("The link text says “{}”, but it goes somewhere else.", clip(shown, 60)),
        LinkFlag::SpecialCharacters => "The address uses unusual characters that can imitate a well-known site.".into(),
        LinkFlag::MixedAlphabets => "The site name mixes different alphabets, a common sign of imitation.".into(),
        LinkFlag::NumericAddress => "The address is a number instead of a name.".into(),
        LinkFlag::HadCredentials => "The link contained a hidden user name and password (removed). This is often used to disguise the real site.".into(),
        LinkFlag::InsecureHttp => "This address is not encrypted.".into(),
    }
}

fn body<'a>(app: &'a App, d: &'a Dialog) -> Element<'a, Message> {
    match d {
        Dialog::Link(link) => {
            let web = link.url.starts_with("http://") || link.url.starts_with("https://");
            // Opening or copying never passes on the codes that tell the sender who clicked.
            let (address, removed) = mailcore::render::strip_tracking(&link.url);
            let mut col = column![text("Open this link?").size(20).font(SEMIBOLD)].spacing(12);
            if !link.host.is_empty() {
                col = col.push(column![muted("This link goes to"), text(link.host.clone()).size(18).font(SEMIBOLD)].spacing(2));
            }
            col = col.push(container(text(clip(&address, 220)).size(12).font(Font::MONOSPACE).color(theme::c().muted)).padding(10).width(Length::Fill).style(theme::code_block));
            if !removed.is_empty() {
                col = col.push(banner(Tone::Good, Icon::Shield, text(format!("Tracking codes removed from this link: {}.", clip(&removed.join(", "), 120))).size(13).into()));
            }
            for f in &link.flags {
                let tone = if matches!(f, LinkFlag::InsecureHttp) { Tone::Warning } else { Tone::Danger };
                col = col.push(banner(tone, Icon::Warning, text(flag_text(f)).size(13).into()));
            }
            let mut buttons = row![].spacing(8);
            if web {
                let label = if link.is_suspicious() { "Open anyway" } else { "Open in browser" };
                buttons = buttons.push(if link.is_suspicious() { danger_button(label, Some(Message::OpenLink(address.clone()))) } else { primary_button(label, Some(Message::OpenLink(address.clone()))) });
            }
            buttons = buttons.push(secondary_button("Copy address", Some(Message::CopyText(address.clone())))).push(fill_space()).push(secondary_button("Cancel", Some(Message::CloseDialog)));
            col.push(buttons).into()
        }
        Dialog::RemoveAccount(id) => {
            let name = app.account_state(id).map(|a| a.account.email.to_string()).unwrap_or_default();
            column![
                text("Remove this account?").size(20).font(SEMIBOLD),
                text(format!("{name} will be removed from this computer: its saved password, its downloaded mail copy, and any unsent messages and drafts written from it. Nothing is deleted on the mail server.")).size(14).color(theme::c().muted),
                row![danger_button("Remove", Some(Message::RemoveAccount(id.clone()))), secondary_button("Cancel", Some(Message::CloseDialog))].spacing(8),
            ]
            .spacing(14)
            .into()
        }
        Dialog::ChangePassword { account, input } => {
            let name = app.account_state(account).map(|a| a.account.email.to_string()).unwrap_or_default();
            column![
                text("Change password").size(20).font(SEMIBOLD),
                muted(format!("New password for {name}. If your provider uses app passwords, enter the app password.")),
                text_input("Password", input).on_input(Message::PasswordInput).on_submit(Message::SavePassword).secure(true).padding(11).size(15).style(theme::input),
                row![primary_button("Save and try again", if input.is_empty() { None } else { Some(Message::SavePassword) }), secondary_button("Cancel", Some(Message::CloseDialog))].spacing(8),
            ]
            .spacing(14)
            .into()
        }
        Dialog::Report { title, ok, text: report } => {
            let tone = if *ok { Tone::Good } else { Tone::Danger };
            column![
                text(title.clone()).size(20).font(SEMIBOLD),
                banner(tone, if *ok { Icon::Check } else { Icon::Warning }, text(if *ok { "Both servers answered correctly." } else { "Something is not working. The details below explain what to do." }).size(14).into()),
                container(crate::theme::scrolling(text(report.clone()).font(Font::MONOSPACE).size(12).color(theme::c().muted)).height(220)).padding(10).style(theme::code_block),
                row![secondary_button("Copy report", Some(Message::CopyText(report.clone()))), fill_space(), primary_button("Close", Some(Message::CloseDialog))].spacing(8),
            ]
            .spacing(14)
            .into()
        }
        Dialog::Info { title, text: t } => column![
            text(title.clone()).size(20).font(SEMIBOLD),
            text(t.clone()).size(14).color(theme::c().muted),
            row![fill_space(), primary_button("OK", Some(Message::CloseDialog))],
        ]
        .spacing(14)
        .into(),
        Dialog::ConfirmSend(warnings) => {
            let mut col = column![text("Send this message?").size(20).font(SEMIBOLD)].spacing(12);
            for w in warnings {
                col = col.push(banner(
                    Tone::Warning,
                    Icon::Warning,
                    text(match w {
                        SendWarning::NoSubject => "The subject is empty.",
                        SendWarning::ForgotAttachment => {
                            "You mention an attachment, but no file is attached."
                        }
                    })
                    .size(14)
                    .into(),
                ));
            }
            col.push(
                row![
                    primary_button("Send anyway", Some(Message::Compose(ComposeMsg::SendConfirmed))),
                    secondary_button("Go back", Some(Message::CloseDialog)),
                ]
                .spacing(8),
            )
            .into()
        }
        Dialog::DiscardDraft => column![
            text("Discard this message?").size(20).font(SEMIBOLD),
            text("What you wrote will be deleted. This cannot be undone.").size(14).color(theme::c().muted),
            row![
                danger_button("Discard", Some(Message::Compose(ComposeMsg::Discard))),
                secondary_button("Keep writing", Some(Message::CloseDialog)),
            ]
            .spacing(8),
        ]
        .spacing(14)
        .into(),
        Dialog::SaveRisky { message, index, name, disguised } => {
            let why = if *disguised {
                "The name of this file is made to hide what it really is. It is very likely harmful."
            } else {
                "This kind of file can run programs on your computer if you open it."
            };
            column![
                text("Save this file?").size(20).font(SEMIBOLD),
                text(clip(name, 80)).size(15).font(SEMIBOLD),
                banner(if *disguised { Tone::Danger } else { Tone::Warning }, Icon::Warning, text(why).size(14).into()),
                muted("Saving it does not run it. Only open it if you were expecting it from someone you trust."),
                row![
                    if *disguised {
                        danger_button("Save anyway", Some(Message::SaveConfirmed(*message, *index)))
                    } else {
                        primary_button("Save", Some(Message::SaveConfirmed(*message, *index)))
                    },
                    secondary_button("Cancel", Some(Message::CloseDialog)),
                ]
                .spacing(8),
            ]
            .spacing(12)
            .into()
        }
        Dialog::PermanentDelete(items) => column![
            text("Delete for good?").size(20).font(SEMIBOLD),
            text(format!("This account has no Trash folder, so {} cannot be moved there. Deleting removes it from the server for good.", if items.len() == 1 { "this message".to_owned() } else { format!("these {} messages", items.len()) })).size(14).color(theme::c().muted),
            row![danger_button("Delete for good", Some(Message::ConfirmPermanentDelete)), secondary_button("Cancel", Some(Message::CloseDialog))].spacing(8),
        ]
        .spacing(14)
        .into(),
    }
}

pub fn overlay<'a>(app: &'a App, d: &'a Dialog) -> Element<'a, Message> {
    let card = container(body(app, d))
        .padding(26)
        .width(480)
        .style(theme::dialog);
    stack![
        mouse_area(
            container(iced::widget::space())
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::scrim)
        )
        .on_press(Message::CloseDialog),
        container(opaque(card)).center(Length::Fill)
    ]
    .into()
}

/// "Sending in a moment... Undo", shown while a sent message can still be taken back.
pub fn undo_bar<'a>(u: &'a crate::app::UndoSend, now: i64) -> Element<'a, Message> {
    let left = (u.until - now).max(0);
    let what = if u.subject.trim().is_empty() {
        "Message".to_owned()
    } else {
        format!("“{}”", crate::util::clip(&u.subject, 30))
    };
    let label = if u.working {
        "Taking it back…".to_owned()
    } else {
        format!("{what} will be sent in about {left} s")
    };
    container(
        container(
            iced::widget::row![
                text(label).size(14),
                iced::widget::button(text("Undo").size(14))
                    .padding(iced::Padding::from([5, 14]))
                    .style(theme::primary)
                    .on_press_maybe(
                        (!u.working)
                            .then_some(Message::Compose(crate::compose_flow::ComposeMsg::Undo))
                    ),
            ]
            .spacing(16)
            .align_y(iced::Alignment::Center),
        )
        .padding(iced::Padding {
            top: 8.0,
            right: 12.0,
            bottom: 8.0,
            left: 18.0,
        })
        .style(theme::toast(false)),
    )
    .center_x(Length::Fill)
    .align_bottom(Length::Fill)
    .padding(24)
    .into()
}

pub fn toast<'a>(t: &'a crate::app::Toast) -> Element<'a, Message> {
    container(
        container(text(t.text.clone()).size(14))
            .padding(iced::Padding::from([10, 18]))
            .style(theme::toast(t.bad)),
    )
    .center_x(Length::Fill)
    .align_bottom(Length::Fill)
    .padding(24)
    .into()
}
