//! Saved reply texts.

use iced::widget::{column, container, row, text, text_editor, text_input};
use iced::{Alignment, Element, Length, Padding};

use super::ui::{
    SEMIBOLD, banner, fill_space, hline, muted, one_line, primary_button, title, tool_button,
};
use crate::app::{App, Message};
use crate::icons::Icon;
use crate::theme::{self, Tone};
use crate::userdata_flow::DataMsg;
use crate::util::clip;

fn d(m: DataMsg) -> Message {
    Message::Data(m)
}

fn section<'a>(heading: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
    container(column![text(heading).size(16).font(SEMIBOLD), body].spacing(10))
        .padding(18)
        .width(Length::Fill)
        .style(theme::card)
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut saved = column![].spacing(6);
    if app.templates.is_empty() {
        saved = saved.push(muted(
            "No templates yet. Save the replies you type again and again, then insert them from the Write screen.",
        ));
    }
    let n = app.templates.len();
    for (i, t) in app.templates.iter().enumerate() {
        let first = t.body.lines().next().unwrap_or("");
        saved = saved.push(
            row![
                column![
                    one_line(clip(&t.name, 60))
                        .size(14)
                        .font(SEMIBOLD)
                        .color(theme::c().text),
                    one_line(clip(first, 80)).size(12).color(theme::c().muted),
                ]
                .spacing(2)
                .width(Length::Fill),
                tool_button(
                    Icon::Trash,
                    "Remove",
                    Some(d(DataMsg::RemoveTemplate(t.id.clone())))
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        if i + 1 < n {
            saved = saved.push(hline());
        }
    }
    let f = &app.template_form;
    let mut form = column![
        column![
            muted("Name"),
            text_input("For example: Thanks, I will get back to you", &f.name)
                .on_input(|t| d(DataMsg::TemplateName(t)))
                .padding(11)
                .size(15)
                .style(theme::input),
        ]
        .spacing(6),
        column![
            muted("Text"),
            container(
                text_editor(&f.body)
                    .placeholder("The text that will be inserted")
                    .on_action(|a| d(DataMsg::TemplateBody(a)))
                    .padding(Padding::from([10, 12]))
                    .size(15)
                    .height(160)
                    .style(theme::editor)
            )
            .style(theme::card),
        ]
        .spacing(6),
    ]
    .spacing(12);
    if let Some(e) = &f.error {
        form = form.push(banner(
            Tone::Danger,
            Icon::Warning,
            text(e.clone()).size(13).into(),
        ));
    }
    form = form.push(row![primary_button(
        "Save template",
        Some(d(DataMsg::AddTemplate))
    )]);
    let col = column![
        row![title("Templates"), fill_space()].align_y(Alignment::Center),
        muted("Templates are plain text saved on this computer. Inserting one pastes the text into your message. Saving a template with an existing name replaces it."),
        section("Saved templates", saved.into()),
        section("New template", form.into()),
    ]
    .spacing(14)
    .max_width(760);
    container(crate::theme::scrolling(
        container(col).padding(Padding::from([28, 32])),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::page)
    .into()
}
