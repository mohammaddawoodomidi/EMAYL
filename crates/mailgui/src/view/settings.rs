use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Length, Padding};

use super::ui::{SEMIBOLD, fill_space, muted, primary_button, title, tool_button};
use crate::about::{self, Facts};
use crate::app::{App, Message};
use crate::icons::Icon;
use crate::theme;

fn section(heading: &'static str, body: Element<'static, Message>) -> Element<'static, Message> {
    container(column![text(heading).size(16).font(SEMIBOLD), body].spacing(10))
        .padding(18)
        .width(Length::Fill)
        .style(theme::card)
        .into()
}

fn line(label: &'static str, value: String) -> Element<'static, Message> {
    row![
        container(text(label).size(13).color(theme::c().muted)).width(150),
        text(value).size(13),
    ]
    .spacing(8)
    .into()
}

fn path_line(label: &'static str, value: String) -> Element<'static, Message> {
    row![
        container(text(label).size(13).color(theme::c().muted)).width(150),
        text(value.clone()).size(13).width(Length::Fill),
        tool_button(Icon::Draft, "Copy", Some(Message::CopyText(value))),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn availability(on: bool) -> String {
    if on {
        "Available".to_owned()
    } else {
        "Not set up in this copy of the program".to_owned()
    }
}

fn facts_sections(f: &Facts) -> Vec<Element<'static, Message>> {
    vec![
        section(
            "About",
            column![
                line("Version", f.version.to_owned()),
                line("System", f.system.clone()),
            ]
            .spacing(6)
            .into(),
        ),
        section(
            "Where your files are",
            column![
                path_line("Mail on this computer", f.data_folder.clone()),
                path_line("Account list", f.settings_folder.clone()),
            ]
            .spacing(6)
            .into(),
        ),
        section(
            "Tip",
            muted("Turn on your computer's own disk protection (BitLocker on Windows) so your mail stays private if the computer is lost.").into(),
        ),
        section(
            "Signing in through a website",
            column![
                line("Google", availability(f.google_sign_in)),
                line("Microsoft", availability(f.microsoft_sign_in)),
            ]
            .spacing(6)
            .into(),
        ),
    ]
}

fn choice(label: String, selected: bool, on_press: Message) -> Element<'static, Message> {
    iced::widget::button(text(label).size(13).font(SEMIBOLD))
        .padding(Padding::from([7, 14]))
        .style(if selected {
            theme::primary
        } else {
            theme::secondary
        })
        .on_press(on_press)
        .into()
}

fn swatch(accent: theme::Accent, dark: bool, selected: bool) -> Element<'static, Message> {
    let color = theme::Colors::build(dark, accent).accent;
    iced::widget::button(
        container(iced::widget::space())
            .width(22)
            .height(22)
            .style(theme::dot(color)),
    )
    .padding(4)
    .style(move |_, status| {
        let ring = if selected {
            theme::c().text
        } else if matches!(status, iced::widget::button::Status::Hovered) {
            theme::c().faint
        } else {
            iced::Color::TRANSPARENT
        };
        iced::widget::button::Style {
            background: None,
            border: iced::Border {
                color: ring,
                width: 2.0,
                radius: 999.0.into(),
            },
            ..Default::default()
        }
    })
    .on_press(Message::SetAccent(accent))
    .into()
}

fn labelled(label: &'static str, control: Element<'static, Message>) -> Element<'static, Message> {
    column![text(label).size(13).color(theme::c().muted), control]
        .spacing(6)
        .into()
}

fn appearance(look: crate::prefs::Look, dark: bool) -> Element<'static, Message> {
    let themes = row(theme::ThemeChoice::ALL
        .map(|t| choice(t.label().to_owned(), look.theme == t, Message::SetTheme(t))))
    .spacing(8);
    let accents = row(theme::Accent::ALL.map(|a| swatch(a, dark, look.accent == a))).spacing(6);
    let sizes = row(crate::prefs::SCALES
        .map(|s| choice(format!("{s}%"), look.scale == s, Message::SetScale(s))))
    .spacing(8);
    let density = row![
        choice(
            "Roomy".to_owned(),
            !look.compact,
            Message::SetCompact(false)
        ),
        choice(
            "Compact".to_owned(),
            look.compact,
            Message::SetCompact(true)
        ),
    ]
    .spacing(8);
    let previews = row![
        choice("Show".to_owned(), look.previews, Message::SetPreviews(true)),
        choice(
            "Hide".to_owned(),
            !look.previews,
            Message::SetPreviews(false)
        ),
    ]
    .spacing(8);
    let pane = row![
        choice(
            "Beside the list".to_owned(),
            look.side_by_side,
            Message::SetSideBySide(true)
        ),
        choice(
            "On its own page".to_owned(),
            !look.side_by_side,
            Message::SetSideBySide(false)
        ),
    ]
    .spacing(8)
    .wrap();
    let widths = row(crate::prefs::ListWidth::ALL.map(|w| {
        choice(
            w.label().to_owned(),
            look.list_width == w,
            Message::SetListWidth(w),
        )
    }))
    .spacing(8);
    section(
        "Appearance",
        column![
            labelled("Theme", themes.into()),
            labelled("Accent colour", accents.into()),
            labelled("Interface size", sizes.into()),
            labelled("Message list rows", density.into()),
            labelled("First words of each message in the list", previews.into()),
            labelled("Where a message opens", pane.into()),
            labelled("Width of the message list", widths.into()),
            muted("Small windows always show the list and the message one at a time. A wider list needs a wider window to sit beside a message."),
            muted("These choices are remembered on this computer."),
        ]
        .spacing(14)
        .into(),
    )
}

pub fn view(app: &App) -> Element<'_, Message> {
    let facts = app.facts(app.sqlite.as_deref());
    let report = about::report(&facts);
    let mut col = column![
        row![
            title("Settings"),
            fill_space(),
            primary_button("Copy report for support", Some(Message::CopyText(report)))
        ]
        .align_y(Alignment::Center),
        muted("The report lists the version, folders, server names and how each account is doing. It never contains passwords, message text or email addresses."),
    ]
    .spacing(10);
    col = col.push(appearance(app.prefs.look, app.colors().dark));
    col = col.push(section(
        "Sending",
        column![
            labelled(
                "Time to take a message back after pressing Send",
                row(crate::prefs::SEND_DELAYS.map(|s| choice(
                    if s == 0 { "Off".to_owned() } else { format!("{s} s") },
                    app.prefs.send_delay == s,
                    Message::SetSendDelay(s)
                )))
                .spacing(8)
                .wrap()
                .into(),
            ),
            muted("During this time an Undo button is shown and the message is not sent yet. If you close the program in that time, it is sent when you open it again."),
        ]
        .spacing(10)
        .into(),
    ));
    col = col.push(section(
        "Filters and templates",
        column![
            muted("Block senders, send mail from chosen senders to Junk or the archive, and keep saved replies."),
            row![
                super::ui::secondary_button(
                    "Filters and blocked senders",
                    Some(Message::Data(crate::userdata_flow::DataMsg::ShowFilters))
                ),
                super::ui::secondary_button(
                    "Templates",
                    Some(Message::Data(crate::userdata_flow::DataMsg::ShowTemplates))
                ),
            ]
            .spacing(8)
            .wrap(),
        ]
        .spacing(10)
        .into(),
    ));
    for s in facts_sections(&facts) {
        col = col.push(s);
    }
    container(crate::theme::scrolling(
        container(col.spacing(14).max_width(760)).padding(Padding::from([28, 32])),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::page)
    .into()
}
