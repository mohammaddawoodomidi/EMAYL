use std::time::Duration;

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Element, Length, Padding};
use mailcore::imap::FolderRole;
use mailcore::store::FolderRow;
use mailcore::syncer::{SyncProgress, SyncStatus};

use super::ui::{SEMIBOLD, fill_space, one_line};
use crate::app::{App, Message, Nav, Screen};
use crate::compose_flow::{ComposeMsg, OutboxMsg, outbox_counts};
use crate::icons::{Icon, icon};
use crate::theme;
use crate::util::initial;

pub const WIDTH: f32 = 236.0;

fn role_icon(role: FolderRole) -> Icon {
    match role {
        FolderRole::Inbox => Icon::Inbox,
        FolderRole::Sent => Icon::Send,
        FolderRole::Drafts => Icon::Draft,
        FolderRole::Archive | FolderRole::All => Icon::Archive,
        FolderRole::Trash => Icon::Trash,
        FolderRole::Junk => Icon::Shield,
        _ => Icon::Folder,
    }
}

fn role_rank(role: FolderRole) -> u8 {
    match role {
        FolderRole::Inbox => 0,
        FolderRole::Drafts => 1,
        FolderRole::Sent => 2,
        FolderRole::Archive => 3,
        FolderRole::All => 4,
        FolderRole::Junk => 5,
        FolderRole::Trash => 6,
        _ => 7,
    }
}

/// Folders worth showing, in a familiar order.
pub fn visible_folders(folders: &[FolderRow]) -> Vec<&FolderRow> {
    let mut v: Vec<&FolderRow> = folders
        .iter()
        .filter(|f| f.selectable && f.role != FolderRole::Flagged)
        .collect();
    v.sort_by(|a, b| {
        role_rank(a.role).cmp(&role_rank(b.role)).then_with(|| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        })
    });
    v
}

pub fn folder_label(f: &FolderRow) -> String {
    if f.role == FolderRole::Inbox {
        "Inbox".to_owned()
    } else {
        mailcore::imap::folder::pretty_name(&f.display_name)
    }
}

/// Short text and a colour for the account's connection state.
pub fn status_line(s: &SyncStatus) -> (String, iced::Color) {
    match s {
        SyncStatus::Starting | SyncStatus::Connecting => ("Connecting…".into(), theme::c().warn),
        SyncStatus::Syncing(p) => (
            match short_progress(p) {
                Some(t) => t,
                None => "Syncing…".into(),
            },
            theme::c().accent,
        ),
        SyncStatus::Online {
            pending_actions, ..
        } => {
            if *pending_actions > 0 {
                (
                    format!("Sending {pending_actions} change(s)…"),
                    theme::c().accent,
                )
            } else {
                ("Up to date".into(), theme::c().ok)
            }
        }
        SyncStatus::Offline { retry_in, .. } => (
            format!("Offline · retrying in {}", wait_text(*retry_in)),
            theme::c().faint,
        ),
        SyncStatus::NeedsAttention { .. } => ("Needs your attention".into(), theme::c().danger),
        SyncStatus::Stopped => ("Stopped".into(), theme::c().faint),
    }
}

/// "Downloading 340 of 1,000" while a folder is being filled.
fn short_progress(p: &SyncProgress) -> Option<String> {
    (p.messages_total > 0).then(|| {
        format!(
            "Downloading {} of {}",
            group(p.messages_done),
            group(p.messages_total)
        )
    })
}

/// The longer line for the empty list: which folder, how far, how many folders.
pub fn progress_text(p: &SyncProgress) -> Option<String> {
    if p.folder.is_empty() {
        return None;
    }
    let name = if p.folder.eq_ignore_ascii_case("inbox") {
        "Inbox"
    } else {
        p.folder.as_str()
    };
    let mut t = format!("Downloading {name}");
    if p.messages_total > 0 {
        t.push_str(&format!(
            ": {} of {} messages",
            group(p.messages_done),
            group(p.messages_total)
        ));
    }
    if p.folders_total > 1 {
        t.push_str(&format!(
            " · folder {} of {}",
            (p.folders_done + 1).min(p.folders_total),
            p.folders_total
        ));
    }
    Some(t)
}

/// 1234 -> "1,234".
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn wait_text(d: Duration) -> String {
    let s = d.as_secs();
    if s < 90 {
        format!("{s}s")
    } else {
        format!("{} min", s.div_ceil(60))
    }
}

fn badge<'a>(n: u32) -> Element<'a, Message> {
    if n == 0 {
        return fill_space();
    }
    container(
        text(if n > 999 {
            "999+".to_owned()
        } else {
            n.to_string()
        })
        .size(11)
        .font(SEMIBOLD)
        .color(theme::c().accent),
    )
    .padding(Padding::from([1, 7]))
    .style(|_| iced::widget::container::Style {
        background: Some(iced::Background::Color(theme::c().accent_soft)),
        border: iced::Border {
            radius: 999.0.into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .into()
}

fn nav_item<'a>(
    ic: Icon,
    label: String,
    count: u32,
    selected: bool,
    account: Option<iced::Color>,
    on_press: Message,
) -> Element<'a, Message> {
    let tint = account.unwrap_or(if selected {
        theme::c().accent
    } else {
        theme::c().muted
    });
    button(
        row![
            icon(ic, 17.0, tint),
            one_line(label).size(14).font(if selected || count > 0 {
                SEMIBOLD
            } else {
                iced::Font::DEFAULT
            }),
            fill_space(),
            badge(count)
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding(Padding::from([8, 10]))
    .style(theme::row(selected))
    .on_press(on_press)
    .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut col = column![].spacing(2).padding(Padding::from([0, 10]));
    // Home is always there, so there is always a way back to the mail.
    col = col.push(nav_item(
        if app.accounts.len() > 1 {
            Icon::Inbox
        } else {
            Icon::Home
        },
        if app.accounts.len() > 1 {
            "All inboxes".into()
        } else {
            "Home".into()
        },
        app.unread_inboxes(),
        app.nav == Nav::AllInboxes && app.screen == Screen::Mailbox,
        None,
        Message::Go(Nav::AllInboxes),
    ));
    let several = app.accounts.len() > 1;
    for (index, a) in app.accounts.iter().enumerate() {
        let id = a.account.id.as_str();
        let (status, color) = status_line(&a.status);
        // With several accounts each one has its own colour and a round mark with its first
        // letter; its folders carry the same colour, so it is clear whose folder is whose.
        let mark_color = crate::util::account_color(index);
        let mark: Element<'_, Message> = container(
            text(initial(
                &a.account.display_name,
                &a.account.email.to_string(),
            ))
            .size(13)
            .font(SEMIBOLD)
            .color(iced::Color::WHITE),
        )
        .width(26)
        .height(26)
        .center_x(26)
        .center_y(26)
        .style(theme::dot(mark_color))
        .into();
        col = col.push(
            row![
                mark,
                column![
                    one_line(a.account.display_name.clone())
                        .size(13)
                        .font(SEMIBOLD)
                        .color(theme::c().text),
                    one_line(a.account.email.to_string())
                        .size(12)
                        .color(theme::c().muted),
                    one_line(status).size(12).color(color),
                ]
                .spacing(2)
            ]
            .spacing(10)
            .padding(Padding {
                top: 14.0,
                left: 10.0,
                bottom: 6.0,
                right: 4.0,
            }),
        );
        if let Some(folders) = app.folders.get(id) {
            for f in visible_folders(folders) {
                let selected = app.screen == Screen::Mailbox
                    && app.nav
                        == (Nav::Folder {
                            account: id.to_owned(),
                            folder_id: f.id,
                        });
                let count = if matches!(
                    f.role,
                    FolderRole::Sent | FolderRole::Trash | FolderRole::Junk
                ) {
                    0
                } else {
                    f.unread
                };
                col = col.push(nav_item(
                    role_icon(f.role),
                    folder_label(f),
                    count,
                    selected,
                    several.then_some(mark_color),
                    Message::Go(Nav::Folder {
                        account: id.to_owned(),
                        folder_id: f.id,
                    }),
                ));
            }
        }
    }

    let top = row![
        icon(Icon::Mail, 22.0, theme::c().accent),
        text("EMAYL").size(18).font(SEMIBOLD).color(theme::c().text)
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 18.0,
        left: 20.0,
        bottom: 14.0,
        right: 10.0,
    });

    let (waiting, problems) = outbox_counts(&app.outbox);
    let outbox_count = u32::try_from(waiting + problems + app.drafts.len()).unwrap_or(u32::MAX);
    let outbox_item = nav_item(
        if problems > 0 {
            Icon::Warning
        } else {
            Icon::Send
        },
        "Outbox".to_owned(),
        outbox_count,
        app.screen == Screen::Outbox,
        None,
        Message::Outbox(OutboxMsg::Show),
    );
    let writing: Element<'_, Message> = match &app.compose {
        Some(c) if app.screen != Screen::Compose => nav_item(
            Icon::Draft,
            if c.subject.trim().is_empty() {
                "Message being written".to_owned()
            } else {
                crate::util::clip(&c.subject, 24)
            },
            0,
            false,
            None,
            Message::Compose(ComposeMsg::Show),
        ),
        _ => iced::widget::space().height(0).into(),
    };
    let write_button = container(
        button(
            row![
                icon(Icon::Compose, 16.0, theme::WHITE),
                text("Write").size(14).font(SEMIBOLD)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding(Padding::from([10, 14]))
        .style(theme::primary)
        .on_press_maybe((!app.accounts.is_empty()).then_some(Message::Compose(ComposeMsg::New))),
    )
    .padding(Padding {
        top: 0.0,
        right: 10.0,
        bottom: 8.0,
        left: 10.0,
    });

    let bottom = column![
        writing,
        outbox_item,
        button(
            row![
                icon(Icon::Plus, 16.0, theme::c().muted),
                text("Add account").size(13)
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        )
        .width(Length::Fill)
        .padding(Padding::from([8, 10]))
        .style(theme::row(matches!(app.screen, Screen::AddAccount { .. })))
        .on_press(Message::ShowAddAccount),
        button(
            row![
                icon(Icon::User, 16.0, theme::c().muted),
                text("Accounts").size(13)
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        )
        .width(Length::Fill)
        .padding(Padding::from([8, 10]))
        .style(theme::row(app.screen == Screen::Accounts))
        .on_press(Message::ShowAccounts),
        button(
            row![
                icon(Icon::Settings, 16.0, theme::c().muted),
                text("Settings").size(13)
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        )
        .width(Length::Fill)
        .padding(Padding::from([8, 10]))
        .style(theme::row(app.screen == Screen::Settings))
        .on_press(Message::ShowSettings),
    ]
    .spacing(2)
    .padding(Padding::from([10, 10]));

    container(column![
        top,
        write_button,
        crate::theme::scrolling(col).height(Length::Fill),
        bottom
    ])
    .width(WIDTH)
    .height(Length::Fill)
    .style(theme::sidebar)
    .into()
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn p(folder: &str, done: usize, total: usize, fd: usize, ft: usize) -> SyncProgress {
        SyncProgress {
            folder: folder.into(),
            folders_done: fd,
            folders_total: ft,
            messages_done: done,
            messages_total: total,
            stored_total: done,
        }
    }

    #[test]
    fn numbers_get_thousands_separators() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(1234567), "1,234,567");
    }

    #[test]
    fn progress_lines_read_naturally() {
        assert_eq!(
            progress_text(&p("INBOX", 340, 1000, 0, 4)).as_deref(),
            Some("Downloading Inbox: 340 of 1,000 messages · folder 1 of 4")
        );
        assert_eq!(progress_text(&SyncProgress::default()), None);
        assert_eq!(
            short_progress(&p("INBOX", 5, 10, 0, 1)).as_deref(),
            Some("Downloading 5 of 10")
        );
        assert_eq!(short_progress(&p("INBOX", 0, 0, 0, 1)), None);
        // Never "folder 5 of 4".
        assert_eq!(
            progress_text(&p("Sent", 1, 2, 4, 4)).as_deref(),
            Some("Downloading Sent: 1 of 2 messages · folder 4 of 4")
        );
    }
}
