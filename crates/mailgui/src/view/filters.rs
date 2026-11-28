//! Blocked senders and filter rules.

use iced::widget::{column, container, pick_list, row, text, text_input};
use iced::{Alignment, Element, Length, Padding};
use mailcore::rules::{Rule, RuleAction, RuleField};

use super::ui::{
    SEMIBOLD, banner, fill_space, hline, muted, one_line, primary_button, secondary_button, title,
    tool_button,
};
use crate::app::{App, Message};
use crate::icons::Icon;
use crate::theme::{self, Tone};
use crate::userdata_flow::DataMsg;
use crate::util::clip;

fn d(m: DataMsg) -> Message {
    Message::Data(m)
}

#[derive(Clone, PartialEq, Eq)]
struct AccountChoice {
    id: Option<String>,
    label: String,
}

impl std::fmt::Display for AccountChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn section<'a>(heading: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
    container(column![text(heading).size(16).font(SEMIBOLD), body].spacing(10))
        .padding(18)
        .width(Length::Fill)
        .style(theme::card)
        .into()
}

fn choice<'a>(label: &'a str, selected: bool, on_press: Message) -> Element<'a, Message> {
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

fn scope_label(app: &App, r: &Rule) -> String {
    match &r.account {
        None => "all accounts".to_owned(),
        Some(id) => app
            .accounts
            .iter()
            .find(|a| a.account.id.as_str() == id)
            .map_or_else(
                || "an account that was removed".to_owned(),
                |a| a.account.email.to_string(),
            ),
    }
}

fn rule_row<'a>(app: &'a App, r: &'a Rule, block: bool) -> Element<'a, Message> {
    let what = if block {
        clip(&r.needle, 60)
    } else {
        r.describe()
    };
    row![
        column![
            one_line(what)
                .size(14)
                .font(SEMIBOLD)
                .color(theme::c().text),
            one_line(format!("applies to {}", scope_label(app, r)))
                .size(12)
                .color(theme::c().muted),
        ]
        .spacing(2)
        .width(Length::Fill),
        tool_button(
            Icon::Close,
            if block { "Unblock" } else { "Remove" },
            Some(d(DataMsg::RemoveRule(r.id.clone()))),
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn list<'a>(items: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let mut col = column![].spacing(6);
    let n = items.len();
    for (i, it) in items.into_iter().enumerate() {
        col = col.push(it);
        if i + 1 < n {
            col = col.push(hline());
        }
    }
    col.into()
}

fn add_form(app: &App) -> Element<'_, Message> {
    let f = &app.rule_form;
    let fields =
        row(RuleField::ALL.map(|x| choice(x.label(), f.field == x, d(DataMsg::FormField(x)))))
            .spacing(8);
    let actions =
        row(RuleAction::ALL.map(|x| choice(x.label(), f.action == x, d(DataMsg::FormAction(x)))))
            .spacing(8)
            .wrap();
    let mut choices = vec![AccountChoice {
        id: None,
        label: "All accounts".into(),
    }];
    choices.extend(app.accounts.iter().map(|a| AccountChoice {
        id: Some(a.account.id.as_str().to_owned()),
        label: a.account.email.to_string(),
    }));
    let selected = choices.iter().find(|c| c.id == f.account).cloned();
    let hint = match f.field {
        RuleField::From => {
            "An address (name@example.com) matches exactly that sender. A domain (@example.com) matches everyone at that domain. Other text is looked for in the sender's name and address."
        }
        RuleField::Subject => "Messages whose subject contains this text.",
        RuleField::To => "Messages addressed to a person or list containing this text.",
    };
    let mut col = column![
        column![muted("Look at"), fields].spacing(6),
        column![
            muted("Text to look for (capital letters do not matter)"),
            text_input("at least 3 characters", &f.text)
                .on_input(|t| d(DataMsg::FormText(t)))
                .on_submit(d(DataMsg::AddRule))
                .padding(11)
                .size(15)
                .style(theme::input),
            muted(hint),
        ]
        .spacing(6),
        column![muted("Then"), actions].spacing(6),
        column![
            muted("For"),
            pick_list(choices, selected, |c| d(DataMsg::FormAccount(c.id)))
                .padding(9)
                .style(theme::pick),
        ]
        .spacing(6),
    ]
    .spacing(14);
    if let Some(e) = &f.error {
        col = col.push(banner(
            Tone::Danger,
            Icon::Warning,
            text(e.clone()).size(13).into(),
        ));
    }
    col.push(row![primary_button("Add rule", Some(d(DataMsg::AddRule)))])
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let blocked: Vec<Element<'_, Message>> = app
        .rules
        .iter()
        .filter(|r| r.is_block())
        .map(|r| rule_row(app, r, true))
        .collect();
    let others: Vec<Element<'_, Message>> = app
        .rules
        .iter()
        .filter(|r| !r.is_block())
        .map(|r| rule_row(app, r, false))
        .collect();

    let mut col = column![
        row![title("Filters and blocked senders"), fill_space()].align_y(Alignment::Center),
        muted("Rules look only at who a message is from, its subject, or who it is addressed to, never at what it says. They are applied to new mail that arrives in the Inbox. EMAYL does not guess what is spam by itself: that is still done by your mail provider's own spam filter, and anything it puts in Junk stays there."),
    ]
    .spacing(10);
    if let Some(p) = &app.rules_problem {
        col = col.push(banner(
            Tone::Danger,
            Icon::Warning,
            text(p.clone()).size(13).into(),
        ));
    }
    col = col.push(section(
        "Blocked senders",
        if blocked.is_empty() {
            muted("Nobody is blocked. Open a message and press Block to send everything from that sender to Junk, or add one below.")
                .into()
        } else {
            list(blocked)
        },
    ));
    col = col.push(section(
        "Rules",
        if others.is_empty() {
            muted("No other rules yet.").into()
        } else {
            list(others)
        },
    ));
    col = col.push(section("Add a rule or block a sender", add_form(app)));
    let mut apply = column![
        muted("Rules normally act on mail that arrives from now on. To use them on the mail that is already in your inboxes, press the button. Nothing is deleted: mail goes to Junk or the archive, and you can move it back."),
        row![secondary_button(
            "Apply to the mail already in my inboxes",
            Some(d(DataMsg::ApplyNow))
        )],
    ]
    .spacing(10);
    if let Some(n) = &app.rules_note {
        apply = apply.push(muted(n.clone()));
    }
    col = col.push(section("Apply now", apply.into()));
    container(crate::theme::scrolling(
        container(col.spacing(14).max_width(760)).padding(Padding::from([28, 32])),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::page)
    .into()
}
