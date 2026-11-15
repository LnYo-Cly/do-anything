//! The `browser.network` observer: the requests a page finished while a step ran.
//!
//! Chrome reports a request in four events, all addressed by `requestId`:
//! `Network.requestWillBeSent`, `Network.responseReceived`,
//! `Network.loadingFinished` and `Network.loadingFailed`. The page session
//! keeps one table that merges them, so a case sees one record per request with
//! its method, URL, status, resource type and failure reason.
//!
//! A request belongs to the step in which it finished, not the step that
//! started it: an asynchronous response often arrives after the step that
//! clicked ended. The observer collects after each step, so that is where it
//! reads the socket and returns the records that are complete by then.
//!
//! Only the page this case attached to is observed; another tab's events are
//! left alone. Request headers and response bodies are never read, so cookies
//! and tokens cannot enter an observation.

use std::collections::VecDeque;

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Value, json};

use super::cdp::Connection;
use super::{BrowserSession, SESSION_KIND};
use crate::core::observer::{Observer, ObserverDefinition, ObserverError};
use crate::core::registry::Registry;
use crate::core::session::Sessions;

/// Name of the observer this module registers.
pub const OBSERVER_NAME: &str = "browser.network";

/// Most unfinished requests one page tracks at a time.
///
/// A page that starts requests faster than they finish would otherwise grow the
/// table without limit. The oldest unfinished request is discarded and counted
/// when the table is full.
pub const MAX_IN_FLIGHT: usize = 1_000;

/// Most finished requests one page keeps for the next collection.
///
/// The observer normally drains this list after every step. The bound covers a
/// case that declares an observation but stops before the next collection, and
/// a case that never declares the observer at all.
pub const MAX_COMPLETED: usize = 1_000;

/// Registers the observer of this module.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    let mut args_schema = serde_json::to_value(schema_for!(NetworkArgs))
        .map_err(|error| format!("could not build the schema of `{OBSERVER_NAME}`: {error}"))?;
    // `schemars` leaves out an empty `properties`, but an observer's arguments
    // are always an object with named properties; `da schema` and its
    // regression test expect the member even when there are none.
    args_schema["properties"] = json!({});
    registry.register_observer(ObserverDefinition {
        name: OBSERVER_NAME,
        description: "Reports the requests the page finished while the step ran.",
        args_schema,
        validate,
        create,
    })
}

/// Arguments of `browser.network`: none.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NetworkArgs {}

fn validate(args: &Value) -> Result<(), String> {
    serde_json::from_value::<NetworkArgs>(args.clone())
        .map(|_| ())
        .map_err(|error| format!("invalid arguments: {error}"))
}

fn create(args: &Value) -> Result<Box<dyn Observer>, ObserverError> {
    validate(args).map_err(ObserverError::InvalidArguments)?;
    Ok(Box::new(NetworkObserver {
        dropped_baseline: 0,
    }))
}

/// One case's observation of the requests of its page.
struct NetworkObserver {
    /// `NetworkState::dropped` when the current step started.
    dropped_baseline: u64,
}

impl Observer for NetworkObserver {
    fn start(&mut self, sessions: &mut Sessions) -> Result<(), ObserverError> {
        // There is no page before the first `browser.open`, and that step opens
        // it; nothing to record yet.
        self.dropped_baseline = match session(sessions)? {
            Some(session) => session.network_dropped(),
            None => 0,
        };
        Ok(())
    }

    fn collect(&mut self, sessions: &mut Sessions) -> Result<Value, ObserverError> {
        let Some(session) = session(sessions)? else {
            return Ok(empty());
        };
        session.poll_network().map_err(ObserverError::Environment)?;
        Ok(session.take_network_step(self.dropped_baseline))
    }
}

/// The case's browser session, when it has one yet.
fn session(sessions: &mut Sessions) -> Result<Option<&mut BrowserSession>, ObserverError> {
    sessions
        .get::<BrowserSession>(SESSION_KIND)
        .map_err(ObserverError::Environment)
}

/// The value reported when no page exists, or nothing finished.
fn empty() -> Value {
    json!({ "requests": [], "pending": 0, "dropped": 0 })
}

/// One request seen in this page, merged from the events that name its id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedRequest {
    request_id: String,
    method: String,
    url: String,
    resource_type: Option<String>,
    status: Option<i64>,
    failed: Option<String>,
}

impl TrackedRequest {
    /// The record a step result reports, without the protocol's own id.
    fn record(&self) -> Value {
        json!({
            "method": self.method,
            "url": self.url,
            "status": self.status,
            "type": self.resource_type,
            "failed": self.failed,
        })
    }
}

/// The requests of one attached page, bounded and merged by `requestId`.
pub(super) struct NetworkState {
    /// The page session every event must carry to belong to this case.
    page: String,
    /// Requests seen but not finished, in the order they started.
    in_flight: VecDeque<TrackedRequest>,
    /// Requests finished since the last collection, in completion order.
    completed: VecDeque<TrackedRequest>,
    /// Messages and requests this page discarded to stay within its bounds.
    dropped: u64,
}

impl NetworkState {
    /// Creates the table of one attached page.
    pub(super) fn new(page: &str) -> Self {
        Self {
            page: page.to_string(),
            in_flight: VecDeque::new(),
            completed: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Merges every queued network event, then puts the other messages back.
    ///
    /// Non-network messages keep their original handling: a later
    /// `wait_for_event` still finds the page events it waits for.
    pub(super) fn absorb(&mut self, connection: &mut Connection) {
        self.dropped += connection.take_dropped();
        let messages = connection.take_messages();
        let mut kept = Vec::with_capacity(messages.len());
        for message in messages {
            if !self.handle(&message) {
                kept.push(message);
            }
        }
        connection.restore_messages(kept);
    }

    /// Reads what has already arrived and merges it.
    pub(super) fn poll(&mut self, connection: &mut Connection) -> Result<(), String> {
        connection
            .poll_messages()
            .map_err(|error| format!("could not read network events: {error}"))?;
        self.absorb(connection);
        Ok(())
    }

    /// How many messages and requests this page discarded so far.
    pub(super) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Takes the requests finished since the last call and counts the rest.
    pub(super) fn take_step(&mut self, dropped_baseline: u64) -> Value {
        let requests: Vec<Value> = self
            .completed
            .drain(..)
            .map(|request| request.record())
            .collect();
        let pending = self.in_flight.len();
        json!({
            "requests": requests,
            "pending": pending,
            "dropped": self.dropped.saturating_sub(dropped_baseline),
        })
    }

    /// Merges one CDP message and reports whether it belonged to this page.
    ///
    /// Every `Network.*` event of this page is consumed, including the
    /// `*ExtraInfo` events, which carry the headers this observer must not keep.
    fn handle(&mut self, message: &Value) -> bool {
        if message.get("sessionId").and_then(Value::as_str) != Some(self.page.as_str()) {
            return false;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return false;
        };
        if !method.starts_with("Network.") {
            return false;
        }
        let params = message.get("params").unwrap_or(&Value::Null);
        let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
            return true;
        };
        match method {
            "Network.requestWillBeSent" => self.started(request_id, params),
            "Network.responseReceived" => self.responded(request_id, params),
            "Network.loadingFinished" => self.finished(request_id),
            "Network.loadingFailed" => self.failed(request_id, params),
            _ => {}
        }
        true
    }

    /// Starts one request, or updates the last hop of a redirect.
    ///
    /// A redirect reuses the request id and sends `requestWillBeSent` again. The
    /// record keeps the last URL the page requested, which is the one a case
    /// names.
    fn started(&mut self, request_id: &str, params: &Value) {
        let Some(request) = params.get("request") else {
            return;
        };
        let Some(url) = request.get("url").and_then(Value::as_str) else {
            return;
        };
        if ignored_url(url) {
            self.remove(our_index(&self.in_flight, request_id));
            return;
        }
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET");
        let resource_type = params
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(index) = our_index(&self.in_flight, request_id) {
            if let Some(pending) = self.in_flight.get_mut(index) {
                pending.method = method.to_string();
                pending.url = url.to_string();
                pending.resource_type = resource_type;
                pending.status = None;
                pending.failed = None;
            }
            return;
        }
        if self.in_flight.len() == MAX_IN_FLIGHT {
            self.in_flight.pop_front();
            self.dropped += 1;
        }
        self.in_flight.push_back(TrackedRequest {
            request_id: request_id.to_string(),
            method: method.to_string(),
            url: url.to_string(),
            resource_type,
            status: None,
            failed: None,
        });
    }

    /// Records the response status of a request that is still in flight.
    fn responded(&mut self, request_id: &str, params: &Value) {
        let Some(response) = params.get("response") else {
            return;
        };
        let status = response.get("status").and_then(Value::as_i64);
        let url = response.get("url").and_then(Value::as_str);
        let resource_type = params
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(index) = our_index(&self.in_flight, request_id)
            && let Some(pending) = self.in_flight.get_mut(index)
        {
            pending.status = status;
            if let Some(url) = url {
                pending.url = url.to_string();
            }
            if resource_type.is_some() {
                pending.resource_type = resource_type;
            }
        }
    }

    /// Moves a request that finished to the completed list.
    fn finished(&mut self, request_id: &str) {
        if let Some(index) = our_index(&self.in_flight, request_id)
            && let Some(request) = self.in_flight.remove(index)
        {
            self.complete(request);
        }
    }

    /// Moves a failed request to the completed list with its reason.
    fn failed(&mut self, request_id: &str, params: &Value) {
        if let Some(index) = our_index(&self.in_flight, request_id)
            && let Some(mut request) = self.in_flight.remove(index)
        {
            request.status = None;
            request.failed = Some(
                params
                    .get("errorText")
                    .and_then(Value::as_str)
                    .unwrap_or("the request failed")
                    .to_string(),
            );
            self.complete(request);
        }
    }

    /// Appends one finished request, dropping the oldest when the list is full.
    fn complete(&mut self, request: TrackedRequest) {
        if self.completed.len() == MAX_COMPLETED {
            self.completed.pop_front();
            self.dropped += 1;
        }
        self.completed.push_back(request);
    }

    /// Removes a request that is no longer wanted.
    fn remove(&mut self, index: Option<usize>) {
        if let Some(index) = index {
            self.in_flight.remove(index);
        }
    }
}

/// The index of a request id in one table.
fn our_index(requests: &VecDeque<TrackedRequest>, request_id: &str) -> Option<usize> {
    requests
        .iter()
        .position(|request| request.request_id == request_id)
}

/// Whether a URL is generated by the page rather than fetched from a server.
fn ignored_url(url: &str) -> bool {
    url.starts_with("data:") || url.starts_with("blob:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(session: &str, method: &str, params: Value) -> Value {
        json!({ "sessionId": session, "method": method, "params": params })
    }

    fn started(request_id: &str, method: &str, url: &str, kind: &str) -> Value {
        event(
            "page-1",
            "Network.requestWillBeSent",
            json!({
                "requestId": request_id,
                "type": kind,
                "request": { "method": method, "url": url },
            }),
        )
    }

    #[test]
    fn four_events_merge_into_two_records_and_another_page_is_ignored() {
        let mut network = NetworkState::new("page-1");
        network.handle(&started(
            "1",
            "POST",
            "http://127.0.0.1:1/api/login",
            "Fetch",
        ));
        network.handle(&started("2", "GET", "http://127.0.0.1:1/missing", "Fetch"));
        network.handle(&event(
            "page-1",
            "Network.responseReceived",
            json!({
                "requestId": "1",
                "type": "Fetch",
                "response": { "url": "http://127.0.0.1:1/api/login", "status": 200 },
            }),
        ));
        network.handle(&event(
            "page-1",
            "Network.loadingFinished",
            json!({ "requestId": "1" }),
        ));
        network.handle(&event(
            "page-1",
            "Network.loadingFailed",
            json!({ "requestId": "2", "errorText": "net::ERR_CONNECTION_REFUSED" }),
        ));
        // Another tab's request must not enter this case's table.
        network.handle(&event(
            "page-2",
            "Network.requestWillBeSent",
            json!({
                "requestId": "3",
                "type": "Fetch",
                "request": { "method": "GET", "url": "http://127.0.0.1:1/other" },
            }),
        ));

        let value = network.take_step(0);

        assert_eq!(
            value["requests"],
            json!([
                {
                    "method": "POST",
                    "url": "http://127.0.0.1:1/api/login",
                    "status": 200,
                    "type": "Fetch",
                    "failed": null,
                },
                {
                    "method": "GET",
                    "url": "http://127.0.0.1:1/missing",
                    "status": null,
                    "type": "Fetch",
                    "failed": "net::ERR_CONNECTION_REFUSED",
                }
            ]),
            "one record per request, in completion order: {value}"
        );
        assert_eq!(value["pending"], 0);
        assert_eq!(value["dropped"], 0);
        assert_eq!(
            network.take_step(0)["requests"],
            json!([]),
            "a collected request must not be reported twice"
        );
    }

    #[test]
    fn data_and_blob_urls_are_not_reported() {
        let mut network = NetworkState::new("page-1");
        network.handle(&started("1", "GET", "data:text/plain,hello", "Image"));
        network.handle(&started("2", "GET", "blob:http://127.0.0.1/abc", "Image"));
        network.handle(&event(
            "page-1",
            "Network.loadingFinished",
            json!({ "requestId": "1" }),
        ));

        let value = network.take_step(0);

        assert_eq!(value["requests"], json!([]), "{value}");
        assert_eq!(value["pending"], 0, "{value}");
    }

    #[test]
    fn the_unfinished_table_stops_at_its_bound_and_counts_what_it_drops() {
        let mut network = NetworkState::new("page-1");
        for index in 0..MAX_IN_FLIGHT + 2 {
            network.handle(&started(
                &index.to_string(),
                "GET",
                "http://127.0.0.1:1/",
                "Fetch",
            ));
        }

        let value = network.take_step(0);
        assert_eq!(
            value["pending"], MAX_IN_FLIGHT,
            "unfinished requests must stay bounded"
        );
        assert_eq!(
            value["dropped"], 2,
            "the oldest unfinished requests must be counted as dropped"
        );
    }

    #[test]
    fn the_completed_table_stops_at_its_bound_and_counts_what_it_drops() {
        let mut network = NetworkState::new("page-1");
        for index in 0..MAX_COMPLETED + 2 {
            let request_id = index.to_string();
            network.handle(&started(&request_id, "GET", "http://127.0.0.1:1/", "Fetch"));
            network.handle(&event(
                "page-1",
                "Network.loadingFinished",
                json!({ "requestId": request_id }),
            ));
        }

        let value = network.take_step(0);
        assert_eq!(
            value["requests"].as_array().map(Vec::len),
            Some(MAX_COMPLETED),
            "completed requests must stay bounded"
        );
        assert_eq!(
            value["dropped"], 2,
            "the oldest completed requests must be counted as dropped"
        );
    }

    #[test]
    fn the_step_count_starts_where_start_recorded_it() {
        let mut network = NetworkState::new("page-1");
        for index in 0..MAX_IN_FLIGHT + 1 {
            network.handle(&started(
                &index.to_string(),
                "GET",
                "http://127.0.0.1:1/",
                "Fetch",
            ));
        }

        assert_eq!(
            network.take_step(1)["dropped"],
            0,
            "a drop before this step must not be reported as this step's"
        );
        assert_eq!(
            network.take_step(0)["dropped"],
            1,
            "the counter itself is cumulative, not reset by a collection"
        );
    }

    #[test]
    fn the_observer_takes_no_arguments_and_reports_nothing_without_a_page() {
        let observer = create(&json!({})).expect("empty arguments must be accepted");
        let error = match create(&json!({ "filter": "x" })) {
            Ok(_) => panic!("an argument must be refused"),
            Err(error) => error,
        };
        let ObserverError::InvalidArguments(message) = error else {
            panic!("expected an argument error, got: {error:?}");
        };
        assert!(message.contains("filter"), "got: {message}");

        let mut observer = observer;
        observer
            .start(&mut Sessions::new())
            .expect("start without a session must succeed");
        let value = observer
            .collect(&mut Sessions::new())
            .expect("collect without a session must succeed");
        assert_eq!(value, empty());
    }

    #[test]
    fn the_registered_schema_refuses_an_extra_field() {
        let mut registry = Registry::new();
        register(&mut registry).expect("the network observer must register");
        let definition = registry
            .observer(OBSERVER_NAME)
            .expect("the network observer must be registered");
        let args_schema = &definition.args_schema;
        assert_eq!(args_schema["type"], "object");
        assert_eq!(
            args_schema["properties"],
            json!({}),
            "`browser.network` has no arguments: {args_schema}"
        );
        assert_eq!(
            args_schema["additionalProperties"], false,
            "`browser.network` takes no arguments: {args_schema}"
        );
        assert!(jsonschema_valid(args_schema, &json!({})));
        assert!(!jsonschema_valid(args_schema, &json!({ "x": 1 })));
    }

    /// Whether a JSON value satisfies a schema, without a test dependency.
    ///
    /// The project's integration tests use the `jsonschema` crate, which a unit
    /// test cannot see; the two fields above are the whole contract, so they are
    /// checked directly instead.
    fn jsonschema_valid(schema: &Value, value: &Value) -> bool {
        if schema["type"] == "object" {
            if !value.is_object() {
                return false;
            }
            if schema["additionalProperties"] == false {
                let properties = schema["properties"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                if value
                    .as_object()
                    .is_some_and(|entries| entries.keys().any(|key| !properties.contains_key(key)))
                {
                    return false;
                }
            }
        }
        true
    }
}
