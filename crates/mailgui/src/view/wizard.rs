use iced::widget::{button, column, container, pick_list, row, text, text_input};
use iced::{Alignment, Element, Font, Length, Padding};

use super::ui::{
    SEMIBOLD, banner, fill_space, labelled, muted, primary_button, secondary_button, tool_button,
};
use crate::app::{App, Message};
use crate::icons::{Icon, icon};
use crate::theme::{self, Tone};
use crate::wizard::{
    Outcome, SecurityChoice, Step, failure_headline, provider_label, sign_in_help,
};

fn field<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    on_input: fn(String) -> Message,
) -> Element<'a, Message> {
    labelled(
        label,
        text_input(placeholder, value)
            .on_input(on_input)
            .padding(11)
            .size(15)
            .style(theme::input)
            .into(),
    )
}

fn outcome<'a>(app: &'a App) -> Option<Element<'a, Message>> {
    match &app.wizard.outcome {
        Outcome::Idle => None,
        Outcome::Busy(what) => Some(muted(*what).into()),
        Outcome::Problem(msg) => Some(banner(
            Tone::Warning,
            Icon::Warning,
            text(msg.clone()).size(14).into(),
        )),
        Outcome::CheckFailed(d) => {
            let report = d.render();
            let mut help = column![].spacing(8);
            if let Some(h) = sign_in_help(d) {
                help = help
                    .push(banner(
                        Tone::Info,
                        Icon::Shield,
                        text(h.steps).size(13).into(),
                    ))
                    .push(
                        row(h.buttons.iter().map(|(label, url)| {
                            primary_button(label, Some(Message::OpenLink((*url).to_owned())))
                        }))
                        .spacing(8),
                    );
            }
            Some(
                column![
                    banner(
                        Tone::Danger,
                        Icon::Warning,
                        text(failure_headline(d)).size(14).into()
                    ),
                    help,
                    container(
                        crate::theme::scrolling(
                            text(report.clone())
                                .font(Font::MONOSPACE)
                                .size(12)
                                .color(theme::c().muted)
                        )
                        .height(150)
                    )
                    .padding(10)
                    .style(theme::code_block),
                    row![
                        fill_space(),
                        tool_button(Icon::Draft, "Copy report", Some(Message::CopyText(report)))
                    ],
                ]
                .spacing(8)
                .into(),
            )
        }
    }
}

fn password_field<'a>(app: &'a App) -> Element<'a, Message> {
    let w = &app.wizard;
    let input = text_input("Password", &w.password)
        .on_input(Message::WizPassword)
        .on_submit(Message::WizSubmit)
        .secure(!w.show_password)
        .padding(11)
        .size(15)
        .style(theme::input);
    labelled(
        "Password",
        row![
            input,
            button(text(if w.show_password { "Hide" } else { "Show" }).size(13))
                .style(theme::ghost)
                .padding(Padding::from([9, 10]))
                .on_press(Message::WizToggleShow)
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
    )
}

/// The website sign-in this provider offers, if this copy of the program can do it.
fn web_provider(app: &App) -> Option<mailcore::account::OAuthProvider> {
    let svc = app.svc.as_ref()?;
    app.wizard
        .web_provider()
        .filter(|p| svc.oauth_available(*p))
}

fn website_sign_in<'a>(
    app: &'a App,
    provider: mailcore::account::OAuthProvider,
) -> Element<'a, Message> {
    let name = provider_label(provider);
    let waiting = app.signing_in.as_ref().is_some_and(|s| !s.existing);
    let action: Element<'a, Message> = if waiting {
        row![
            muted("Waiting for you to finish signing in, in your web browser…"),
            secondary_button("Cancel", Some(Message::WizCancelSignIn)),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    } else {
        button(
            row![
                icon(Icon::Shield, 16.0, theme::WHITE),
                text(format!("Sign in with {name}")).size(14).font(SEMIBOLD)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(Padding::from([11, 18]))
        .style(theme::primary)
        .on_press_maybe((!app.wizard.is_busy()).then_some(Message::WizWebSignIn))
        .into()
    };
    column![
        action,
        muted(format!(
            "Your web browser opens the {name} sign-in page. EMAYL never sees your {name} password."
        )),
    ]
    .spacing(8)
    .into()
}

fn manual_fields<'a>(app: &'a App) -> Element<'a, Message> {
    let m = &app.wizard.manual;
    let sec = |sel: SecurityChoice, f: fn(SecurityChoice) -> Message| {
        pick_list(SecurityChoice::ALL, Some(sel), f)
            .padding(10)
            .width(Length::Fill)
    };
    column![
        text("Incoming mail server").size(14).font(SEMIBOLD),
        row![
            container(field(
                "Server",
                "imap.example.com",
                &m.imap_host,
                Message::WizManualImapHost
            ))
            .width(Length::FillPortion(3)),
            container(field(
                "Port",
                "993",
                &m.imap_port,
                Message::WizManualImapPort
            ))
            .width(Length::FillPortion(1)),
        ]
        .spacing(10),
        labelled(
            "Connection",
            sec(m.imap_security, Message::WizManualImapSecurity).into()
        ),
        text("Outgoing mail server").size(14).font(SEMIBOLD),
        row![
            container(field(
                "Server",
                "smtp.example.com",
                &m.smtp_host,
                Message::WizManualSmtpHost
            ))
            .width(Length::FillPortion(3)),
            container(field(
                "Port",
                "465",
                &m.smtp_port,
                Message::WizManualSmtpPort
            ))
            .width(Length::FillPortion(1)),
        ]
        .spacing(10),
        labelled(
            "Connection",
            sec(m.smtp_security, Message::WizManualSmtpSecurity).into()
        ),
        field(
            "User name (leave empty to use your email address)",
            "",
            &m.username,
            Message::WizManualUser
        ),
    ]
    .spacing(12)
    .into()
}

pub fn card<'a>(app: &'a App, first: bool) -> Element<'a, Message> {
    let w = &app.wizard;
    let busy = w.is_busy();
    let mut col = column![].spacing(16);
    match w.step {
        Step::Email => {
            col = col
                .push(text("Add your email account").size(24).font(SEMIBOLD))
                .push(muted(
                    "EMAYL finds the right settings for most providers by itself.",
                ))
                .push(field("Your name", "Alex Smith", &w.name, Message::WizName))
                .push(labelled(
                    "Email address",
                    text_input("alex@example.com", &w.email)
                        .on_input(Message::WizEmail)
                        .on_submit(Message::WizContinue)
                        .padding(11)
                        .size(15)
                        .style(theme::input)
                        .into(),
                ));
        }
        Step::Password => {
            let provider = w
                .found
                .as_ref()
                .and_then(|f| f.provider.clone())
                .unwrap_or_else(|| w.email.clone());
            col = col.push(
                text(format!("Sign in to {provider}"))
                    .size(24)
                    .font(SEMIBOLD),
            );
            if let Some(f) = &w.found {
                col = col.push(muted(format!(
                    "Settings found: incoming {} · outgoing {}",
                    f.settings.imap.host, f.settings.smtp.host
                )));
                for n in &f.notes {
                    col = col.push(muted(n.clone()));
                }
            }
            let web = web_provider(app);
            if let Some(p) = web {
                col = col.push(website_sign_in(app, p));
            }
            if w.needs_website_sign_in() {
                if web.is_none() {
                    col = col.push(banner(
                        Tone::Warning,
                        Icon::Warning,
                        text("This provider only allows signing in through its own website, and this copy of the program was built without that sign-in. Install the official release of EMAYL to add this account.").size(14).into(),
                    ));
                }
            } else {
                if web.is_some() {
                    col = col.push(muted("Or use an app password instead:"));
                }
                col = col.push(password_field(app));
                if let Some(h) = w.password_hint() {
                    col = col.push(banner(Tone::Info, Icon::Shield, text(h).size(13).into()));
                }
            }
        }
        Step::Manual => {
            col = col
                .push(text("Enter your server settings").size(24).font(SEMIBOLD))
                .push(manual_fields(app))
                .push(password_field(app));
        }
    }
    if let Some(o) = outcome(app) {
        col = col.push(o);
    }

    // A website-only provider has nothing to submit here: the sign-in button is the action.
    let website_only = w.step == Step::Password && w.needs_website_sign_in();
    let go: Option<Message> = if busy {
        None
    } else {
        Some(if w.step == Step::Email {
            Message::WizContinue
        } else {
            Message::WizSubmit
        })
    };
    let go_label = match w.step {
        Step::Email => "Continue",
        _ => "Add account",
    };
    let mut buttons = row![].spacing(10).align_y(Alignment::Center);
    if !website_only {
        buttons = buttons.push(primary_button(go_label, go));
    }
    if w.step != Step::Email {
        buttons = buttons.push(secondary_button(
            "Back",
            if busy { None } else { Some(Message::WizBack) },
        ));
    }
    if !first && w.step == Step::Email {
        buttons = buttons.push(secondary_button("Cancel", Some(Message::BackToMailbox)));
    }
    if w.step == Step::Password {
        buttons = buttons.push(fill_space()).push(
            button(text("Enter settings myself").size(13))
                .style(theme::ghost)
                .padding(Padding::from([8, 10]))
                .on_press(Message::WizManual),
        );
    }
    col = col.push(buttons);

    container(col)
        .padding(32)
        .width(500)
        .style(theme::card)
        .into()
}

pub fn view<'a>(app: &'a App, first: bool) -> Element<'a, Message> {
    let placed = if first {
        container(card(app, first))
            .center(Length::Fill)
            .padding(Padding::from([40, 20]))
    } else {
        container(card(app, first))
            .center_x(Length::Fill)
            .padding(Padding::from([40, 20]))
    };
    container(crate::theme::scrolling(placed))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::page)
        .into()
}
