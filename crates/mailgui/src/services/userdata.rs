//! Blocked senders, filter rules and templates, as the window uses them.

use mailcore::rules::{Rule, RuleAction, RuleField};
use mailcore::templates::Template;

use super::{Services, lock};

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Services {
    pub fn list_rules(&self) -> Result<Vec<Rule>, String> {
        let _g = lock(&self.userdata_lock);
        self.rulebook
            .load()
            .map_err(|e| format!("The list of rules could not be read: {e}"))
    }

    pub fn add_rule(&self, rule: Rule) -> Result<Vec<Rule>, String> {
        let _g = lock(&self.userdata_lock);
        let all = self.rulebook.add(rule).map_err(text)?;
        self.store.set_rules(all.clone());
        Ok(all)
    }

    pub fn remove_rule(&self, id: &str) -> Result<Vec<Rule>, String> {
        let _g = lock(&self.userdata_lock);
        let all = self.rulebook.remove(id).map_err(text)?;
        self.store.set_rules(all.clone());
        Ok(all)
    }

    /// Blocks an address for every account: new mail from it goes to Junk.
    pub fn block_sender(&self, address: &str) -> Result<(), String> {
        let rule = Rule::new(None, RuleField::From, address, RuleAction::Junk)?;
        self.add_rule(rule).map(|_| ())
    }

    /// Runs the rules over the mail already in the inboxes (blocking). Returns how many changed.
    pub fn apply_rules_now(&self) -> Result<usize, String> {
        mailcore::syncer::apply_rules_to_inboxes(&self.store).map_err(text)
    }

    pub fn list_templates(&self) -> Result<Vec<Template>, String> {
        let _g = lock(&self.userdata_lock);
        self.templatebook.load().map_err(text)
    }

    pub fn add_template(&self, t: Template) -> Result<Vec<Template>, String> {
        let _g = lock(&self.userdata_lock);
        self.templatebook.add(t).map_err(text)
    }

    pub fn remove_template(&self, id: &str) -> Result<Vec<Template>, String> {
        let _g = lock(&self.userdata_lock);
        self.templatebook.remove(id).map_err(text)
    }
}
