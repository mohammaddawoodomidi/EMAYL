use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Element, Length, Padding};
use mailcore::render::{AttachmentRisk, RenderedMessage, Severity};
use mailcore::store::MessageRow;

use super::richtext;
use super::ui::{
    SEMIBOLD, avatar, banner, clipped, danger_tool_button, fill_space, hline, muted, one_line,
    tool_button, wrapped,
};
use crate::app::{Action, App, BodyState, Message, Reader};
use crate::compose_flow::ComposeMsg;
use crate::icons::{Icon, icon};
use crate::theme::{self, Tone};
use crate::util::{clip, display_sender, initial, long_when, short_when, size_text};
use mailcore::replies::ReplyKind;

fn empty<'a>() -> Element<'a, Message> {
    container(
        column![
            icon(Icon::Mail, 56.0, theme::c().border),
            text("Select a message to read")
                .size(16)
                .color(theme::c().muted)
        ]
        .spacing(12)
        .align_x(Alignment::Center),
    )
    .center(Length::Fill)
    .into()
}

fn toolbar<'a>(r: &'a Reader, back: bool, in_junk: bool) -> Element<'a, Message> {
    let any = !r.messages.is_empty();
    let go = |m: Message| if any { Some(m) } else { None };
    let all_read = r.messages.iter().all(|m| !m.is_unread());
    let all_starred = !r.messages.is_empty() && r.messages.iter().all(MessageRow::is_flagged);
    let mut bar = row![].spacing(2).align_y(Alignment::Center);
    if back {
        bar = bar.push(tool_button(
            Icon::ChevronLeft,
            "Back",
            Some(Message::CloseReader),
        ));
    }
    bar.push(tool_button(
        Icon::Archive,
        "Archive",
        go(Message::Act(Action::Archive)),
    ))
    .push(danger_tool_button(
        Icon::Trash,
        "Delete",
        go(Message::Act(Action::Delete)),
    ))
    .push(if in_junk {
        tool_button(Icon::Inbox, "Not junk", go(Message::Act(Action::NotSpam)))
    } else {
        tool_button(Icon::Shield, "Junk", go(Message::Act(Action::Spam)))
    })
    .push(danger_tool_button(
        Icon::Close,
        "Block",
        go(Message::Data(crate::userdata_flow::DataMsg::BlockSender)),
    ))
    .push(fill_space())
    .push(tool_button(
        Icon::Unread,
        if all_read { "Mark unread" } else { "Mark read" },
        go(Message::Act(Action::ToggleRead)),
    ))
    .push(tool_button(
        if all_starred {
            Icon::StarFilled
        } else {
            Icon::Star
        },
        if all_starred { "Unstar" } else { "Star" },
        go(Message::Act(Action::ToggleStar)),
    ))
    .padding(Padding::from([8, 14]))
    .into()
}

fn tone_of(s: Severity) -> Tone {
    match s {
        Severity::Info => Tone::Info,
        Severity::Caution => Tone::Warning,
        Severity::Warning => Tone::Danger,
    }
}

fn people(list: &[mailcore::render::Person]) -> String {
    let names: Vec<String> = list
        .iter()
        .take(3)
        .map(|p| display_sender(&p.name, &p.address))
        .collect();
    let extra = list.len().saturating_sub(3);
    let mut s = names.join(", ");
    if extra > 0 {
        s.push_str(&format!(" +{extra}"));
    }
    s
}

fn collapsed<'a>(app: &'a App, m: &'a MessageRow) -> Element<'a, Message> {
    let who = display_sender(&m.from_name, &m.from_addr);
    button(
        row![
            avatar(initial(&m.from_name, &m.from_addr), &m.from_addr, 32.0),
            column![
                clipped(one_line(clip(&who, 60)).size(14).font(SEMIBOLD)),
                clipped(
                    one_line(clip(
                        if m.preview.is_empty() {
                            &m.subject
                        } else {
                            &m.preview
                        },
                        120
                    ))
                    .size(13)
                    .color(theme::c().muted)
                ),
            ]
            .spacing(2)
            .width(Length::Fill),
            text(short_when(m.date, app.now, app.tz))
                .size(12)
                .color(theme::c().muted),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding(Padding::from([10, 14]))
    .style(theme::row(false))
    .on_press(Message::ToggleExpand(m.id))
    .into()
}

fn attachments<'a>(rm: &'a RenderedMessage, message: i64) -> Option<Element<'a, Message>> {
    if rm.attachments.is_empty() {
        return None;
    }
    let mut col = column![].spacing(6);
    for a in &rm.attachments {
        let (note, tone) = match a.risk {
            AttachmentRisk::Normal => (None, Tone::Info),
            AttachmentRisk::Executable => (
                Some("This kind of file can run programs on your computer."),
                Tone::Warning,
            ),
            AttachmentRisk::DisguisedExecutable => (
                Some("The name hides what this file really is. Do not open it."),
                Tone::Danger,
            ),
        };
        let mut line = row![
            icon(Icon::Paperclip, 15.0, theme::c().muted),
            wrapped(clip(&a.filename, 60)).size(13).font(SEMIBOLD),
            muted(size_text(a.size)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(n) = note {
            line = line.push(text(n).size(12).color(theme::tone_color(tone)));
        }
        line = line.push(fill_space()).push(tool_button(
            Icon::Download,
            "Save",
            Some(Message::SaveAttachment(message, a.index)),
        ));
        col = col.push(
            container(line)
                .padding(Padding::from([7, 12]))
                .style(theme::chip),
        );
    }
    Some(col.into())
}

fn secondary_icon_button<'a>(
    ic: Icon,
    label: &'a str,
    kind: ReplyKind,
    main: bool,
) -> Element<'a, Message> {
    let tint = if main {
        iced::Color::WHITE
    } else {
        theme::c().text
    };
    button(
        row![icon(ic, 16.0, tint), text(label).size(14)]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding(Padding::from([9, 16]))
    .style(if main {
        theme::primary
    } else {
        theme::secondary
    })
    .on_press(Message::Compose(ComposeMsg::Reply(kind)))
    .into()
}

fn expanded<'a>(app: &'a App, m: &'a MessageRow) -> Element<'a, Message> {
    let state = app.bodies.get(&m.id);
    let rm = match state {
        Some(BodyState::Ready(rm)) => Some(rm.as_ref()),
        _ => None,
    };
    let (name, addr) = match rm.and_then(|r| r.headers.from.as_ref()) {
        Some(p) => (p.name.clone(), p.address.clone()),
        None => (m.from_name.clone(), m.from_addr.clone()),
    };
    let to_line = match rm {
        Some(r) if !r.headers.to.is_empty() => format!("to {}", people(&r.headers.to)),
        _ => clip(&format!("to {}", m.to), 80),
    };
    let when = match rm.and_then(|r| r.headers.date) {
        Some(d) => long_when(d, app.tz),
        None => long_when(m.date, app.tz),
    };
    // Who it is from, and when, are the first thing; the address and the recipients are
    // there for checking but quieter.
    let sender_line = if name.is_empty() {
        to_line
    } else {
        format!("{addr}  ·  {to_line}")
    };
    let head = button(
        row![
            avatar(initial(&name, &addr), &addr, 40.0),
            column![
                row![
                    clipped(
                        one_line(display_sender(&name, &addr))
                            .size(15)
                            .font(SEMIBOLD)
                            .color(theme::c().text)
                    ),
                    text(when).size(12).color(theme::c().muted),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
                clipped(one_line(sender_line).size(12).color(theme::c().muted)),
            ]
            .spacing(3)
            .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding(0)
    .style(|_, _| button::Style::default())
    .on_press(Message::ToggleExpand(m.id));
    // Copying the text is a small icon at the end of the header, not a block under the message.
    let copy: Element<'_, Message> = match rm {
        Some(r) => iced::widget::tooltip(
            button(icon(Icon::Draft, 16.0, theme::c().muted))
                .padding(Padding::from([7, 8]))
                .style(theme::ghost)
                .on_press(Message::CopyText(r.plain_text.clone())),
            container(text("Copy the text of this message").size(12))
                .padding(8)
                .style(theme::card),
            iced::widget::tooltip::Position::Left,
        )
        .into(),
        None => iced::widget::space().width(0).into(),
    };
    // Notes about the message (what was kept out, signature checks) stay out of sight until
    // asked for with this small icon. It appears only when there is something to show.
    let details_open = app
        .reader
        .as_ref()
        .is_some_and(|r| r.details_open.contains(&m.id));
    let quotes_open = app
        .reader
        .as_ref()
        .is_some_and(|r| r.quotes_open.contains(&m.id));
    let notes = rm.and_then(footer);
    let details: Element<'_, Message> = if notes.is_some() {
        iced::widget::tooltip(
            button(icon(
                Icon::Shield,
                16.0,
                if details_open {
                    theme::c().accent
                } else {
                    theme::c().muted
                },
            ))
            .padding(Padding::from([7, 8]))
            .style(theme::ghost)
            .on_press(Message::ToggleDetails(m.id)),
            container(
                text(if details_open {
                    "Hide the details"
                } else {
                    "Details about this message"
                })
                .size(12),
            )
            .padding(8)
            .style(theme::card),
            iced::widget::tooltip::Position::Left,
        )
        .into()
    } else {
        iced::widget::space().width(0).into()
    };
    let head = row![head, details, copy]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut col = column![head, hline()].spacing(16);
    match state {
        None | Some(BodyState::Loading) => col = col.push(muted("Opening…")),
        Some(BodyState::Failed(e)) => {
            col = col.push(banner(
                Tone::Danger,
                Icon::Warning,
                text(e.clone()).size(14).into(),
            ))
        }
        Some(BodyState::Ready(rm)) => {
            // Only things that should make the person stop and think are shown above the
            // message. Everything that is merely good to know goes quietly below it.
            let mut warnings: Vec<_> = rm
                .notices
                .iter()
                .filter(|n| n.severity != Severity::Info)
                .collect();
            warnings.sort_by_key(|n| std::cmp::Reverse(n.severity));
            for n in warnings {
                col = col.push(banner(
                    tone_of(n.severity),
                    Icon::Warning,
                    text(n.text.clone()).size(13).into(),
                ));
            }
            if details_open && let Some(f) = notes {
                col = col.push(f);
            }
            col = col.push(richtext::body(rm, m.id, quotes_open));
            if let Some(a) = attachments(rm, m.id) {
                col = col.push(a);
            }
        }
    }
    container(col)
        .padding(Padding::from([20, 24]))
        .width(Length::Fill)
        .style(theme::card)
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let Some(r) = &app.reader else {
        return container(empty())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::page)
            .into();
    };
    let subject = r
        .messages
        .iter()
        .rev()
        .map(|m| m.subject.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("(no subject)")
        .to_owned();
    let mut cards = column![].spacing(12);
    // The newest message comes first; older ones follow below it.
    for m in r.messages.iter().rev() {
        if r.expanded.contains(&m.id) {
            cards = cards.push(expanded(app, m));
        } else {
            cards = cards.push(container(collapsed(app, m)).style(theme::card));
        }
    }
    let mut chips = row![].spacing(8).align_y(Alignment::Center);
    if r.messages.len() > 1 {
        chips = chips.push(chip(format!("{} messages", r.messages.len())));
    }
    if app.accounts.len() > 1
        && let Some(m) = r.messages.first()
    {
        chips = chips.push(chip(clip(&m.account, 40)));
    }
    let head = column![
        wrapped(subject)
            .size(24)
            .font(SEMIBOLD)
            .color(theme::c().text)
            .width(Length::Fill),
        chips,
    ]
    .spacing(10);
    let reply_bar = container(
        row![
            secondary_icon_button(Icon::Reply, "Reply", ReplyKind::Reply, true),
            secondary_icon_button(Icon::ReplyAll, "Reply all", ReplyKind::ReplyAll, false),
            secondary_icon_button(Icon::Forward, "Forward", ReplyKind::Forward, false),
        ]
        .spacing(8)
        .wrap(),
    )
    .padding(Padding::from([10, 28]))
    .width(Length::Fill);
    let content = column![head, cards]
        .spacing(18)
        .padding(Padding::from([24, 28]))
        .max_width(860);
    container(column![
        toolbar(r, app.narrow(), in_junk(app, r)),
        hline(),
        crate::theme::scrolling(container(content).center_x(Length::Fill)).height(Length::Fill),
        hline(),
        reply_bar,
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::page)
    .into()
}

/// True when the open conversation is being read in a Junk folder.
fn in_junk(app: &App, r: &Reader) -> bool {
    r.messages.first().is_some_and(|m| {
        app.folders.get(&m.account).is_some_and(|fs| {
            fs.iter()
                .any(|f| f.id == m.folder_id && f.role == mailcore::imap::FolderRole::Junk)
        })
    })
}

fn chip<'a>(label: String) -> Element<'a, Message> {
    container(text(label).size(12))
        .padding(Padding::from([3, 10]))
        .style(theme::chip)
        .into()
}

/// What sits under a message: quiet notes (privacy, signed or encrypted mail), so that none
/// of it competes with the message itself. `None` when there is nothing to say.
fn footer(rm: &RenderedMessage) -> Option<Element<'_, Message>> {
    let note = |ic: Icon, tint: iced::Color, t: String| -> Element<'_, Message> {
        row![
            icon(ic, 13.0, tint),
            wrapped(t)
                .size(12)
                .color(theme::c().muted)
                .width(Length::Fill)
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    };
    let mut notes = column![].spacing(4).width(Length::Fill);
    let mut any = false;
    for n in rm.notices.iter().filter(|n| {
        n.severity == Severity::Info && n.kind != mailcore::render::NoticeKind::PicturesWithheld
    }) {
        notes = notes.push(note(Icon::Shield, theme::c().muted, n.text.clone()));
        any = true;
    }
    if let Some((short, long)) = privacy_note(rm) {
        notes = notes.push(iced::widget::tooltip(
            note(Icon::Shield, theme::c().ok, short),
            container(wrapped(long).size(12))
                .padding(10)
                .max_width(340)
                .style(theme::card),
            iced::widget::tooltip::Position::Top,
        ));
        any = true;
    }
    any.then(|| notes.into())
}

/// What was kept from reaching the sender: a short line to show, and the full explanation for
/// the tooltip. `None` when there was nothing.
fn privacy_note(rm: &RenderedMessage) -> Option<(String, String)> {
    let s = rm.tracker_summary();
    if s.is_empty() {
        return None;
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut short: Vec<String> = Vec::new();
    let mut long: Vec<String> = Vec::new();
    if s.pictures > 0 {
        short.push(format!(
            "{} not loaded",
            plural(s.pictures, "picture", "pictures")
        ));
        long.push(format!(
            "{} not loaded{}.",
            plural(s.pictures, "picture was", "pictures were"),
            if s.tracking_pictures > 0 {
                format!(" ({} looked like tracking)", s.tracking_pictures)
            } else {
                String::new()
            }
        ));
    }
    if s.tracking_links > 0 {
        short.push(format!(
            "{} cleaned",
            plural(s.tracking_links, "tracking link", "tracking links")
        ));
        long.push(format!(
            "{} with tracking codes will be cleaned when opened.",
            plural(s.tracking_links, "link", "links")
        ));
    }
    long.push("These cannot be used to tell the sender that you opened the message.".to_owned());
    if !s.hosts.is_empty() {
        long.push(format!("Would have contacted: {}.", s.hosts.join(", ")));
    }
    Some((format!("Private: {}", short.join(" · ")), long.join(" ")))
}
