use iced::widget::{button, column, container, pick_list, row, text, text_editor, text_input};
use iced::{Alignment, Element, Length, Padding};

use super::ui::{
    SEMIBOLD, banner, danger_tool_button, fill_space, hline, icon_only, muted, primary_button,
    tool_button,
};
use crate::app::{App, Message};
use crate::compose_flow::{ComposeMsg, ComposeState};
use crate::icons::{Icon, icon};
use crate::theme::{self, Tone};
use crate::util::{clip, size_text};

/// One entry of the "From" list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FromChoice {
    id: String,
    label: String,
}

impl std::fmt::Display for FromChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn c(m: ComposeMsg) -> Message {
    Message::Compose(m)
}

fn heading(s: &ComposeState) -> &'static str {
    if s.subject.starts_with("Fwd:") {
        "Forward"
    } else if s.in_reply_to.is_some() {
        "Reply"
    } else {
        "New message"
    }
}

fn line<'a>(
    label: &'a str,
    input: Element<'a, Message>,
    extra: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut r = row![
        container(muted(label)).width(64),
        container(input).width(Length::Fill)
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([2, 16]));
    if let Some(e) = extra {
        r = r.push(e);
    }
    r.into()
}

fn address_input<'a>(
    placeholder: &'a str,
    value: &'a str,
    on_input: fn(String) -> ComposeMsg,
) -> text_input::TextInput<'a, Message> {
    text_input(placeholder, value)
        .on_input(move |t| c(on_input(t)))
        .padding(8)
        .size(15)
        .style(theme::bare_input)
}

fn attachment_row<'a>(s: &'a ComposeState) -> Option<Element<'a, Message>> {
    if s.attachments.is_empty() {
        return None;
    }
    let mut chips: Vec<Element<'a, Message>> = Vec::new();
    for (i, a) in s.attachments.iter().enumerate() {
        chips.push(
            container(
                row![
                    icon(Icon::Paperclip, 14.0, theme::c().muted),
                    text(clip(&a.filename, 32)).size(13),
                    muted(size_text(a.data.len())),
                    icon_only(
                        Icon::Close,
                        theme::c().muted,
                        Some(c(ComposeMsg::RemoveAttachment(i)))
                    ),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding(Padding {
                top: 2.0,
                right: 4.0,
                bottom: 2.0,
                left: 10.0,
            })
            .style(theme::chip)
            .into(),
        );
    }
    let chips = row(chips).spacing(8).align_y(Alignment::Center).wrap();
    Some(container(chips).padding(Padding::from([8, 16])).into())
}

#[derive(Clone, PartialEq, Eq)]
struct TemplateChoice {
    id: String,
    name: String,
}

impl std::fmt::Display for TemplateChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// "Insert template" list; nothing is shown until at least one template exists.
fn templates_menu(app: &App) -> Element<'_, Message> {
    if app.templates.is_empty() {
        return iced::widget::space().width(0).into();
    }
    let choices: Vec<TemplateChoice> = app
        .templates
        .iter()
        .map(|t| TemplateChoice {
            id: t.id.clone(),
            name: crate::util::clip(&t.name, 30),
        })
        .collect();
    pick_list(choices, None::<TemplateChoice>, |t| {
        c(ComposeMsg::InsertTemplate(t.id))
    })
    .placeholder("Insert template")
    .style(theme::pick)
    .padding(7)
    .text_size(13)
    .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let Some(s) = &app.compose else {
        return fill_space();
    };

    let from: Element<'_, Message> = if app.accounts.len() > 1 {
        let choices: Vec<FromChoice> = app
            .accounts
            .iter()
            .map(|a| FromChoice {
                id: a.account.id.as_str().to_owned(),
                label: format!("{} <{}>", a.account.display_name, a.account.email),
            })
            .collect();
        let selected = choices.iter().find(|x| x.id == s.account).cloned();
        pick_list(choices, selected, |x| c(ComposeMsg::Account(x.id)))
            .style(theme::pick)
            .padding(8)
            .width(Length::Fill)
            .into()
    } else {
        let label = app
            .accounts
            .first()
            .map(|a| format!("{} <{}>", a.account.display_name, a.account.email))
            .unwrap_or_default();
        container(text(label).size(15))
            .padding(Padding::from([8, 8]))
            .into()
    };

    let cc_toggle = (!s.show_cc).then(|| {
        button(text("Cc / Bcc").size(13))
            .style(theme::ghost)
            .padding(Padding::from([6, 10]))
            .on_press(c(ComposeMsg::ToggleCc))
            .into()
    });

    let mut fields = column![
        line("From", from, None),
        hline(),
        line(
            "To",
            address_input(
                "Name or email address, separated by commas",
                &s.to,
                ComposeMsg::To
            )
            .id("compose-to")
            .into(),
            cc_toggle
        ),
        hline(),
    ];
    if s.show_cc {
        fields = fields
            .push(line(
                "Cc",
                address_input("", &s.cc, ComposeMsg::Cc).into(),
                None,
            ))
            .push(hline())
            .push(line(
                "Bcc",
                address_input(
                    "People here are hidden from the others",
                    &s.bcc,
                    ComposeMsg::Bcc,
                )
                .into(),
                None,
            ))
            .push(hline());
    }
    fields = fields.push(line(
        "Subject",
        text_input("What is this about?", &s.subject)
            .on_input(|t| c(ComposeMsg::Subject(t)))
            .padding(8)
            .size(15)
            .style(theme::bare_input)
            .into(),
        None,
    ));
    fields = fields.push(hline());

    let editor = text_editor(&s.body)
        .id("compose-body")
        .placeholder("Write your message…")
        .on_action(|a| c(ComposeMsg::Body(a)))
        .padding(Padding::from([14, 18]))
        .size(15)
        .height(Length::Fill)
        .style(theme::editor);

    let mut card = column![fields, editor];
    if let Some(a) = attachment_row(s) {
        card = card.push(hline()).push(a);
    }

    let mut notes = column![].spacing(8);
    if s.loading_attachments {
        notes = notes.push(muted("Adding the attachments of the original message…"));
    }
    if let Some(n) = &s.notice {
        notes = notes.push(banner(
            Tone::Info,
            Icon::Shield,
            text(n.clone()).size(13).into(),
        ));
    }
    if let Some(e) = &s.error {
        notes = notes.push(banner(
            Tone::Danger,
            Icon::Warning,
            text(e.clone()).size(13).into(),
        ));
    }

    let busy = s.sending || s.loading_attachments;
    let files = if s.attachments.is_empty() {
        String::new()
    } else {
        format!(
            "{} file{} · {}",
            s.attachments.len(),
            if s.attachments.len() == 1 { "" } else { "s" },
            size_text(s.attachment_bytes())
        )
    };
    let bar = row![
        primary_button(
            if s.sending { "Sending…" } else { "Send" },
            (!busy).then(|| c(ComposeMsg::Send))
        ),
        tool_button(
            Icon::Paperclip,
            "Attach files",
            (!s.sending).then(|| c(ComposeMsg::Attach))
        ),
        templates_menu(app),
        muted(files),
        fill_space(),
        danger_tool_button(
            Icon::Trash,
            "Discard",
            (!s.sending).then(|| c(ComposeMsg::AskDiscard))
        ),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let top = row![
        text(heading(s))
            .size(22)
            .font(SEMIBOLD)
            .color(theme::c().text),
        fill_space(),
        tool_button(
            Icon::Close,
            "Save and close",
            (!s.sending).then(|| c(ComposeMsg::Close))
        ),
    ]
    .align_y(Alignment::Center);

    let page = column![
        top,
        container(card).height(Length::Fill).style(theme::card),
        notes,
        bar
    ]
    .spacing(14)
    .padding(Padding::from([22, 28]))
    .max_width(980)
    .height(Length::Fill);

    container(container(page).center_x(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::page)
        .into()
}
