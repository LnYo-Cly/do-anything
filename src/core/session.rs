//! The session contract: a resource one case keeps alive across its steps.
//!
//! A step that needs a resource across steps asks [`Sessions`] for it on first
//! use. Every later step and every observation of the same case receives the
//! same value, and the runner closes it when the case ends, whether the case
//! passed, failed, errored, timed out or stopped early.
//!
//! The engine never names a concrete resource: a plugin declares its own type,
//! implements [`Session`] and chooses the kind it is stored under. The runner
//! gives each case its own [`Sessions`], so cases that run at the same time with
//! `--jobs` cannot share one.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::registry::is_allowed_name_character;

/// Makes a session downcast to the concrete type that created it.
///
/// It has a blanket implementation, so a session only implements [`Session`].
/// The trait is public because [`Session`] requires it, not because a plugin
/// should implement it directly.
pub trait AsAny: Any {
    /// Returns this value as [`Any`], which is what the downcast needs.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// One file a session offers as evidence for a failed case.
///
/// The engine does not interpret the file: a text file is redacted with the
/// run's secrets when it is written, and bytes are written unchanged. The name
/// is one safe path segment; the writer puts it under a directory named after
/// the session kind, so two sessions cannot collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    name: String,
    content: EvidenceContent,
}

/// The payload of one [`Evidence`] file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceContent {
    /// UTF-8 text, redacted before it is written.
    Text(String),
    /// Bytes written unchanged, for example an image.
    Bytes(Vec<u8>),
}

impl Evidence {
    /// Creates a text evidence file.
    ///
    /// The name must be one safe path segment and must not shadow a file the
    /// bundle layout already uses.
    pub fn text(name: impl Into<String>, text: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        check_evidence_name(&name)?;
        Ok(Self {
            name,
            content: EvidenceContent::Text(text.into()),
        })
    }

    /// Creates a binary evidence file.
    pub fn bytes(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Result<Self, String> {
        let name = name.into();
        check_evidence_name(&name)?;
        Ok(Self {
            name,
            content: EvidenceContent::Bytes(bytes.into()),
        })
    }

    /// The file name, as one path segment.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file content.
    pub fn content(&self) -> &EvidenceContent {
        &self.content
    }
}

/// The evidence one session returned, or why it could not return any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvidence {
    /// Session kind, used as the directory that holds the files.
    pub kind: String,
    /// Files the session returned.
    pub files: Vec<Evidence>,
    /// Why the session could not return its evidence, if it failed.
    pub error: Option<String>,
    /// Whether this session's evidence contains unmanaged state and must stay local-only.
    pub evidence_is_local_only: bool,
    /// Why this session's evidence must stay local-only, if applicable.
    pub local_only_reason: Option<String>,
}

/// A resource a case keeps alive across its steps.
///
/// The runner calls [`Self::close`] exactly once when the case ends. An
/// implementation must release everything it holds, including a connection or a
/// child process, and return a message the user can act on if it cannot.
///
/// `Send` is required because a case may run on a worker thread. A session
/// belongs to one case; it is never shared with another case, even under
/// `--jobs`.
pub trait Session: Send + AsAny {
    /// Releases the resource.
    ///
    /// The message is reported as a close error. The runner has already decided
    /// the case's own result, so a close failure never hides it.
    fn close(&mut self) -> Result<(), String>;

    /// Returns files that help diagnose a case that did not pass.
    ///
    /// The runner asks only after a case did not pass and only when failure
    /// bundles are enabled, before [`Self::close`] releases the resource. The
    /// default is no evidence, which is what a session with nothing to add
    /// returns.
    fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        Ok(Vec::new())
    }

    /// Returns per-step diagnostic files when `--trace-steps` is enabled.
    ///
    /// Called only immediately after an executed step when the runner has
    /// tracing turned on. Default is empty so opt-in is zero-overhead for
    /// sessions that do not capture per-step artifacts.
    fn step_evidence(&mut self) -> Result<Vec<Evidence>, String> {
        Ok(Vec::new())
    }

    /// Detaches this resource so a later process can reattach without closing it.
    ///
    /// Returns `Ok(Some(handle))` with an opaque JSON handle when the session
    /// supports persistence across separate commands, or `Ok(None)` when it
    /// only lives within one run. After `Ok(Some(_))`, [`Self::close`] and
    /// `Drop` must leave the underlying resource running.
    fn detach(&mut self) -> Result<Option<Value>, String> {
        Ok(None)
    }

    /// Returns an interactive snapshot of this session's current state,
    /// optionally scoped to `scope_ref` and bounded by `max_depth` / `max_nodes`.
    ///
    /// Sessions that do not expose snapshots return `Ok(None)`.
    fn snapshot(
        &mut self,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        let _ = (scope_ref, max_depth, max_nodes);
        Ok(None)
    }

    /// Returns a post-step snapshot only when this session participated in the
    /// step that just ran.
    fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        Ok(None)
    }

    /// Returns structured metadata produced by the most recent step on this
    /// session, clearing it afterward.
    fn take_step_metadata(&mut self) -> Option<Value> {
        None
    }

    /// Returns a reason why evidence from this session must stay local-only,
    /// or `None` when source-based redaction covers all state.
    fn local_only_reason(&self) -> Option<String> {
        None
    }

    /// Captures an input-masked PNG frame and its logical `{ width, height }`
    /// bounds for interactive visual locating.
    ///
    /// Sessions without a visual surface return `Ok(None)`.
    fn masked_viewport_capture(&mut self) -> Result<Option<MaskedViewportCapture>, String> {
        Ok(None)
    }

    /// Synthesizes a stable target descriptor for a snapshot `ref` without
    /// executing a step on it.
    fn synthesize_ref(&mut self, ref_id: &str) -> Result<Option<SynthesizedTarget>, String> {
        let _ = ref_id;
        Ok(None)
    }

    /// Reverse-queries the live session tree at `(x, y)` to synthesize a stable
    /// target descriptor when an addressable element occupies that point.
    ///
    /// Returns `Ok(None)` when the point lands on an opaque surface without an
    /// addressable element or when the session does not support point lookup.
    fn synthesize_at_point(&mut self, x: f64, y: f64) -> Result<Option<SynthesizedTarget>, String> {
        let _ = (x, y);
        Ok(None)
    }
}

/// An input-masked PNG frame and logical dimensions captured from a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedViewportCapture {
    /// Input-masked PNG bytes (`image/png`).
    pub png: Vec<u8>,
    /// Logical surface width in pixels.
    pub width: u32,
    /// Logical surface height in pixels.
    pub height: u32,
}

/// A deterministic target descriptor synthesized from a live session element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedTarget {
    /// Target fields (such as `role`/`name` or `testid`) valid in a case file.
    pub locator: Value,
    /// Whether the target relies on a structural path fallback.
    pub fragile: bool,
}

/// Handles returned when open sessions detach at the end of an interactive
/// command.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DetachedSessions {
    /// Opaque JSON handles keyed by session kind.
    pub handles: BTreeMap<String, Value>,
    /// Errors reported while detaching or closing non-detachable sessions.
    pub errors: Vec<String>,
}

/// The sessions of one case, keyed by the kind the plugin chose.
///
/// A kind follows the step name rule: lowercase letters, digits, `.`, `_` and
/// `-`. It is a plugin's own namespace, so two unrelated resources can share
/// one case without naming each other.
#[derive(Default)]
pub struct Sessions {
    /// In creation order; [`Self::close_all`] pops from the end.
    entries: Vec<SessionEntry>,
    /// Directory a session resolves a relative path against when it writes one.
    base_directory: Option<PathBuf>,
    /// Whether steps execute inside an interactive `da session` command.
    interactive: bool,
}

/// One open session and the kind it is stored under.
struct SessionEntry {
    kind: String,
    session: Box<dyn Session>,
}

impl Sessions {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the directory a session resolves a relative output path against.
    ///
    /// The runner sets this to the directory that holds the case file before
    /// the first step runs. A session that does not write a path ignores it.
    pub fn set_base_directory(&mut self, directory: impl Into<PathBuf>) {
        self.base_directory = Some(directory.into());
    }

    /// The directory a session resolves a relative output path against.
    pub fn base_directory(&self) -> Option<&Path> {
        self.base_directory.as_deref()
    }

    /// Marks whether steps execute inside an interactive `da session` command.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.interactive = interactive;
    }

    /// Whether steps execute inside an interactive `da session` command.
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Inserts a reattached session under `kind`.
    ///
    /// Fails if `kind` is invalid or already open in this [`Sessions`].
    pub fn insert_reattached(
        &mut self,
        kind: &str,
        session: Box<dyn Session>,
    ) -> Result<(), String> {
        check_kind(kind)?;
        if self.entries.iter().any(|entry| entry.kind == kind) {
            return Err(format!("session `{kind}` is already open"));
        }
        self.entries.push(SessionEntry {
            kind: kind.to_string(),
            session,
        });
        Ok(())
    }

    /// Returns the session of `kind`, creating it with `create` on first use.
    ///
    /// The generic type is the concrete session type. A kind that is already
    /// open with another type is an error, not a panic: one kind holds one
    /// concrete type for the whole case.
    ///
    /// The factory runs at most once per case. If it returns an error, no slot
    /// is left behind, so a later step may try again.
    pub fn get_or_create<T>(
        &mut self,
        kind: &str,
        create: impl FnOnce() -> Result<T, String>,
    ) -> Result<&mut T, String>
    where
        T: Session + 'static,
    {
        check_kind(kind)?;
        if let Some(index) = self.entries.iter().position(|entry| entry.kind == kind) {
            return downcast(&mut self.entries[index], kind);
        }
        let session =
            create().map_err(|message| format!("could not create session `{kind}`: {message}"))?;
        self.entries.push(SessionEntry {
            kind: kind.to_string(),
            session: Box::new(session),
        });
        let entry = self
            .entries
            .last_mut()
            .ok_or_else(|| format!("could not store session `{kind}`"))?;
        downcast(entry, kind)
    }

    /// Returns the session of `kind` without creating it.
    ///
    /// A missing session is `Ok(None)`: an observation may run before the step
    /// that opens the resource. A kind that holds another type is an error.
    pub fn get<T>(&mut self, kind: &str) -> Result<Option<&mut T>, String>
    where
        T: Session + 'static,
    {
        check_kind(kind)?;
        match self.entries.iter().position(|entry| entry.kind == kind) {
            Some(index) => downcast(&mut self.entries[index], kind).map(Some),
            None => Ok(None),
        }
    }

    /// Asks every open session for its failure evidence.
    ///
    /// A session that fails is reported in its group's `error` instead of
    /// stopping the others. A session that returns no files and no error is
    /// left out, so a case with no evidence adds no directory to its bundle.
    pub fn collect_evidence(&mut self) -> Vec<SessionEvidence> {
        self.collect_evidence_with(|session| session.evidence())
    }

    /// Collects per-step evidence from every open session after one step runs.
    ///
    /// Sessions that return no files and no error are omitted.
    pub fn collect_step_evidence(&mut self) -> Vec<SessionEvidence> {
        self.collect_evidence_with(|session| session.step_evidence())
    }

    fn collect_evidence_with(
        &mut self,
        mut read: impl FnMut(&mut dyn Session) -> Result<Vec<Evidence>, String>,
    ) -> Vec<SessionEvidence> {
        let mut groups = Vec::new();
        for entry in &mut self.entries {
            let local_only_reason = entry.session.local_only_reason();
            let evidence_is_local_only = local_only_reason.is_some();
            match read(&mut *entry.session) {
                Ok(files) if files.is_empty() => {}
                Ok(files) => groups.push(SessionEvidence {
                    kind: entry.kind.to_string(),
                    files,
                    error: None,
                    evidence_is_local_only,
                    local_only_reason,
                }),
                Err(message) => groups.push(SessionEvidence {
                    kind: entry.kind.to_string(),
                    files: Vec::new(),
                    error: Some(format!("session `{}`: {message}", entry.kind)),
                    evidence_is_local_only,
                    local_only_reason,
                }),
            }
        }
        groups
    }

    /// Returns the combined local-only reason across open sessions, if any.
    pub fn local_only_reason(&self) -> Option<String> {
        let reasons: Vec<String> = self
            .entries
            .iter()
            .filter_map(|entry| entry.session.local_only_reason())
            .collect();
        if reasons.is_empty() {
            None
        } else {
            Some(reasons.join("; "))
        }
    }

    /// Closes every open session in reverse creation order.
    ///
    /// Every session is attempted, even when an earlier close fails, and each
    /// one is removed before its [`Session::close`] runs, so a failing or
    /// panicking close cannot make it close twice. The returned messages name
    /// the session kind.
    pub fn close_all(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        while let Some(mut entry) = self.entries.pop() {
            if let Err(message) = entry.session.close() {
                errors.push(format!(
                    "could not close session `{}`: {message}",
                    entry.kind
                ));
            }
        }
        errors
    }

    /// Detaches every open session that supports persistence, closing any
    /// non-detachable sessions instead.
    pub fn detach_all(&mut self) -> DetachedSessions {
        let mut out = DetachedSessions::default();
        while let Some(mut entry) = self.entries.pop() {
            match entry.session.detach() {
                Ok(Some(handle)) => {
                    out.handles.insert(entry.kind.to_string(), handle);
                }
                Ok(None) => {
                    if let Err(message) = entry.session.close() {
                        out.errors.push(format!(
                            "could not close session `{}`: {message}",
                            entry.kind
                        ));
                    }
                }
                Err(message) => {
                    out.errors.push(format!(
                        "could not detach session `{}`: {message}",
                        entry.kind
                    ));
                }
            }
        }
        out
    }

    /// Collects a snapshot from the first open session that produces one.
    pub fn collect_snapshot(
        &mut self,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<(String, Value)>, String> {
        for entry in &mut self.entries {
            if let Some(snapshot) = entry.session.snapshot(scope_ref, max_depth, max_nodes)? {
                return Ok(Some((entry.kind.to_string(), snapshot)));
            }
        }
        Ok(None)
    }

    /// Collects a post-step snapshot from any session that participated in the
    /// step that just finished.
    pub fn collect_step_snapshot(&mut self) -> Result<Option<(String, Value)>, String> {
        for entry in &mut self.entries {
            if let Some(snapshot) = entry.session.snapshot_after_step()? {
                return Ok(Some((entry.kind.to_string(), snapshot)));
            }
        }
        Ok(None)
    }

    /// Collects and clears any per-step metadata produced by open sessions.
    pub fn take_step_metadata(&mut self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        for entry in &mut self.entries {
            if let Some(meta) = entry.session.take_step_metadata() {
                out.insert(entry.kind.to_string(), meta);
            }
        }
        out
    }

    /// Captures an input-masked PNG and surface dimensions from the first open
    /// session that exposes a visual surface.
    pub fn collect_masked_viewport_capture(
        &mut self,
    ) -> Result<Option<(String, MaskedViewportCapture)>, String> {
        for entry in &mut self.entries {
            if let Some(capture) = entry.session.masked_viewport_capture()? {
                return Ok(Some((entry.kind.to_string(), capture)));
            }
        }
        Ok(None)
    }

    /// Synthesizes a stable target descriptor for `ref_id` from the open
    /// session of `session_kind` (or the first open session that answers).
    pub fn synthesize_ref(
        &mut self,
        session_kind: Option<&str>,
        ref_id: &str,
    ) -> Result<Option<(String, SynthesizedTarget)>, String> {
        for entry in &mut self.entries {
            if session_kind.is_some_and(|expected| entry.kind != expected) {
                continue;
            }
            if let Some(target) = entry.session.synthesize_ref(ref_id)? {
                return Ok(Some((entry.kind.to_string(), target)));
            }
        }
        Ok(None)
    }

    /// Reverse-queries `(x, y)` on the open session of `session_kind` (or the
    /// first open session that answers) to synthesize a stable target descriptor.
    pub fn synthesize_at_point(
        &mut self,
        session_kind: Option<&str>,
        x: f64,
        y: f64,
    ) -> Result<Option<(String, SynthesizedTarget)>, String> {
        for entry in &mut self.entries {
            if session_kind.is_some_and(|expected| entry.kind != expected) {
                continue;
            }
            if let Some(target) = entry.session.synthesize_at_point(x, y)? {
                return Ok(Some((entry.kind.to_string(), target)));
            }
        }
        Ok(None)
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        // The normal path calls `close_all` and reports what it finds. This is
        // the fallback for a panic or an early return, where there is no report
        // left to write; it must not leave a resource open.
        let _ = self.close_all();
    }
}

/// Borrows one entry as its concrete type.
fn downcast<'a, T>(entry: &'a mut SessionEntry, kind: &str) -> Result<&'a mut T, String>
where
    T: Session + 'static,
{
    // The trait object is named explicitly: a method call on the box would
    // reach the blanket `AsAny` impl of `Box<dyn Session>` itself and return
    // the box instead of the session inside it.
    let session: &mut dyn Session = &mut *entry.session;
    session.as_any_mut().downcast_mut::<T>().ok_or_else(|| {
        format!(
            "session `{kind}` is already open with a different type; one session kind holds one concrete type"
        )
    })
}

/// Checks an evidence file name: one safe segment that cannot shadow the
/// bundle's own files.
fn check_evidence_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(
            "evidence file name is empty; use letters, digits, `.`, `_` and `-`".to_string(),
        );
    }
    if matches!(name, "." | "..") {
        return Err(format!(
            "evidence file name `{name}` is a path segment, not a file name"
        ));
    }
    if let Some(character) = name.chars().find(|character| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-')
    }) {
        return Err(format!(
            "evidence file name `{name}` contains `{character}`; use only letters, digits, `.`, `_` and `-`"
        ));
    }
    if matches!(name, "case.yaml" | "report.json" | "meta.json") {
        return Err(format!(
            "evidence file name `{name}` is reserved by the failure bundle; choose another name"
        ));
    }
    Ok(())
}

/// Checks a session kind against the step name rule and as a path segment.
pub(crate) fn check_kind(kind: &str) -> Result<(), String> {
    if kind.is_empty() {
        return Err(
            "session kind is empty; use lowercase letters, digits, `.`, `_` and `-`".to_string(),
        );
    }
    if matches!(kind, "." | "..") {
        return Err(format!(
            "session kind `{kind}` is a path segment, not a name; use lowercase letters, digits, `.`, `_` and `-`"
        ));
    }
    if kind == "trace" {
        return Err(
            "session kind `trace` is reserved by the failure bundle; choose another kind"
                .to_string(),
        );
    }
    if let Some(character) = kind
        .chars()
        .find(|character| !is_allowed_name_character(*character))
    {
        return Err(format!(
            "session kind `{kind}` contains `{character}`; use only lowercase letters, digits, `.`, `_` and `-`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn entries(&self) -> Vec<String> {
            self.0.lock().expect("the test log is not poisoned").clone()
        }
    }

    #[derive(Debug)]
    struct Tracked {
        id: usize,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Session for Tracked {
        fn close(&mut self) -> Result<(), String> {
            self.log
                .lock()
                .expect("the test log is not poisoned")
                .push(format!("close {}", self.id));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Other;

    impl Session for Other {
        fn close(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Failing {
        message: String,
    }

    impl Session for Failing {
        fn close(&mut self) -> Result<(), String> {
            Err(self.message.clone())
        }
    }

    fn tracked(log: &Log, id: usize) -> Tracked {
        Tracked {
            id,
            log: Arc::clone(&log.0),
        }
    }

    #[test]
    fn one_kind_returns_one_instance_and_creates_it_once() {
        let mut sessions = Sessions::new();
        let log = Log::default();
        let mut created = 0;

        let first = sessions
            .get_or_create("test.session", || {
                created += 1;
                Ok(tracked(&log, 7))
            })
            .expect("the session must be created");
        assert_eq!(first.id, 7);

        let second = sessions
            .get_or_create("test.session", || {
                created += 1;
                Ok(tracked(&log, 8))
            })
            .expect("the existing session must be returned");
        assert_eq!(second.id, 7, "the second call must not replace the session");
        assert_eq!(created, 1, "the factory must run only on the first call");

        let found = sessions
            .get::<Tracked>("test.session")
            .expect("the lookup must succeed")
            .expect("the session must exist");
        assert_eq!(found.id, 7);

        assert!(
            sessions
                .get::<Tracked>("test.other")
                .expect("a missing kind is not a type error")
                .is_none()
        );
    }

    #[test]
    fn a_kind_that_is_already_open_with_another_type_is_an_error() {
        let mut sessions = Sessions::new();
        let log = Log::default();
        sessions
            .get_or_create("test.session", || Ok(tracked(&log, 1)))
            .expect("the session must be created");

        let error = sessions
            .get_or_create::<Other>("test.session", || Ok(Other))
            .expect_err("another type must be refused");
        assert!(error.contains("test.session"), "got: {error}");
        assert!(error.contains("different type"), "got: {error}");

        let error = sessions
            .get::<Other>("test.session")
            .expect_err("the lookup must report the type mismatch");
        assert!(error.contains("test.session"), "got: {error}");
    }

    #[test]
    fn the_factory_error_is_reported_with_the_session_kind() {
        let mut sessions = Sessions::new();

        let error = sessions
            .get_or_create::<Tracked>("test.session", || Err("no resource".to_string()))
            .expect_err("the factory failure must be reported");

        assert!(error.contains("test.session"), "got: {error}");
        assert!(error.contains("no resource"), "got: {error}");
        assert!(
            sessions
                .get::<Tracked>("test.session")
                .expect("the lookup must succeed")
                .is_none(),
            "a failed creation must not leave a slot behind"
        );
    }

    #[test]
    fn a_session_kind_follows_the_step_name_rule() {
        let mut sessions = Sessions::new();
        for kind in ["", "Test.session", "test/session", "test session"] {
            let error = sessions
                .get_or_create::<Other>(kind, || Ok(Other))
                .expect_err("the kind must be refused");
            assert!(
                error.contains("lowercase letters"),
                "the message must state the rule for `{kind}`, got: {error}"
            );
            let error = sessions
                .get::<Other>(kind)
                .expect_err("the lookup must refuse the kind");
            assert!(
                error.contains("lowercase letters"),
                "the message must state the rule for `{kind}`, got: {error}"
            );
        }
    }

    #[test]
    fn evidence_names_are_one_safe_segment_and_cannot_shadow_the_bundle() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "../meta.json",
            "space name",
            "café",
        ] {
            assert!(
                Evidence::text(name, "text").is_err(),
                "`{name}` must be refused as an evidence name"
            );
            assert!(
                Evidence::bytes(name, vec![0]).is_err(),
                "`{name}` must be refused as an evidence name"
            );
        }
        for name in ["case.yaml", "report.json", "meta.json"] {
            let error = Evidence::text(name, "text").expect_err("a reserved name must be refused");
            assert!(error.contains("reserved"), "got: {error}");
        }
        assert!(Evidence::text("page.txt", "text").is_ok());
        assert!(Evidence::bytes("PAGE.TXT", vec![0]).is_ok());
    }

    #[test]
    fn a_session_that_refuses_or_returns_nothing_is_handled() {
        struct Nothing;
        impl Session for Nothing {
            fn close(&mut self) -> Result<(), String> {
                Ok(())
            }
        }

        struct Refusing;
        impl Session for Refusing {
            fn close(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
                Err("the page is gone".to_string())
            }
        }

        let mut sessions = Sessions::new();
        sessions
            .get_or_create("test.nothing", || Ok(Nothing))
            .expect("the empty session must be created");
        assert!(
            sessions.collect_evidence().is_empty(),
            "a session with no evidence must not add a group"
        );

        sessions
            .get_or_create("test.refusing", || Ok(Refusing))
            .expect("the refusing session must be created");
        let groups = sessions.collect_evidence();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, "test.refusing");
        assert!(groups[0].files.is_empty());
        assert!(
            groups[0]
                .error
                .as_deref()
                .is_some_and(|message| message.contains("page is gone")),
            "got: {groups:?}"
        );
    }

    #[test]
    fn collecting_evidence_groups_text_and_binary_files_by_session_kind() {
        struct EvidenceSession;
        impl Session for EvidenceSession {
            fn close(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
                Ok(vec![
                    Evidence::text("notes.txt", "the page said no")
                        .expect("the text name is valid"),
                    Evidence::bytes("screen.bin", vec![0x89, b'P', b'N', b'G'])
                        .expect("the binary name is valid"),
                ])
            }
        }

        let mut sessions = Sessions::new();
        sessions.set_base_directory("cases/shop");
        assert_eq!(
            sessions.base_directory().map(Path::to_path_buf),
            Some(PathBuf::from("cases/shop"))
        );
        sessions
            .get_or_create("test.evidence", || Ok(EvidenceSession))
            .expect("the evidence session must be created");

        let groups = sessions.collect_evidence();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, "test.evidence");
        assert!(groups[0].error.is_none());
        assert_eq!(groups[0].files[0].name(), "notes.txt");
        assert_eq!(
            groups[0].files[0].content(),
            &EvidenceContent::Text("the page said no".to_string())
        );
        assert_eq!(groups[0].files[1].name(), "screen.bin");
        assert_eq!(
            groups[0].files[1].content(),
            &EvidenceContent::Bytes(vec![0x89, b'P', b'N', b'G'])
        );
    }

    #[test]
    fn a_session_kind_cannot_be_a_path_segment() {
        let mut sessions = Sessions::new();
        for kind in [".", ".."] {
            let error = sessions
                .get_or_create::<Other>(kind, || Ok(Other))
                .expect_err("a path segment must not be a session kind");
            assert!(error.contains("path segment"), "got: {error}");
        }
    }

    #[test]
    fn every_session_is_closed_once_in_reverse_creation_order() {
        let mut sessions = Sessions::new();
        let log = Log::default();
        for (id, kind) in ["test.session0", "test.session1", "test.session2"]
            .into_iter()
            .enumerate()
        {
            sessions
                .get_or_create(kind, || Ok(tracked(&log, id)))
                .expect("the session must be created");
        }

        let errors = sessions.close_all();
        assert!(errors.is_empty(), "got: {errors:?}");
        assert_eq!(log.entries(), vec!["close 2", "close 1", "close 0"]);

        let errors = sessions.close_all();
        assert!(errors.is_empty(), "a closed session must not close again");
        assert_eq!(
            log.entries().len(),
            3,
            "every session must close exactly once"
        );
    }

    #[test]
    fn dropping_sessions_closes_what_was_not_closed_yet() {
        let log = Log::default();
        {
            let mut sessions = Sessions::new();
            sessions
                .get_or_create("test.session", || Ok(tracked(&log, 3)))
                .expect("the session must be created");
        }

        assert_eq!(log.entries(), vec!["close 3"]);
    }

    #[test]
    fn a_close_failure_names_the_session_kind_and_keeps_the_message() {
        let mut sessions = Sessions::new();
        sessions
            .get_or_create("test.broken", || {
                Ok(Failing {
                    message: "the connection was already gone".to_string(),
                })
            })
            .expect("the session must be created");

        let errors = sessions.close_all();

        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("test.broken"), "got: {errors:?}");
        assert!(
            errors[0].contains("the connection was already gone"),
            "got: {errors:?}"
        );
        assert!(
            sessions.close_all().is_empty(),
            "a failed close must still remove the session"
        );
    }

    #[test]
    fn a_close_failure_does_not_stop_the_other_sessions() {
        let mut sessions = Sessions::new();
        let log = Log::default();
        sessions
            .get_or_create("test.first", || Ok(tracked(&log, 1)))
            .expect("the first session must be created");
        sessions
            .get_or_create("test.broken", || {
                Ok(Failing {
                    message: "the connection was already gone".to_string(),
                })
            })
            .expect("the broken session must be created");
        sessions
            .get_or_create("test.last", || Ok(tracked(&log, 2)))
            .expect("the last session must be created");

        let errors = sessions.close_all();

        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("test.broken"), "got: {errors:?}");
        assert_eq!(
            log.entries(),
            vec!["close 2", "close 1"],
            "the later session must close first and the failure must not stop the earlier one"
        );
    }

    #[test]
    fn detach_all_persists_detachable_and_closes_non_detachable() {
        #[derive(Debug)]
        struct Detachable {
            detached: bool,
            log: Arc<Mutex<Vec<String>>>,
            meta: Option<Value>,
        }
        impl Session for Detachable {
            fn close(&mut self) -> Result<(), String> {
                if !self.detached {
                    self.log
                        .lock()
                        .expect("log lock")
                        .push("close detachable".to_string());
                }
                Ok(())
            }
            fn detach(&mut self) -> Result<Option<Value>, String> {
                self.detached = true;
                Ok(Some(serde_json::json!({"token": 42})))
            }
            fn snapshot(
                &mut self,
                scope: Option<&str>,
                _max_depth: Option<usize>,
                _max_nodes: Option<usize>,
            ) -> Result<Option<Value>, String> {
                Ok(Some(serde_json::json!({ "scope": scope })))
            }
            fn take_step_metadata(&mut self) -> Option<Value> {
                self.meta.take()
            }
        }

        let log = Log::default();
        let mut sessions = Sessions::new();
        sessions
            .get_or_create("test.plain", || Ok(tracked(&log, 10)))
            .expect("plain must be created");
        sessions
            .get_or_create("test.detach", || {
                Ok(Detachable {
                    detached: false,
                    log: Arc::clone(&log.0),
                    meta: Some(serde_json::json!({ "synthesized": true })),
                })
            })
            .expect("detachable must be created");

        let snap = sessions
            .collect_snapshot(Some("e1"), None, None)
            .expect("snapshot must succeed")
            .expect("snapshot must be present");
        assert_eq!(snap.0, "test.detach");
        assert_eq!(snap.1["scope"], "e1");

        let meta = sessions.take_step_metadata();
        assert_eq!(
            meta.get("test.detach"),
            Some(&serde_json::json!({ "synthesized": true }))
        );
        assert!(sessions.take_step_metadata().is_empty());

        let detached = sessions.detach_all();
        assert!(detached.errors.is_empty(), "got: {:?}", detached.errors);
        assert_eq!(
            detached.handles.get("test.detach"),
            Some(&serde_json::json!({"token": 42}))
        );
        assert!(
            !detached.handles.contains_key("test.plain"),
            "non-detachable session must not return a handle"
        );
        assert_eq!(log.entries(), vec!["close 10"]);
    }
}
