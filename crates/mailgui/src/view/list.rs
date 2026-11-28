use iced::widget::{button, column, container, row, text, text_input, toggler};
use iced::{Alignment, Element, Length, Padding};
use mailcore::store::{ListScope, SearchHit, ThreadRow};
use mailcore::syncer::SyncStatus;

use super::sidebar::{folder_label, status_line};
use super::ui::{
    SEMIBOLD, avatar, banner, danger_tool_button, fill_space, hline, icon_only, muted, one_line,
    secondary_button, title, tool_button,
};
use crate::app::{Action, App, Message, Nav};
use crate::icons::{Icon, icon};
use crate::theme::{self, Tone};
use crate::util::{clip, display_sender, initial, short_when};

fn heading(app: &App) -> String {
    match &app.nav {
        Nav::AllInboxes => "All inboxes".to_owned(),
        Nav::Folder { .. } => app
            .current_folder()
            .map_or_else(|| "Mail".to_owned(), folder_label),
    }
}

/// Space between two rows of the list.
const ROW_GAP: f32 = 2.0;

/// Every conversation row has the same height, which is what lets the list draw only the rows
/// that can be seen. The numbers leave a little room above the text, which is at most
/// 56 px high with previews and 37 px without (three lines at 14/13/13 px, or two).
fn row_height(app: &App) -> f32 {
    match (app.prefs.look.previews, app.prefs.look.compact) {
        (true, false) => 78.0,
        (true, true) => 68.0,
        (false, false) => 60.0,
        (false, true) => 50.0,
    }
}

fn centered<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .width(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

/// Rows that are drawn above and below what is on screen, so a quick scroll never shows a gap.
const OVERSCAN: usize = 8;
/// A window is never assumed to be shorter than this many logical pixels, because the real
/// height is only learned from the first scroll.
const MIN_VIEWPORT: f32 = 2000.0;

/// The part of `total` rows to draw for the given scroll position: `(first, end)`.
pub fn visible_rows(total: usize, row: f32, scroll: f32, viewport: f32) -> (usize, usize) {
    if total == 0 || row <= 0.0 {
        return (0, 0);
    }
    let stride = row + ROW_GAP;
    let top = scroll.max(0.0);
    let first = ((top / stride).floor() as usize).saturating_sub(OVERSCAN);
    let seen = viewport.max(MIN_VIEWPORT);
    let end = (((top + seen) / stride).ceil() as usize)
        .saturating_add(OVERSCAN)
        .min(total);
    (first.min(end), end)
}

/// A blank block that stands in for `rows` rows that are not drawn.
fn gap<'a>(rows: usize, row: f32) -> Element<'a, Message> {
    let h = rows as f32 * (row + ROW_GAP) - ROW_GAP;
    iced::widget::space().height(h).into()
}

fn thread_row<'a>(app: &'a App, t: &'a ThreadRow) -> Element<'a, Message> {
    let selected = app
        .reader
        .as_ref()
        .is_some_and(|r| r.thread_key == t.thread_key);
    let unread = t.unread > 0;
    let who = if t.participants.is_empty() {
        "(unknown)".to_owned()
    } else {
        t.participants.join(", ")
    };
    let first = t.participants.first().cloned().unwrap_or_default();
    let name_font = if unread {
        SEMIBOLD
    } else {
        iced::Font::DEFAULT
    };
    let count = if t.count > 1 {
        format!("  {}", t.count)
    } else {
        String::new()
    };
    let mut meta = row![].spacing(6).align_y(Alignment::Center);
    if t.maybe_attachments {
        meta = meta.push(icon(Icon::Paperclip, 13.0, theme::c().faint));
    }
    if t.flagged {
        meta = meta.push(icon(Icon::StarFilled, 13.0, theme::c().warn));
    }
    meta = meta.push(
        text(short_when(t.newest_date, app.now, app.tz))
            .size(12)
            .color(if unread {
                theme::c().accent
            } else {
                theme::c().muted
            }),
    );
    let subject = if t.subject.trim().is_empty() {
        "(no subject)".to_owned()
    } else {
        t.subject.clone()
    };
    // With several accounts, "All inboxes" says which account each conversation belongs to.
    let account_tag: Option<Element<'_, Message>> = (app.accounts.len() > 1
        && app.nav == Nav::AllInboxes)
        .then(|| {
            app.accounts
                .iter()
                .position(|a| a.account.id.as_str() == t.account)
                .map(|index| {
                    let email = app.accounts[index].account.email.to_string();
                    row![
                        container(iced::widget::space())
                            .width(8)
                            .height(8)
                            .style(theme::dot(crate::util::account_color(index))),
                        one_line(clip(&email, 18)).size(11).color(theme::c().muted),
                    ]
                    .spacing(4)
                    .align_y(Alignment::Center)
                    .into()
                })
        })
        .flatten();
    let body = column![
        row![
            one_line(clip(
                &format!("{who}{count}"),
                if app.selecting { 28 } else { 34 }
            ))
            .size(14)
            .font(name_font)
            .color(theme::c().text),
            fill_space(),
            meta
        ]
        .align_y(Alignment::Center),
        row![
            container(
                one_line(clip(
                    &subject,
                    match (app.selecting, account_tag.is_some()) {
                        (false, false) => 44,
                        (true, false) => 38,
                        (false, true) => 30,
                        (true, true) => 24,
                    }
                ))
                .size(13)
                .font(if unread {
                    SEMIBOLD
                } else {
                    iced::Font::DEFAULT
                })
                .color(theme::c().text)
            )
            .width(Length::Fill)
            .clip(true),
        ]
        .push(account_tag.unwrap_or_else(|| iced::widget::space().width(0).into()))
        .spacing(6)
        .align_y(Alignment::Center),
    ]
    .extend(
        (app.prefs.look.previews && !t.preview.trim().is_empty()).then(
            || -> Element<'_, Message> {
                container(
                    one_line(clip(&t.preview, if app.selecting { 42 } else { 50 }))
                        .size(13)
                        .color(theme::c().muted),
                )
                .width(Length::Fill)
                .clip(true)
                .into()
            },
        ),
    )
    .spacing(2)
    .width(Length::Fill);
    if app.selecting {
        let ticked = app.selected.contains(&t.thread_key);
        return button(centered(
            row![
                tick_box(ticked),
                avatar(initial(&first, &first), &first, 36.0),
                body
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        ))
        .width(Length::Fill)
        .height(Length::Fixed(row_height(app)))
        .padding(Padding::from([0, 10]))
        .style(theme::row(ticked))
        .on_press(Message::ToggleSelect(t.thread_key.clone()))
        .into();
    }
    let dot: Element<'_, Message> = if unread {
        container(iced::widget::space())
            .width(8)
            .height(8)
            .style(theme::dot(theme::c().accent))
            .into()
    } else {
        container(iced::widget::space()).width(8).height(8).into()
    };
    button(centered(
        row![dot, avatar(initial(&first, &first), &first, 36.0), body]
            .spacing(10)
            .align_y(Alignment::Center),
    ))
    .width(Length::Fill)
    .height(Length::Fixed(row_height(app)))
    .padding(Padding::from([0, 10]))
    .style(theme::row(selected))
    .on_press(Message::OpenThread(t.thread_key.clone(), scope_for(app)))
    .into()
}

fn tick_box<'a>(on: bool) -> Element<'a, Message> {
    container(if on {
        Element::from(icon(Icon::Check, 14.0, iced::Color::WHITE))
    } else {
        iced::widget::space().into()
    })
    .width(22)
    .height(22)
    .center(22)
    .style(theme::tick_box(on))
    .into()
}

/// The top of the list while conversations are being chosen: what is chosen, and what to do.
fn selection_head(app: &App) -> Element<'_, Message> {
    let n = app.selected.len();
    let none = n == 0;
    let go = |a: Action| if none { None } else { Some(Message::Bulk(a)) };
    let count = if none {
        "Choose conversations".to_owned()
    } else if n == 1 {
        "1 selected".to_owned()
    } else {
        format!("{n} selected")
    };
    let all_picked = !app.threads.is_empty() && n >= app.threads.len();
    column![
        row![
            tool_button(Icon::Close, "Done", Some(Message::SelectMode(false))),
            fill_space(),
            text(count).size(14).font(SEMIBOLD).color(theme::c().text),
            fill_space(),
            tool_button(
                Icon::Check,
                if all_picked { "None" } else { "All" },
                Some(if all_picked {
                    Message::SelectNone
                } else {
                    Message::SelectAll
                }),
            ),
        ]
        .align_y(Alignment::Center),
        row![
            tool_button(Icon::Archive, "Archive", go(Action::Archive)),
            danger_tool_button(Icon::Trash, "Delete", go(Action::Delete)),
            tool_button(Icon::Shield, "Junk", go(Action::Spam)),
        ]
        .spacing(2),
        row![
            tool_button(Icon::Mail, "Read", go(Action::MarkRead)),
            tool_button(Icon::Unread, "Unread", go(Action::MarkUnread)),
            tool_button(Icon::Star, "Star", go(Action::ToggleStar)),
        ]
        .spacing(2),
    ]
    .spacing(2)
    .padding(Padding {
        top: 10.0,
        right: 10.0,
        bottom: 6.0,
        left: 10.0,
    })
    .into()
}

fn scope_for(app: &App) -> ListScope {
    match &app.nav {
        Nav::AllInboxes => ListScope::AllInboxes,
        Nav::Folder { folder_id, .. } => ListScope::Folder(*folder_id),
    }
}

fn hit_row<'a>(app: &'a App, h: &'a SearchHit) -> Element<'a, Message> {
    let m = &h.message;
    let selected = app
        .reader
        .as_ref()
        .is_some_and(|r| r.thread_key == m.thread_key);
    let who = display_sender(&m.from_name, &m.from_addr);
    let subject = if m.subject.trim().is_empty() {
        "(no subject)".to_owned()
    } else {
        m.subject.clone()
    };
    // The matching words are drawn in bold.
    let mut spans: Vec<iced::widget::text::Span<'a, (), iced::Font>> = Vec::new();
    let snip = &h.snippet;
    let mut at = 0;
    for r in &snip.highlights {
        if r.start >= at
            && r.end <= snip.text.len()
            && snip.text.is_char_boundary(r.start)
            && snip.text.is_char_boundary(r.end)
        {
            if r.start > at {
                spans.push(iced::widget::span(&snip.text[at..r.start]).color(theme::c().muted));
            }
            spans.push(
                iced::widget::span(&snip.text[r.clone()])
                    .font(SEMIBOLD)
                    .color(theme::c().text),
            );
            at = r.end;
        }
    }
    if at < snip.text.len() {
        spans.push(iced::widget::span(&snip.text[at..]).color(theme::c().muted));
    }
    let snippet = iced::widget::rich_text(spans)
        .size(13)
        .wrapping(iced::widget::text::Wrapping::None);
    let body = column![
        row![
            super::ui::clipped(one_line(clip(&who, 40)).size(14).font(SEMIBOLD)),
            text(short_when(m.date, app.now, app.tz))
                .size(12)
                .color(theme::c().muted)
        ],
        container(one_line(clip(&subject, 44)).size(13))
            .width(Length::Fill)
            .clip(true),
        container(snippet).width(Length::Fill).clip(true),
    ]
    .spacing(2)
    .width(Length::Fill);
    button(
        row![
            avatar(initial(&m.from_name, &m.from_addr), &m.from_addr, 36.0),
            body
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding(Padding::from([10, 10]))
    .style(theme::row(selected))
    .on_press(Message::OpenThread(
        m.thread_key.clone(),
        ListScope::Folder(m.folder_id),
    ))
    .into()
}

fn attention<'a>(app: &'a App) -> Option<Element<'a, Message>> {
    let ids: Vec<&crate::app::AccountState> = match &app.nav {
        Nav::AllInboxes => app.accounts.iter().collect(),
        Nav::Folder { account, .. } => app.account_state(account).into_iter().collect(),
    };
    for a in ids {
        let who = a.account.email.to_string();
        match &a.status {
            SyncStatus::NeedsAttention { error, advice } => {
                let id = a.account.id.as_str().to_owned();
                return Some(banner(
                    Tone::Danger,
                    Icon::Warning,
                    column![
                        text(format!("Can't connect to {who}"))
                            .size(14)
                            .font(SEMIBOLD),
                        text(advice.to_string()).size(13).color(theme::c().muted),
                        text(clip(error, 160)).size(12).color(theme::c().faint),
                        row![
                            match a.account.auth {
                                mailcore::account::AuthMethod::Password => {
                                    super::ui::primary_button(
                                        "Change password",
                                        Some(Message::AskChangePassword(id.clone())),
                                    )
                                }
                                mailcore::account::AuthMethod::OAuth2 { .. } => {
                                    super::ui::primary_button(
                                        "Sign in again",
                                        Some(Message::ResignIn(id.clone())),
                                    )
                                }
                            },
                            secondary_button("Try again", Some(Message::RetryAccount(id))),
                        ]
                        .spacing(8)
                    ]
                    .spacing(6)
                    .into(),
                ));
            }
            SyncStatus::Offline { advice, .. } => {
                let (line, _) = status_line(&a.status);
                return Some(banner(
                    Tone::Warning,
                    Icon::Warning,
                    column![
                        text(format!("{who}: {line}")).size(14).font(SEMIBOLD),
                        text(clip(advice, 200)).size(13).color(theme::c().muted)
                    ]
                    .spacing(4)
                    .into(),
                ));
            }
            _ => {}
        }
    }
    None
}

fn empty_state<'a>(app: &'a App) -> Element<'a, Message> {
    let syncing = app.accounts.iter().any(|a| {
        !matches!(
            a.status,
            SyncStatus::Online { .. }
                | SyncStatus::NeedsAttention { .. }
                | SyncStatus::Offline { .. }
        )
    });
    let progress: Option<String> = app.accounts.iter().find_map(|a| match &a.status {
        SyncStatus::Syncing(p) => super::sidebar::progress_text(p),
        _ => None,
    });
    let (headline, hint) = if syncing || app.list_loading {
        (
            "Getting your mail…",
            progress.unwrap_or_else(|| "The first download can take a minute.".to_owned()),
        )
    } else if app.unread_only {
        (
            "No unread mail",
            "You are all caught up. Press Unread again to see everything.".to_owned(),
        )
    } else {
        (
            "Nothing here",
            "Messages you receive will show up in this list.".to_owned(),
        )
    };
    container(
        column![
            icon(Icon::Inbox, 44.0, theme::c().faint),
            text(headline).size(16).font(SEMIBOLD),
            muted(hint),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .center_x(Length::Fill)
    .padding(Padding {
        top: 70.0,
        ..Padding::ZERO
    })
    .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let head: Element<'_, Message> = if app.selecting {
        selection_head(app)
    } else {
        let mut h = row![title(heading(app)), fill_space()];
        if app.hits.is_none() {
            h = h.push(
                iced::widget::button(text("Unread").size(12).font(SEMIBOLD))
                    .padding(Padding::from([5, 10]))
                    .style(if app.unread_only {
                        theme::primary
                    } else {
                        theme::ghost
                    })
                    .on_press(Message::SetUnreadOnly(!app.unread_only)),
            );
        }
        if app.hits.is_none() && !app.threads.is_empty() {
            h = h.push(icon_only(
                Icon::Select,
                theme::c().muted,
                Some(Message::SelectMode(true)),
            ));
        }
        h.push(icon_only(
            Icon::Refresh,
            theme::c().muted,
            Some(Message::RefreshNow),
        ))
        .align_y(Alignment::Center)
        .padding(Padding {
            top: 16.0,
            right: 12.0,
            bottom: 8.0,
            left: 18.0,
        })
        .into()
    };

    let search = row![
        container(icon(Icon::Search, 16.0, theme::c().muted)).padding(Padding {
            left: 4.0,
            ..Padding::ZERO
        }),
        text_input("Search mail", &app.search)
            .on_input(Message::SearchChanged)
            .padding(9)
            .size(14)
            .style(theme::search_input)
            .width(Length::Fill),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let mut search_box = column![search].spacing(8);
    if !app.search.trim().is_empty() {
        search_box = search_box.push(
            row![
                text("Search every account")
                    .size(13)
                    .color(theme::c().muted),
                toggler(app.search_everywhere)
                    .size(18)
                    .on_toggle(Message::SearchEverywhere),
                fill_space(),
                button(text("Clear").size(13))
                    .style(theme::ghost)
                    .padding(Padding::from([4, 8]))
                    .on_press(Message::ClearSearch),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }

    let mut items = column![].spacing(ROW_GAP).padding(Padding::from([4, 8]));
    if let Some(hits) = &app.hits {
        if hits.is_empty() {
            items = items.push(
                container(
                    column![
                        icon(Icon::Search, 36.0, theme::c().faint),
                        text("No matches").size(16).font(SEMIBOLD),
                        muted("Try fewer or different words.")
                    ]
                    .spacing(8)
                    .align_x(Alignment::Center),
                )
                .center_x(Length::Fill)
                .padding(Padding {
                    top: 60.0,
                    ..Padding::ZERO
                }),
            );
        }
        for h in hits {
            items = items.push(hit_row(app, h));
        }
    } else if app.threads.is_empty() {
        items = items.push(empty_state(app));
    } else {
        // Only the conversations near the visible part are built; blank blocks of the same
        // height stand in for the rest, so a long list costs no more than a short one.
        let row = row_height(app);
        let (first, end) = visible_rows(app.threads.len(), row, app.list_scroll, app.list_height);
        if first > 0 {
            items = items.push(gap(first, row));
        }
        for t in &app.threads[first..end] {
            items = items.push(thread_row(app, t));
        }
        if end < app.threads.len() {
            items = items.push(gap(app.threads.len() - end, row));
        }
        if app.threads.len() >= app.limit {
            items = items.push(
                container(secondary_button("Show more", Some(Message::ShowMore)))
                    .center_x(Length::Fill)
                    .padding(10),
            );
        }
        if let Some(f) = app.current_folder()
            && f.floor_uid > 1
        {
            let label = if app.busy_older {
                "Loading older mail…"
            } else {
                "Load older mail from the server"
            };
            items = items.push(
                container(secondary_button(
                    label,
                    if app.busy_older {
                        None
                    } else {
                        Some(Message::LoadOlder(f.id))
                    },
                ))
                .center_x(Length::Fill)
                .padding(10),
            );
        }
    }

    let mut top = column![
        head,
        container(search_box).padding(Padding {
            left: 14.0,
            right: 14.0,
            bottom: 10.0,
            top: 0.0
        })
    ];
    if let Some(a) = attention(app) {
        top = top.push(container(a).padding(Padding {
            left: 12.0,
            right: 12.0,
            bottom: 8.0,
            top: 0.0,
        }));
    }
    container(column![
        top,
        hline(),
        crate::theme::scrolling(items)
            .id(iced::widget::Id::new(crate::app::LIST_SCROLL))
            .on_scroll(|v| Message::ListScrolled(v.absolute_offset().y, v.bounds().height))
            .height(Length::Fill)
    ])
    .width(if app.narrow() {
        Length::Fill
    } else {
        Length::Fixed(app.prefs.look.list_width.px())
    })
    .height(Length::Fill)
    .style(theme::page)
    .into()
}

#[cfg(test)]
mod window_tests {
    use super::{ROW_GAP, visible_rows};

    const ROW: f32 = 78.0;

    #[test]
    fn nothing_to_show_builds_nothing() {
        assert_eq!(visible_rows(0, ROW, 0.0, 800.0), (0, 0));
        assert_eq!(visible_rows(0, ROW, 9_999.0, 800.0), (0, 0));
    }

    #[test]
    fn a_short_list_is_built_whole() {
        assert_eq!(visible_rows(20, ROW, 0.0, 800.0), (0, 20));
        assert_eq!(visible_rows(1, ROW, 0.0, 0.0), (0, 1));
    }

    #[test]
    fn a_long_list_builds_a_bounded_window() {
        for scroll in [0.0, 500.0, 40_000.0, 700_000.0] {
            let (first, end) = visible_rows(10_000, ROW, scroll, 800.0);
            assert!(end - first < 80, "window of {} rows", end - first);
        }
    }

    #[test]
    fn the_window_always_covers_what_can_be_seen() {
        let total = 10_000;
        let stride = ROW + ROW_GAP;
        for step in 0..2000 {
            let scroll = step as f32 * 37.3;
            for viewport in [300.0, 800.0, 1500.0, 4000.0] {
                let (first, end) = visible_rows(total, ROW, scroll, viewport);
                let want_first = (scroll / stride) as usize;
                let want_end = (((scroll + viewport) / stride).ceil() as usize).min(total);
                assert!(first <= want_first, "scroll {scroll}: starts at {first}");
                assert!(end >= want_end, "scroll {scroll}: ends at {end}");
                assert!(first <= end && end <= total);
            }
        }
    }

    #[test]
    fn scrolling_past_the_end_or_below_zero_does_not_panic() {
        for scroll in [f32::MAX, 1.0e12, -50.0, f32::NAN, f32::INFINITY] {
            let (first, end) = visible_rows(100, ROW, scroll, 800.0);
            assert!(
                first <= end && end <= 100,
                "scroll {scroll}: {first}..{end}"
            );
        }
    }
}
