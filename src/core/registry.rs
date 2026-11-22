//! The registry: step and observer names, their argument schemas and their
//! behavior.
//!
//! Steps and observers share the registry, the name rule and the shape of their
//! declaration, so a plugin registers both the same way. They are kept in
//! separate namespaces: a step named `sample.call` and an observer named
//! `sample.call` do not collide, because a case names a step in `steps:` and an
//! observer in `observe:`.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::core::observer::ObserverDefinition;
use crate::core::session::{Session, Sessions, check_kind};

/// Why a step could not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// The arguments do not satisfy the step's schema.
    InvalidArguments(String),
    /// The environment failed, for example a refused connection.
    Environment(String),
    /// The step exceeded its time budget.
    Timeout(String),
    /// The step ran but could not finish its work.
    Execution(String),
}

/// A step implementation and the contract it declares to the engine.
#[derive(Clone)]
pub struct StepDefinition {
    /// Name used in case files, for example `sample.call`.
    pub name: &'static str,
    /// One sentence describing what the step does.
    pub description: &'static str,
    /// JSON Schema for the step's arguments.
    pub args_schema: Value,
    /// Checks arguments against the same contract as [`Self::args_schema`],
    /// returning a message the user can act on.
    pub validate: fn(&Value) -> Result<(), String>,
    /// Checks whether a step may be retried with `retry` given its expanded
    /// arguments.
    ///
    /// Only read-only steps may use `retry`; state-changing steps must return
    /// an error so `da lint` and preflight refuse them before execution.
    pub check_retry: fn(&Value) -> Result<(), String>,
    /// Reports fragile but valid arguments for `da lint`.
    ///
    /// A warning never fails a case: the step is accepted and still runs. Lint
    /// prints each warning where a reader can act on it, so a step that works
    /// only on one machine or at one size can say so without blocking CI.
    /// Steps that have nothing to warn about leave this at
    /// [`no_lint_warnings`].
    pub lint_warnings: fn(&Value) -> Vec<String>,
    /// Runs the step and returns its JSON result.
    ///
    /// The [`Sessions`] value belongs to the running case. A step that needs a
    /// long-lived resource asks it for the resource on first use and receives
    /// the same one on every later step of that case.
    pub run: fn(&Value, &mut Sessions) -> Result<Value, StepError>,
}

/// Default `check_retry` implementation that refuses `retry` for steps that
/// change state.
pub fn disallow_retry(_args: &Value) -> Result<(), String> {
    Err("changes state and cannot use `retry`; only read-only steps may be retried".to_string())
}

/// `check_retry` implementation for read-only steps that are always safe to
/// poll.
pub fn allow_retry(_args: &Value) -> Result<(), String> {
    Ok(())
}

/// `lint_warnings` implementation for steps with nothing fragile to report.
pub fn no_lint_warnings(_args: &Value) -> Vec<String> {
    Vec::new()
}

/// A detachable session kind a driver registers so later processes can
/// reattach to or dispose of its detached handle.
#[derive(Clone)]
pub struct SessionDefinition {
    /// Session kind, matching the key passed to [`Sessions`].
    pub kind: &'static str,
    /// Reconnects to a detached resource from its opaque JSON handle.
    pub reattach: fn(&Value) -> Result<Box<dyn Session>, String>,
    /// Releases resources named by `handle` when reattach fails or is skipped.
    pub dispose_handle: fn(&Value) -> Result<(), String>,
}

type ValidateFn = fn(&Value) -> Result<(), String>;
type ArgRedactorFn = fn(&Value, bool) -> Value;

/// The steps, observers and detachable sessions available to the runner.
#[derive(Default)]
pub struct Registry {
    steps: BTreeMap<&'static str, StepDefinition>,
    observers: BTreeMap<&'static str, ObserverDefinition>,
    sessions: BTreeMap<&'static str, SessionDefinition>,
    session_validators: BTreeMap<&'static str, ValidateFn>,
    arg_redactors: BTreeMap<&'static str, ArgRedactorFn>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a step.
    ///
    /// Two steps with the same name would make a case ambiguous, so a
    /// duplicate is refused instead of silently replacing the first. The name
    /// itself is checked by [`validate_name`].
    pub fn register(&mut self, definition: StepDefinition) -> Result<(), String> {
        validate_name(definition.name)?;
        if self.steps.contains_key(definition.name) {
            return Err(format!(
                "step `{}` is already registered; every step name must be unique",
                definition.name
            ));
        }
        self.steps.insert(definition.name, definition);
        Ok(())
    }

    /// Looks a step up by name.
    pub fn get(&self, name: &str) -> Option<&StepDefinition> {
        self.steps.get(name)
    }

    /// Every registered step name, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.steps.keys().copied().collect()
    }

    /// Adds an observer.
    ///
    /// An observer gets the same name rule and the same uniqueness rule as a
    /// step, but its own namespace: it is declared in `observe:`, never in
    /// `steps:`.
    pub fn register_observer(&mut self, definition: ObserverDefinition) -> Result<(), String> {
        validate_name(definition.name)?;
        if self.observers.contains_key(definition.name) {
            return Err(format!(
                "observer `{}` is already registered; every observer name must be unique",
                definition.name
            ));
        }
        self.observers.insert(definition.name, definition);
        Ok(())
    }

    /// Looks an observer up by name.
    pub fn observer(&self, name: &str) -> Option<&ObserverDefinition> {
        self.observers.get(name)
    }

    /// Every registered observer name, in order.
    pub fn observer_names(&self) -> Vec<&'static str> {
        self.observers.keys().copied().collect()
    }

    /// Registers a detachable session kind so `da session` can reattach to or
    /// dispose of its handle across processes.
    pub fn register_session(&mut self, definition: SessionDefinition) -> Result<(), String> {
        check_kind(definition.kind)?;
        if self.sessions.contains_key(definition.kind) {
            return Err(format!(
                "session `{}` is already registered; every session kind must be unique",
                definition.kind
            ));
        }
        self.sessions.insert(definition.kind, definition);
        Ok(())
    }

    /// Looks up a registered detachable session kind.
    pub fn session(&self, kind: &str) -> Option<&SessionDefinition> {
        self.sessions.get(kind)
    }

    /// Registers a validator for `step_name` used inside interactive `da session`
    /// commands (where additional session-only arguments such as `ref` are allowed).
    pub fn register_session_validator(
        &mut self,
        step_name: &'static str,
        validate: fn(&Value) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.steps.contains_key(step_name) {
            return Err(format!(
                "cannot register session validator for unregistered step `{step_name}`"
            ));
        }
        self.session_validators.insert(step_name, validate);
        Ok(())
    }

    /// Validates `args` for `step_name` inside an interactive session.
    ///
    /// Falls back to the step's regular validator when no interactive-specific
    /// validator is registered.
    pub fn validate_for_session(&self, step_name: &str, args: &Value) -> Result<(), String> {
        if let Some(validate) = self.session_validators.get(step_name) {
            return validate(args);
        }
        let Some(definition) = self.steps.get(step_name) else {
            return Err(format!("`{step_name}` is not a registered step"));
        };
        (definition.validate)(args)
    }

    /// Registers a step-specific argument redactor called before step arguments
    /// enter reports, terminals, bundles (`preserve_placeholders = false`), or
    /// unexpanded session templates (`preserve_placeholders = true`).
    pub fn register_arg_redactor(
        &mut self,
        step_name: &'static str,
        redact: fn(&Value, bool) -> Value,
    ) -> Result<(), String> {
        if !self.steps.contains_key(step_name) {
            return Err(format!(
                "cannot register argument redactor for unregistered step `{step_name}`"
            ));
        }
        self.arg_redactors.insert(step_name, redact);
        Ok(())
    }

    /// Returns a scrubbed copy of resolved `args` for `step_name` using any
    /// registered argument redactor, or clones `args` unchanged when none is
    /// registered.
    pub fn redact_step_args(&self, step_name: &str, args: &Value) -> Value {
        match self.arg_redactors.get(step_name) {
            Some(redact) => redact(args, false),
            None => args.clone(),
        }
    }

    /// Returns a scrubbed copy of unexpanded template `args` for `step_name`,
    /// preserving whole-string `${env:NAME}` and `${var}` placeholders for
    /// deterministic session export while masking inline sensitive values.
    pub fn redact_template_args(&self, step_name: &str, args: &Value) -> Value {
        match self.arg_redactors.get(step_name) {
            Some(redact) => redact(args, true),
            None => args.clone(),
        }
    }
}

/// Field names every step already gives a meaning to.
const RESERVED_NAMES: [&str; 4] = ["expect", "optional", "retry", "save"];

/// Checks the name a step registers under.
///
/// A name is written in every case that uses the step and becomes a key of the
/// schema's `$defs`, so it is limited to characters that need no escaping and
/// that read the same on every platform. The fields a step already carries,
/// `expect`, `optional`, `retry` and `save`, cannot also name a step.
fn validate_name(name: &str) -> Result<(), String> {
    if RESERVED_NAMES.contains(&name) {
        return Err(format!(
            "step name `{name}` is reserved for the case format; choose another name"
        ));
    }
    if name.is_empty() {
        return Err(
            "step name is empty; use lowercase letters, digits, `.`, `_` and `-`".to_string(),
        );
    }
    if let Some(character) = name
        .chars()
        .find(|character| !is_allowed_name_character(*character))
    {
        return Err(format!(
            "step name `{name}` contains `{character}`; use only lowercase letters, digits, `.`, `_` and `-`"
        ));
    }
    Ok(())
}

/// Whether one character may appear in a name the registry holds.
///
/// The rule is shared with the names a case gives its observations, so a key of
/// `observed` reads the same way as a step or observer name.
pub fn is_allowed_name_character(character: char) -> bool {
    character.is_ascii_lowercase()
        || character.is_ascii_digit()
        || matches!(character, '.' | '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::observer::Observer;

    fn definition(name: &'static str) -> StepDefinition {
        StepDefinition {
            name,
            description: "Test step.",
            args_schema: serde_json::json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: |_, _| Ok(Value::Null),
        }
    }

    /// An observer that collects nothing.
    struct Silent;

    impl Observer for Silent {
        fn start(
            &mut self,
            _sessions: &mut Sessions,
        ) -> Result<(), crate::core::observer::ObserverError> {
            Ok(())
        }

        fn collect(
            &mut self,
            _sessions: &mut Sessions,
        ) -> Result<Value, crate::core::observer::ObserverError> {
            Ok(Value::Null)
        }
    }

    fn observer(name: &'static str) -> crate::core::observer::ObserverDefinition {
        crate::core::observer::ObserverDefinition {
            name,
            description: "Test observer.",
            args_schema: serde_json::json!({"type": "object"}),
            validate: |_| Ok(()),
            create: |_| Ok(Box::new(Silent)),
        }
    }

    #[test]
    fn registers_and_looks_up_steps() {
        let mut registry = Registry::new();
        registry
            .register(definition("test.one"))
            .expect("the first registration must succeed");

        assert!(registry.get("test.one").is_some());
        assert!(registry.get("test.two").is_none());
        assert_eq!(registry.names(), vec!["test.one"]);
    }

    #[test]
    fn refuses_a_duplicate_name() {
        let mut registry = Registry::new();
        registry
            .register(definition("test.one"))
            .expect("the first registration must succeed");

        let error = registry
            .register(definition("test.one"))
            .expect_err("the duplicate must be refused");
        assert!(error.contains("test.one"), "got: {error}");
    }

    #[test]
    fn accepts_the_characters_a_step_name_may_use() {
        for name in ["demo.call", "demo_call", "demo-call", "demo2.call", "a"] {
            let mut registry = Registry::new();
            registry
                .register(definition(name))
                .unwrap_or_else(|error| panic!("`{name}` must be accepted: {error}"));
        }
    }

    #[test]
    fn refuses_a_name_with_a_character_outside_the_rule() {
        for (name, offending) in [
            ("Demo.call", 'D'),
            ("demo/Call", '/'),
            ("demo~call", '~'),
            ("demo call", ' '),
            ("demo:call", ':'),
            ("demo.cäse", 'ä'),
            ("", '\0'),
        ] {
            let mut registry = Registry::new();
            let error = registry
                .register(definition(name))
                .expect_err("the name must be refused");

            assert!(
                error.contains("lowercase letters"),
                "the message must state the rule for `{name}`, got: {error}"
            );
            if !name.is_empty() {
                assert!(
                    error.contains(offending),
                    "the message must name `{offending}` in `{name}`, got: {error}"
                );
            }
        }
    }

    #[test]
    fn refuses_the_names_the_case_format_already_uses() {
        for name in ["expect", "optional", "retry", "save"] {
            let mut registry = Registry::new();
            let error = registry
                .register(definition(name))
                .expect_err("a reserved name must be refused");

            assert!(
                error.contains("reserved"),
                "the message must say the name is reserved, got: {error}"
            );
            assert!(error.contains(name), "got: {error}");
        }
    }

    #[test]
    fn observers_are_registered_and_looked_up_separately_from_steps() {
        let mut registry = Registry::new();
        // The two namespaces do not collide: one name can be a step and an
        // observer, because a case uses each in its own place.
        registry
            .register(definition("test.same"))
            .expect("the step must register");
        registry
            .register_observer(observer("test.same"))
            .expect("the observer must register");

        assert!(registry.get("test.same").is_some());
        assert!(registry.observer("test.same").is_some());
        assert!(registry.observer("test.other").is_none());
        assert_eq!(registry.names(), vec!["test.same"]);
        assert_eq!(registry.observer_names(), vec!["test.same"]);
    }

    #[test]
    fn refuses_a_duplicate_observer_name() {
        let mut registry = Registry::new();
        registry
            .register_observer(observer("test.watch"))
            .expect("the first registration must succeed");

        let error = registry
            .register_observer(observer("test.watch"))
            .expect_err("the duplicate must be refused");

        assert!(error.contains("test.watch"), "got: {error}");
        assert!(error.contains("observer"), "got: {error}");
    }

    #[test]
    fn an_observer_name_follows_the_step_name_rule() {
        for name in ["Demo.call", "demo/call", "demo call", ""] {
            let mut registry = Registry::new();
            let error = registry
                .register_observer(observer(name))
                .expect_err("the name must be refused");

            assert!(
                error.contains("lowercase letters"),
                "the message must state the rule for `{name}`, got: {error}"
            );
        }
    }

    #[test]
    fn session_registration_and_session_validators_work() {
        let mut registry = Registry::new();
        registry
            .register(definition("test.step"))
            .expect("step must register");
        registry
            .register_session(SessionDefinition {
                kind: "test.kind",
                reattach: |_| Err("gone".to_string()),
                dispose_handle: |_| Ok(()),
            })
            .expect("session kind must register");
        assert!(registry.session("test.kind").is_some());

        let dup = registry
            .register_session(SessionDefinition {
                kind: "test.kind",
                reattach: |_| Err("gone".to_string()),
                dispose_handle: |_| Ok(()),
            })
            .expect_err("duplicate session kind must fail");
        assert!(dup.contains("already registered"), "got: {dup}");

        registry
            .register_session_validator("test.step", |args| {
                if args.get("ref").is_some() {
                    Ok(())
                } else {
                    Err("missing ref".to_string())
                }
            })
            .expect("session validator must register");
        assert!(
            registry
                .validate_for_session("test.step", &serde_json::json!({ "ref": "e1" }))
                .is_ok()
        );
        assert!(
            registry
                .validate_for_session("test.step", &serde_json::json!({}))
                .is_err()
        );
    }
}
