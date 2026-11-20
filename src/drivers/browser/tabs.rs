//! The tabs one case owns, and how a step finds the one it names.
//!
//! A case starts with the tab `browser.open` created. A page action such as
//! `<a target="_blank">` or `window.open` opens another tab, and Chrome reports
//! it as a target whose `openerId` is a tab this case already owns. That
//! relation is the only thing that makes a tab the case's: a browser the user
//! already runs has tabs of its own, and none of them was opened by a page of
//! this case. Teardown closes exactly the tabs of this set.
//!
//! `Target.setDiscoverTargets` reports the creation order, which is what "the
//! tab a page action opened most recently" means; `Target.getTargets` reports
//! the URL and title the tab has now, which a queued `Target.targetInfoChanged`
//! can lag behind (a title often arrives after the URL). Matching therefore
//! reads both: the events for order and the listing for the current names.

use serde_json::{Value, json};

use super::cdp::{Connection, Deadline};
use super::truncate_chars;

/// Most tabs one case may own, so a page that opens tabs in a loop cannot grow
/// a session, its handle or its teardown without bound.
pub(super) const MAX_TABS: usize = 64;

/// Most candidates an ambiguity or timeout message lists.
const MAX_LISTED: usize = 5;

/// Longest URL or title one candidate line shows.
const MAX_LABEL_CHARS: usize = 80;

/// One page target this case owns.
pub(super) struct Tab {
    /// The CDP target id.
    pub(super) target_id: String,
    /// The URL the browser last reported for this tab.
    url: String,
    /// The title the browser last reported for this tab.
    title: String,
    /// Whether the target still exists.
    alive: bool,
}

/// What `browser.switch_tab` selects, after the two string filters and the
/// optional explicit position.
#[derive(Debug, Default, Clone)]
pub(super) struct Selector {
    /// Text the tab's URL must contain.
    pub(super) url_contains: Option<String>,
    /// Text the tab's title must contain.
    pub(super) title_contains: Option<String>,
    /// Zero-based position, in the order the case created its tabs.
    pub(super) index: Option<usize>,
}

impl Selector {
    /// Whether the selector names no tab, so "the most recently opened one"
    /// applies.
    fn is_empty(&self) -> bool {
        self.url_contains.is_none() && self.title_contains.is_none() && self.index.is_none()
    }

    /// Whether `tab` satisfies every filter.
    fn matches(&self, tab: &Tab) -> bool {
        if let Some(url) = &self.url_contains
            && !tab.url.contains(url.as_str())
        {
            return false;
        }
        if let Some(title) = &self.title_contains
            && !tab.title.contains(title.as_str())
        {
            return false;
        }
        true
    }

    /// The selector as a phrase a step error can name.
    fn describe(&self) -> String {
        let mut parts = Vec::with_capacity(3);
        if let Some(url) = &self.url_contains {
            parts.push(format!("`url_contains` `{url}`"));
        }
        if let Some(title) = &self.title_contains {
            parts.push(format!("`title_contains` `{title}`"));
        }
        if let Some(index) = self.index {
            parts.push(format!("`index` {index}"));
        }
        parts.join(" and ")
    }

    /// The selector as the object of "no tab matched".
    fn wanted(&self) -> String {
        let mut parts = Vec::with_capacity(3);
        if let Some(url) = &self.url_contains {
            parts.push(format!("a URL containing `{url}`"));
        }
        if let Some(title) = &self.title_contains {
            parts.push(format!("a title containing `{title}`"));
        }
        if let Some(index) = self.index {
            parts.push(format!("position {index}"));
        }
        parts.join(" and ")
    }
}

/// What one look at the tracked tabs found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Selection {
    /// The position, in creation order, of the one tab to switch to.
    Tab(usize),
    /// More than one tab matches; the positions to report.
    Ambiguous(Vec<usize>),
    /// No tab matches yet.
    None,
}

/// The tabs one case owns, in the order the case created them.
pub(super) struct Tabs {
    /// The first entry is the tab `browser.open` created; every later one was
    /// opened by a page of this case.
    tabs: Vec<Tab>,
    /// The target id of the tab the session drives now.
    current: String,
    /// The browser context the case's first tab lives in, when the browser
    /// reported one; a tab of another context is never adopted.
    context: Option<String>,
}

impl Tabs {
    /// Records the tab the browser just created for this case and starts
    /// tracking the tabs the case's pages open.
    pub(super) fn start(
        connection: &mut Connection,
        target_id: &str,
        deadline: Deadline,
    ) -> Result<Self, String> {
        let mut tabs = Self::attached(target_id);
        tabs.enable_discovery(connection, deadline)?;
        tabs.absorb(connection);
        Ok(tabs)
    }

    /// Records the tab the browser just created for this case.
    pub(super) fn attached(target_id: &str) -> Self {
        Self {
            tabs: vec![Tab::new(target_id)],
            current: target_id.to_string(),
            context: None,
        }
    }

    /// Rebuilds the set from a detached session's handle.
    pub(super) fn restored(target_id: &str, ids: &[String]) -> Self {
        let mut tabs = Vec::with_capacity(ids.len().max(1));
        for id in ids {
            if !tabs.iter().any(|tab: &Tab| tab.target_id == *id) {
                tabs.push(Tab::new(id));
            }
        }
        if !tabs.iter().any(|tab| tab.target_id == target_id) {
            tabs.insert(0, Tab::new(target_id));
        }
        Self {
            tabs,
            current: target_id.to_string(),
            context: None,
        }
    }

    /// The tab the session drives now.
    pub(super) fn current(&self) -> &str {
        &self.current
    }

    /// Every target id this case owns, in creation order.
    pub(super) fn ids(&self) -> Vec<String> {
        self.tabs.iter().map(|tab| tab.target_id.clone()).collect()
    }

    /// The target id at `index`, when a selection names it.
    pub(super) fn target(&self, index: usize) -> Option<&str> {
        self.tabs.get(index).map(|tab| tab.target_id.as_str())
    }

    /// The URL and title the browser last reported for `index`.
    pub(super) fn info(&self, index: usize) -> Option<(String, String)> {
        self.tabs
            .get(index)
            .map(|tab| (tab.url.clone(), tab.title.clone()))
    }

    /// Records that the session drives `target_id` now.
    pub(super) fn set_current(&mut self, target_id: &str, url: &str, title: &str) {
        self.current = target_id.to_string();
        match self.tabs.iter_mut().find(|tab| tab.target_id == target_id) {
            Some(tab) => {
                tab.url = url.to_string();
                tab.title = title.to_string();
                tab.alive = true;
            }
            None => {
                let mut tab = Tab::new(target_id);
                tab.url = url.to_string();
                tab.title = title.to_string();
                self.tabs.push(tab);
            }
        }
    }

    /// Whether the browser last reported `target_id` as a blank document.
    pub(super) fn is_blank(&self, target_id: &str) -> bool {
        self.tabs
            .iter()
            .find(|tab| tab.target_id == target_id)
            .is_some_and(|tab| tab.url.is_empty() || tab.url == "about:blank")
    }

    /// Asks the browser to report the targets it creates and destroys.
    pub(super) fn enable_discovery(
        &mut self,
        connection: &mut Connection,
        deadline: Deadline,
    ) -> Result<(), String> {
        connection
            .call(
                "Target.setDiscoverTargets",
                json!({ "discover": true }),
                None,
                deadline,
            )
            .map_err(|error| format!("could not start tracking the browser's tabs: {error}"))?;
        Ok(())
    }

    /// Merges the target events already received, keeping every other message.
    ///
    /// The events of the tab the session drives now stay queued: they explain a
    /// page failure and must reach the diagnostics of the failing step.
    pub(super) fn absorb(&mut self, connection: &mut Connection) {
        let messages = connection.take_messages();
        let mut kept = Vec::with_capacity(messages.len());
        for message in messages {
            if !self.handle(&message) {
                kept.push(message);
            }
        }
        connection.restore_messages(kept);
    }

    /// Reads the browser's current target list.
    ///
    /// This is what makes a URL or title match see what the tab shows now, and
    /// what catches a tab whose creation event arrived before discovery was on.
    pub(super) fn refresh(
        &mut self,
        connection: &mut Connection,
        deadline: Deadline,
    ) -> Result<(), String> {
        let reply = connection
            .call("Target.getTargets", json!({}), None, deadline)
            .map_err(|error| format!("could not list the browser's tabs: {error}"))?;
        let Some(infos) = reply.get("targetInfos").and_then(Value::as_array) else {
            // A reply without a list must not be read as "the browser has no
            // tabs": that would mark every tab dead and close nothing later.
            return Err("the browser did not list its tabs; the DevTools protocol reply has no `targetInfos`".to_string());
        };
        for info in infos {
            let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
                continue;
            };
            if self.tabs.iter().any(|tab| tab.target_id == target_id) {
                self.update(target_id, info);
            } else {
                self.adopt(info);
            }
        }
        for tab in &mut self.tabs {
            if !infos
                .iter()
                .any(|info| info.get("targetId").and_then(Value::as_str) == Some(&tab.target_id))
            {
                tab.alive = false;
            }
        }
        Ok(())
    }

    /// Picks the tab the selector names, or why it cannot be picked yet.
    pub(super) fn select(&self, selector: &Selector) -> Selection {
        if let Some(index) = selector.index {
            let matches = self
                .tabs
                .get(index)
                .is_some_and(|tab| tab.alive && selector.matches(tab));
            return if matches {
                Selection::Tab(index)
            } else {
                Selection::None
            };
        }
        let matching: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.alive && selector.matches(tab))
            .map(|(index, _)| index)
            .collect();
        if selector.is_empty() {
            // "The tab a page action opened most recently" never means the tab
            // `browser.open` created, so a case that opened nothing keeps
            // waiting instead of silently switching to the tab it is on.
            return matching
                .iter()
                .rev()
                .copied()
                .find(|index| *index > 0)
                .map_or(Selection::None, Selection::Tab);
        }
        match matching.len() {
            0 => Selection::None,
            1 => Selection::Tab(matching[0]),
            _ => Selection::Ambiguous(matching),
        }
    }

    /// The message of a `browser.switch_tab` that found no tab in time.
    pub(super) fn timeout_message(&self, selector: &Selector, timeout_ms: u128) -> String {
        let wanted = if selector.is_empty() {
            "no tab was opened by this case's pages".to_string()
        } else {
            format!("no tab matched {}", selector.wanted())
        };
        format!("{wanted} within {timeout_ms} ms (tabs: {})", self.list())
    }

    /// The message of a `browser.switch_tab` that matched more than one tab.
    pub(super) fn ambiguous_message(&self, selector: &Selector, candidates: &[usize]) -> String {
        let listed: Vec<&usize> = candidates.iter().take(MAX_LISTED).collect();
        let mut parts = Vec::with_capacity(listed.len());
        for index in listed {
            parts.push(self.describe(*index));
        }
        let mut candidates_text = parts.join(", ");
        if candidates.len() > MAX_LISTED {
            candidates_text.push_str(&format!(" (showing {MAX_LISTED} of {})", candidates.len()));
        }
        format!(
            "`browser.switch_tab` matched {} tabs for {}; narrow it with `title_contains` or `index` (candidates: {candidates_text})",
            candidates.len(),
            selector.describe()
        )
    }

    /// Every live tab as a candidate list.
    fn list(&self) -> String {
        let live: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.alive)
            .map(|(index, _)| index)
            .collect();
        if live.is_empty() {
            return "none left".to_string();
        }
        let listed: Vec<&usize> = live.iter().take(MAX_LISTED).collect();
        let mut parts = Vec::with_capacity(listed.len());
        for index in listed {
            parts.push(self.describe(*index));
        }
        let mut text = parts.join(", ");
        if live.len() > MAX_LISTED {
            text.push_str(&format!(" (showing {MAX_LISTED} of {})", live.len()));
        }
        text
    }

    /// One tab as `position: "url" "title"`.
    fn describe(&self, index: usize) -> String {
        let Some(tab) = self.tabs.get(index) else {
            return format!("{index}: <missing>");
        };
        format!(
            "{index}: \"{}\" \"{}\"",
            truncate_chars(&tab.url, MAX_LABEL_CHARS),
            truncate_chars(&tab.title, MAX_LABEL_CHARS)
        )
    }

    /// Merges one CDP message, reporting whether this module consumed it.
    fn handle(&mut self, message: &Value) -> bool {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return false;
        };
        let params = message.get("params").unwrap_or(&Value::Null);
        match method {
            "Target.targetCreated" => {
                if let Some(info) = params.get("targetInfo") {
                    self.created(info)
                } else {
                    false
                }
            }
            "Target.targetInfoChanged" => {
                if let Some(info) = params.get("targetInfo") {
                    self.changed(info)
                } else {
                    false
                }
            }
            "Target.targetDestroyed" => {
                if let Some(target_id) = params.get("targetId").and_then(Value::as_str) {
                    self.destroyed(target_id)
                } else {
                    false
                }
            }
            // Attaching and detaching are what this session does to the case's
            // own tabs. An event about another tab is stale news: it would
            // otherwise abort the next `Target.attachToTarget` for a tab that is
            // still open, because a detach names its target too.
            "Target.attachedToTarget" => params
                .pointer("/targetInfo/targetId")
                .and_then(Value::as_str)
                .is_some_and(|target_id| target_id != self.current),
            "Target.detachedFromTarget" => params
                .get("targetId")
                .and_then(Value::as_str)
                .is_some_and(|target_id| target_id != self.current),
            _ => false,
        }
    }

    /// Adds a page target a page of this case opened.
    fn created(&mut self, info: &Value) -> bool {
        let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
            return false;
        };
        // The session's own tab keeps its events: they explain a page failure
        // and must reach the diagnostics of the failing step.
        if target_id == self.current {
            return false;
        }
        if self.tabs.iter().any(|tab| tab.target_id == target_id) {
            self.update(target_id, info);
        } else {
            self.adopt(info);
        }
        true
    }

    /// Updates a tab this case owns, ignoring every other target.
    fn changed(&mut self, info: &Value) -> bool {
        let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
            return false;
        };
        if target_id == self.current {
            return false;
        }
        if self.tabs.iter().any(|tab| tab.target_id == target_id) {
            self.update(target_id, info);
        }
        true
    }

    /// Marks the tab the browser destroyed.
    fn destroyed(&mut self, target_id: &str) -> bool {
        if target_id == self.current {
            return false;
        }
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.target_id == target_id) {
            tab.alive = false;
        }
        true
    }

    /// Copies the URL and title the browser reports for a tab this case owns.
    fn update(&mut self, target_id: &str, info: &Value) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.target_id == target_id) {
            if let Some(url) = info.get("url").and_then(Value::as_str) {
                tab.url = url.to_string();
            }
            if let Some(title) = info.get("title").and_then(Value::as_str) {
                tab.title = title.to_string();
            }
            tab.alive = true;
        }
        if self.context.is_none()
            && self
                .tabs
                .first()
                .is_some_and(|tab| tab.target_id == target_id)
        {
            self.context = info
                .get("browserContextId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }

    /// Adds `info` when a page of this case opened it.
    fn adopt(&mut self, info: &Value) {
        if info.get("type").and_then(Value::as_str) != Some("page") {
            return;
        }
        let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
            return;
        };
        if target_id == self.current || self.tabs.iter().any(|tab| tab.target_id == target_id) {
            return;
        }
        let Some(opener) = info.get("openerId").and_then(Value::as_str) else {
            return;
        };
        // Only a tab an owned page opened is this case's; a tab the user opened
        // before the case started has no opener inside the set.
        if !self.tabs.iter().any(|tab| tab.target_id == opener) {
            return;
        }
        if let (Some(context), Some(candidate)) = (
            self.context.as_deref(),
            info.get("browserContextId").and_then(Value::as_str),
        ) && context != candidate
        {
            return;
        }
        if self.tabs.len() >= MAX_TABS {
            return;
        }
        let mut tab = Tab::new(target_id);
        if let Some(url) = info.get("url").and_then(Value::as_str) {
            tab.url = url.to_string();
        }
        if let Some(title) = info.get("title").and_then(Value::as_str) {
            tab.title = title.to_string();
        }
        self.tabs.push(tab);
    }
}

impl Tab {
    /// A tab the browser just reported, before any URL or title is known.
    fn new(target_id: &str) -> Self {
        Self {
            target_id: target_id.to_string(),
            url: String::new(),
            title: String::new(),
            alive: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A set with one initial tab and one tab a page action opened.
    fn two_tabs() -> Tabs {
        let mut tabs = Tabs::attached("initial");
        tabs.adopt(&json!({
            "targetId": "opened",
            "type": "page",
            "url": "http://127.0.0.1/child",
            "title": "Child",
            "openerId": "initial",
        }));
        tabs.set_current("initial", "http://127.0.0.1/parent", "Parent");
        tabs
    }

    #[test]
    fn a_tab_needs_an_opener_this_case_owns() {
        let mut tabs = Tabs::attached("initial");
        tabs.adopt(&json!({
            "targetId": "user",
            "type": "page",
            "url": "http://127.0.0.1/user",
            "title": "User",
        }));
        tabs.adopt(&json!({
            "targetId": "foreign",
            "type": "page",
            "url": "http://127.0.0.1/other",
            "openerId": "user",
        }));
        tabs.adopt(&json!({
            "targetId": "worker",
            "type": "service_worker",
            "openerId": "initial",
        }));
        assert_eq!(tabs.ids(), vec!["initial".to_string()]);
    }

    #[test]
    fn a_tab_of_another_browser_context_is_not_adopted() {
        let mut tabs = Tabs::attached("initial");
        tabs.update(
            "initial",
            &json!({
                "targetId": "initial",
                "type": "page",
                "browserContextId": "context-a",
            }),
        );
        tabs.adopt(&json!({
            "targetId": "other-context",
            "type": "page",
            "openerId": "initial",
            "browserContextId": "context-b",
        }));
        tabs.adopt(&json!({
            "targetId": "same-context",
            "type": "page",
            "openerId": "initial",
            "browserContextId": "context-a",
        }));
        assert_eq!(
            tabs.ids(),
            vec!["initial".to_string(), "same-context".to_string()]
        );
    }

    #[test]
    fn the_newest_derived_tab_is_the_default_selection() {
        let tabs = two_tabs();
        assert_eq!(
            tabs.select(&Selector::default()),
            Selection::Tab(1),
            "the tab a page action opened is the one the default selector names"
        );
    }

    #[test]
    fn the_default_selection_never_picks_the_initial_tab() {
        let tabs = Tabs::attached("initial");
        assert_eq!(
            tabs.select(&Selector::default()),
            Selection::None,
            "a case that opened no tab must not switch to the tab it is already on"
        );
    }

    #[test]
    fn a_url_or_title_filter_matches_a_substring() {
        let tabs = two_tabs();
        assert_eq!(
            tabs.select(&Selector {
                url_contains: Some("/child".to_string()),
                ..Selector::default()
            }),
            Selection::Tab(1)
        );
        assert_eq!(
            tabs.select(&Selector {
                title_contains: Some("Paren".to_string()),
                ..Selector::default()
            }),
            Selection::Tab(0),
            "a filter may name the tab the case started on"
        );
        assert_eq!(
            tabs.select(&Selector {
                url_contains: Some("/missing".to_string()),
                ..Selector::default()
            }),
            Selection::None
        );
    }

    #[test]
    fn two_matching_tabs_are_ambiguous() {
        let mut tabs = two_tabs();
        tabs.adopt(&json!({
            "targetId": "opened-2",
            "type": "page",
            "url": "http://127.0.0.1/child-two",
            "title": "Child Two",
            "openerId": "initial",
        }));
        let message = tabs.ambiguous_message(
            &Selector {
                url_contains: Some("/child".to_string()),
                ..Selector::default()
            },
            &[1, 2],
        );
        assert!(
            message.contains("matched 2 tabs") && message.contains("1: \"http://127.0.0.1/child\""),
            "unexpected ambiguity message: {message}"
        );
        assert_eq!(
            tabs.select(&Selector {
                url_contains: Some("/child".to_string()),
                ..Selector::default()
            }),
            Selection::Ambiguous(vec![1, 2])
        );
    }

    #[test]
    fn an_explicit_position_selects_even_when_filters_are_given() {
        let tabs = two_tabs();
        assert_eq!(
            tabs.select(&Selector {
                index: Some(0),
                ..Selector::default()
            }),
            Selection::Tab(0)
        );
        assert_eq!(
            tabs.select(&Selector {
                index: Some(2),
                ..Selector::default()
            }),
            Selection::None,
            "a position that does not exist yet waits for a tab to fill it"
        );
    }

    #[test]
    fn a_destroyed_tab_is_not_selected() {
        let mut tabs = two_tabs();
        tabs.destroyed("opened");
        assert_eq!(tabs.select(&Selector::default()), Selection::None);
        assert!(tabs.ids().contains(&"opened".to_string()));
    }

    #[test]
    fn the_listing_names_the_tabs_and_their_positions() {
        let tabs = two_tabs();
        let message = tabs.timeout_message(&Selector::default(), 400);
        assert!(
            message.contains("no tab was opened by this case's pages within 400 ms"),
            "unexpected timeout message: {message}"
        );
        assert!(
            message.contains("0: \"http://127.0.0.1/parent\" \"Parent\"")
                && message.contains("1: \"http://127.0.0.1/child\" \"Child\""),
            "the timeout message must list the tabs: {message}"
        );
    }

    #[test]
    fn a_restored_set_keeps_the_handle_order() {
        let tabs = Tabs::restored("second", &["first".to_string(), "second".to_string()]);
        assert_eq!(tabs.ids(), vec!["first".to_string(), "second".to_string()]);
        assert_eq!(tabs.current(), "second");
    }
}
