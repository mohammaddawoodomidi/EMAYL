//! Blocked senders, filter rules and saved reply texts: what the person types and presses.

use iced::Task;
use iced::widget::text_editor;
use mailcore::rules::{Rule, RuleAction, RuleField};
use mailcore::templates::Template;

use crate::app::{Action, App, Message, Screen, blocking};

#[derive(Debug, Clone)]
pub enum DataMsg {
    ShowFilters,
    ShowTemplates,
    RulesLoaded(Result<Vec<Rule>, String>),
    FormField(RuleField),
    FormText(String),
    FormAction(RuleAction),
    FormAccount(Option<String>),
    AddRule,
    RemoveRule(String),
    ApplyNow,
    Applied(Result<usize, String>),
    /// Block the sender of the open message and move the conversation to Junk.
    BlockSender,
    Blocked(Result<String, String>),
    TemplatesLoaded(Result<Vec<Template>, String>),
    TemplateName(String),
    TemplateBody(text_editor::Action),
    AddTemplate,
    RemoveTemplate(String),
}

#[derive(Debug, Clone)]
pub struct RuleForm {
    pub field: RuleField,
    pub text: String,
    pub action: RuleAction,
    /// `None`: every account.
    pub account: Option<String>,
    pub error: Option<String>,
}

impl Default for RuleForm {
    fn default() -> Self {
        Self {
            field: RuleField::From,
            text: String::new(),
            action: RuleAction::Junk,
            account: None,
            error: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct TemplateForm {
    pub name: String,
    pub body: text_editor::Content,
    pub error: Option<String>,
}

impl App {
    pub(crate) fn load_rules(&self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        Task::perform(blocking(move || svc.list_rules()), |r| {
            Message::Data(DataMsg::RulesLoaded(
                r.unwrap_or_else(|| Err("The rules could not be read.".into())),
            ))
        })
    }

    pub(crate) fn load_templates(&self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        Task::perform(blocking(move || svc.list_templates()), |r| {
            Message::Data(DataMsg::TemplatesLoaded(
                r.unwrap_or_else(|| Err("The templates could not be read.".into())),
            ))
        })
    }

    pub(crate) fn on_data(&mut self, m: DataMsg) -> Task<Message> {
        match m {
            DataMsg::ShowFilters => {
                self.screen = Screen::Filters;
                self.rule_form.error = None;
                self.rules_note = None;
                return self.load_rules();
            }
            DataMsg::ShowTemplates => {
                self.screen = Screen::Templates;
                self.template_form.error = None;
                return self.load_templates();
            }
            DataMsg::RulesLoaded(r) => match r {
                Ok(v) => {
                    self.rules = v;
                    self.rules_problem = None;
                }
                Err(e) => self.rules_problem = Some(e),
            },
            DataMsg::FormField(f) => self.rule_form.field = f,
            DataMsg::FormText(t) => {
                self.rule_form.text = t;
                self.rule_form.error = None;
            }
            DataMsg::FormAction(a) => self.rule_form.action = a,
            DataMsg::FormAccount(a) => self.rule_form.account = a,
            DataMsg::AddRule => {
                let f = &self.rule_form;
                match Rule::new(f.account.clone(), f.field, &f.text, f.action) {
                    Err(e) => self.rule_form.error = Some(e),
                    Ok(rule) => {
                        let Some(svc) = self.svc.clone() else {
                            return Task::none();
                        };
                        self.rule_form.text.clear();
                        self.rule_form.error = None;
                        return Task::perform(blocking(move || svc.add_rule(rule)), |r| {
                            Message::Data(DataMsg::RulesLoaded(
                                r.unwrap_or_else(|| Err("The rule could not be saved.".into())),
                            ))
                        });
                    }
                }
            }
            DataMsg::RemoveRule(id) => {
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                return Task::perform(blocking(move || svc.remove_rule(&id)), |r| {
                    Message::Data(DataMsg::RulesLoaded(
                        r.unwrap_or_else(|| Err("The rule could not be removed.".into())),
                    ))
                });
            }
            DataMsg::ApplyNow => {
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                self.rules_note = Some("Working…".into());
                return Task::perform(blocking(move || svc.apply_rules_now()), |r| {
                    Message::Data(DataMsg::Applied(
                        r.unwrap_or_else(|| Err("It did not finish.".into())),
                    ))
                });
            }
            DataMsg::Applied(r) => {
                self.rules_note = Some(match r {
                    Ok(0) => "Nothing in your inboxes matched, or it was already as the rules say."
                        .to_owned(),
                    Ok(n) => format!(
                        "Changed {n} message{}. The change is sent to your mail server in the background.",
                        if n == 1 { "" } else { "s" }
                    ),
                    Err(e) => e,
                });
                self.wake_all();
                return self.reload_all();
            }
            DataMsg::BlockSender => return self.block_sender_of_open_message(),
            DataMsg::Blocked(r) => match r {
                Ok(addr) => self.toast(
                    format!(
                        "Blocked {addr}. New mail from them goes to Junk. You can undo this in Settings > Filters."
                    ),
                    false,
                ),
                Err(e) => self.toast(e, true),
            },
            DataMsg::TemplatesLoaded(r) => match r {
                Ok(v) => {
                    self.templates = v;
                    self.template_form.error = None;
                }
                Err(e) => self.template_form.error = Some(e),
            },
            DataMsg::TemplateName(n) => {
                self.template_form.name = n;
                self.template_form.error = None;
            }
            DataMsg::TemplateBody(a) => {
                self.template_form.body.perform(a);
                self.template_form.error = None;
            }
            DataMsg::AddTemplate => {
                let name = self.template_form.name.clone();
                let body = self.template_form.body.text();
                match Template::new(&name, &body) {
                    Err(e) => self.template_form.error = Some(e),
                    Ok(t) => {
                        let Some(svc) = self.svc.clone() else {
                            return Task::none();
                        };
                        self.template_form = TemplateForm::default();
                        return Task::perform(blocking(move || svc.add_template(t)), |r| {
                            Message::Data(DataMsg::TemplatesLoaded(
                                r.unwrap_or_else(|| Err("The template could not be saved.".into())),
                            ))
                        });
                    }
                }
            }
            DataMsg::RemoveTemplate(id) => {
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                return Task::perform(blocking(move || svc.remove_template(&id)), |r| {
                    Message::Data(DataMsg::TemplatesLoaded(
                        r.unwrap_or_else(|| Err("The template could not be removed.".into())),
                    ))
                });
            }
        }
        Task::none()
    }

    fn block_sender_of_open_message(&mut self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let Some(addr) = self
            .reader
            .as_ref()
            .and_then(|r| r.messages.last())
            .map(|m| m.from_addr.trim().to_lowercase())
        else {
            return Task::none();
        };
        if self
            .accounts
            .iter()
            .any(|a| a.account.email.to_string().eq_ignore_ascii_case(&addr))
        {
            self.toast(
                "That is one of your own addresses, so it was not blocked.",
                true,
            );
            return Task::none();
        }
        let blocking_task = Task::perform(
            blocking(move || svc.block_sender(&addr).map(|()| addr)),
            |r| {
                Message::Data(DataMsg::Blocked(
                    r.unwrap_or_else(|| Err("The sender could not be blocked.".into())),
                ))
            },
        );
        Task::batch([blocking_task, self.on_action(Action::Spam)])
    }
}
