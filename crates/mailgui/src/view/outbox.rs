use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Length, Padding};
use mailcore::outbox::{OutboxItem, OutboxState, RecipientStatus, SentCopy};

use super::ui::{
    SEMIBOLD, banner, danger_tool_button, fill_space, muted, secondary_button, title, tool_button,
};
use crate::app::{App, Message};
use crate::compose_flow::{ComposeMsg, OutboxMsg};
use crate::icons::Icon;
use crate::theme::{self, Tone};
use crate::util::{clip, short_when};

fn recipients_text(item: &OutboxItem) -> String {
    let list: Vec<&str> = item.recipients.iter().map(|r| r.address.as_str()).collect();
    match list.as_slice() {
        [] => String::new(),
        [a] => format!("To: {a}"),
        [a, b] => format!("To: {a}, {b}"),
        [a, rest @ ..] => format!("To: {a} and {} more", rest.len()),
    }
}

/// What to tell the person about one message, and how serious it is.
pub fn status_of(item: &OutboxItem, now: i64, tz: i32) -> (String, Tone) {
    match item.state {
        OutboxState::Queued => ("Waiting to be sent".to_owned(), Tone::Info),
        OutboxState::Sending => ("Sending…".to_owned(), Tone::Info),
        OutboxState::Deferred => (
            format!(
                "Could not be sent yet. Next try: {}",
                short_when(item.next_attempt_at, now, tz)
            ),
            Tone::Warning,
        ),
        OutboxState::Sent => (
            format!(
                "Sent {}",
                short_when(item.sent_at.unwrap_or(item.created_at), now, tz)
            ),
            Tone::Good,
        ),
        OutboxState::Failed => ("Not sent".to_owned(), Tone::Danger),
        OutboxState::Bounced => (
            "Delivered to your mail provider, but it came back".to_owned(),
            Tone::Danger,
        ),
    }
}

fn card<'a>(item: &'a OutboxItem, now: i64, tz: i32) -> Element<'a, Message> {
    let (status, tone) = status_of(item, now, tz);
    let subject = if item.subject.trim().is_empty() {
        "(no subject)".to_owned()
    } else {
        clip(&item.subject, 70)
    };
    let mut info = column![
        text(subject).size(15).font(SEMIBOLD).color(theme::c().text),
        muted(recipients_text(item)),
        text(status).size(13).color(theme::tone_color(tone)),
    ]
    .spacing(3);
    if let Some(e) = &item.last_error
        && matches!(
            item.state,
            OutboxState::Failed | OutboxState::Deferred | OutboxState::Bounced
        )
    {
        info = info.push(text(e.advice.clone()).size(13).color(theme::c().text));
        if !e.server_text.is_empty() {
            info = info.push(
                text(format!("The server said: {}", clip(&e.server_text, 160)))
                    .size(12)
                    .color(theme::c().muted),
            );
        }
    }
    for r in item
        .recipients
        .iter()
        .filter(|r| r.status == RecipientStatus::Failed)
    {
        if let Some(n) = &r.note
            && item.recipients.len() > 1
        {
            info = info.push(
                text(format!("{}: {}", r.address, n.advice))
                    .size(12)
                    .color(theme::c().danger),
            );
        }
    }
    if item.state == OutboxState::Sent && item.sent_copy == SentCopy::GaveUp {
        info = info.push(muted("A copy could not be filed in your Sent folder."));
    }

    let id = item.id.clone();
    let actions: Element<'a, Message> = match item.state {
        OutboxState::Failed => row![
            secondary_button(
                "Try again",
                Some(Message::Outbox(OutboxMsg::Retry(id.clone())))
            ),
            danger_tool_button(
                Icon::Trash,
                "Discard",
                Some(Message::Outbox(OutboxMsg::Cancel(id)))
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
        OutboxState::Queued | OutboxState::Deferred => tool_button(
            Icon::Close,
            "Cancel sending",
            Some(Message::Outbox(OutboxMsg::Cancel(id))),
        ),
        OutboxState::Sent | OutboxState::Bounced if item.sent_copy != SentCopy::Pending => {
            tool_button(
                Icon::Check,
                "Dismiss",
                Some(Message::Outbox(OutboxMsg::Forget(id))),
            )
        }
        _ => fill_space(),
    };
    container(
        row![info.width(Length::Fill), actions]
            .spacing(12)
            .align_y(Alignment::Center),
    )
    .padding(16)
    .width(Length::Fill)
    .style(theme::card)
    .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut col = column![
        title("Outbox"),
        muted("Messages stay here until they are on their way. If something goes wrong, nothing is lost: you can try again."),
    ]
    .spacing(10);

    let (sending, done): (Vec<&OutboxItem>, Vec<&OutboxItem>) = app
        .outbox
        .iter()
        .partition(|i| !matches!(i.state, OutboxState::Sent));
    let mut sending = sending;
    sending.sort_by_key(|i| std::cmp::Reverse(i.created_at));
    let mut done = done;
    done.sort_by_key(|i| std::cmp::Reverse(i.sent_at.unwrap_or(i.created_at)));

    if sending.is_empty() && done.is_empty() && app.drafts.is_empty() {
        col = col.push(
            container(muted(
                "Nothing is waiting. Messages you send appear here until they have left.",
            ))
            .padding(Padding::from([30, 0])),
        );
    }
    if !sending.is_empty() {
        col = col.push(
            text("Waiting and problems")
                .size(14)
                .font(SEMIBOLD)
                .color(theme::c().text),
        );
        for i in sending {
            col = col.push(card(i, app.now, app.tz));
        }
    }
    if !app.drafts.is_empty() {
        col = col.push(
            text("Unfinished drafts")
                .size(14)
                .font(SEMIBOLD)
                .color(theme::c().text),
        );
        for d in &app.drafts {
            let subject = if d.subject.trim().is_empty() {
                "(no subject)".to_owned()
            } else {
                clip(&d.subject, 70)
            };
            let to = if d.first_recipient.is_empty() {
                "No recipient yet".to_owned()
            } else {
                format!("To: {}", d.first_recipient)
            };
            col = col.push(
                container(
                    row![
                        column![
                            text(subject).size(15).font(SEMIBOLD).color(theme::c().text),
                            muted(to),
                            muted(format!(
                                "Edited {}",
                                short_when(d.updated_at, app.now, app.tz)
                            )),
                        ]
                        .spacing(3)
                        .width(Length::Fill),
                        secondary_button(
                            "Continue writing",
                            Some(Message::Compose(ComposeMsg::OpenDraft(d.id.clone())))
                        ),
                        danger_tool_button(
                            Icon::Trash,
                            "Delete",
                            Some(Message::Compose(ComposeMsg::DeleteDraft(d.id.clone())))
                        ),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding(16)
                .width(Length::Fill)
                .style(theme::card),
            );
        }
    }
    if !done.is_empty() {
        col = col.push(
            text("Recently sent")
                .size(14)
                .font(SEMIBOLD)
                .color(theme::c().text),
        );
        for i in done {
            col = col.push(card(i, app.now, app.tz));
        }
    }
    if app.outbox.iter().any(|i| {
        i.state == OutboxState::Failed && i.last_error.as_ref().is_some_and(|e| e.account_problem)
    }) {
        col = col.push(banner(
            Tone::Warning,
            Icon::Warning,
            text("Some messages are waiting because an account needs attention. Fix the password under Accounts and they will be sent.").size(13).into(),
        ));
    }
    container(crate::theme::scrolling(
        container(col.spacing(12).max_width(820)).padding(Padding::from([28, 32])),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::page)
    .into()
}
