//! Short, user-facing error reports for the connection pill and room sync
//! surfaces, plus the diagnostic text "Click to copy details" copies.
//!
//! A report has three parts: a short `label` the surface shows ("Disconnected",
//! "Room not found"), a one-sentence `summary` its tooltip explains, and the
//! raw `detail` (the original error text) that only the copied diagnostics
//! carry. Logs keep the error text they always had; an error from a socket
//! the page already replaced logs at debug level, without its payload.
//!
//! freenet-stdlib's browser `WebApi` reports transport failures as
//! `Error::ConnectionError(json)`, whose `source` / `origin` field says where
//! the failure came from. Classification reads those fields, never the prose.
//! The native client has a different `ConnectionError` payload (a tungstenite
//! error), so the JSON path is WASM-only and natives fall back to the error's
//! Display.

// Most constructors serve the WASM connection paths only.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use freenet_stdlib::client_api;

pub const LABEL_DISCONNECTED: &str = "Disconnected";
pub const LABEL_INVALID_OPERATION: &str = "Invalid operation";
pub const LABEL_UNEXPECTED_MESSAGE: &str = "Unexpected message";
pub const LABEL_REFRESHING: &str = "Refreshing...";
pub const LABEL_ROOM_NOT_FOUND: &str = "Room not found";
pub const LABEL_ROOM_FULL: &str = "Room is full";

/// The most raw text (detail or stack) the copied diagnostics carry, per
/// field. It bounds size; it is not redaction.
const MAX_RAW_CHARS: usize = 8_000;

/// Reads a `serde_json::Value` into a [`ConnectionPayload`]. A macro, with
/// method calls only, because river-ui does not depend on `serde_json`: the
/// value's type is inferred from freenet-stdlib's `ConnectionError`, and the
/// tests (where `serde_json` is a dev-dependency) run this same code.
#[allow(unused_macros)]
macro_rules! connection_payload {
    ($value:expr) => {{
        let value = $value;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let error = value.get("error").and_then(|e| {
            if let Some(s) = e.as_str() {
                Some(s.trim().to_string()).filter(|s| !s.is_empty())
            } else if e.is_null() {
                None
            } else if let Some(m) = e
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|m| !m.is_empty())
            {
                Some(m.to_string())
            } else {
                Some(e.to_string())
            }
        });
        let mut redacted = value.clone();
        if let Some(request) = redacted.get_mut("request") {
            let kind = request_kind(request.as_str().unwrap_or(""));
            *request = format!("{kind} (contents omitted)").into();
        }
        ConnectionPayload {
            source: field("source"),
            origin: field("origin"),
            error,
            redacted: redacted.to_string(),
        }
    }};
}

/// One failure, as the UI shows and copies it.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorReport {
    /// The short label the pill or room surface shows.
    pub label: &'static str,
    /// One sentence for the tooltip.
    pub summary: String,
    /// The original error text, for the copied diagnostics only.
    pub detail: String,
    /// When the report was made (ISO 8601), if the platform has a clock.
    pub time: Option<String>,
    /// The JS stack where the report was made, if available.
    pub stack: Option<String>,
}

impl ErrorReport {
    /// A report stamped with the current time and JS stack (WASM only).
    pub fn new(label: &'static str, summary: impl Into<String>, detail: impl Into<String>) -> Self {
        let (time, stack) = capture_context();
        Self {
            label,
            summary: summary.into(),
            detail: detail.into(),
            time,
            stack,
        }
    }

    /// Whether the surface should read as work in progress (warning dot)
    /// rather than a failure.
    pub fn is_progress(&self) -> bool {
        self.label == LABEL_REFRESHING
    }

    /// The text "Click to copy details" puts on the clipboard.
    pub fn diagnostics(&self) -> String {
        format_diagnostics(self, crate::util::BUILD_TIMESTAMP_ISO)
    }

    /// A report for a transport error the `WebApi` handed River.
    pub fn from_client_error(error: &client_api::Error) -> Self {
        let ClientErrorParts {
            label,
            summary,
            detail,
        } = analyse_client_error(error);
        Self::new(label, summary, detail)
    }

    /// The same, for an error whose operation the summary should name
    /// ("Couldn't fetch this room: Connection closed").
    pub fn from_client_error_during(operation: &str, error: &client_api::Error) -> Self {
        let mut report = Self::from_client_error(error);
        report.summary = format!("{operation}: {}", report.summary);
        report.detail = format!("{operation}: {}", report.detail);
        report
    }

    /// A report for a room that could not be subscribed to after it was
    /// published. The copied detail of a send failure is the redacted one,
    /// never its raw text.
    pub fn from_synchronizer_error(error: &super::error::SynchronizerError) -> Self {
        use super::error::SynchronizerError as E;
        match error {
            E::SubscribeSendFailed {
                summary, detail, ..
            } => Self::new(
                LABEL_DISCONNECTED,
                format!("Couldn't subscribe to this room: {summary}"),
                detail.clone(),
            ),
            E::ApiNotInitialized => Self::new(
                LABEL_DISCONNECTED,
                "Couldn't subscribe to this room: not connected to your Freenet node",
                error.to_string(),
            ),
            other => Self::from_text(LABEL_DISCONNECTED, &other.to_string()),
        }
    }

    /// The node answered a request with an error that says the operation is
    /// not supported (P13).
    pub fn unsupported_operation(detail: impl Into<String>) -> Self {
        Self::new(
            LABEL_INVALID_OPERATION,
            "Your Freenet node does not support an operation River used. Check that River and the node are compatible.",
            detail,
        )
    }

    /// The socket did not open in time (P4).
    pub fn connect_timeout(detail: impl Into<String>) -> Self {
        Self::new(
            LABEL_DISCONNECTED,
            "WebSocket connection failed or timed out",
            detail,
        )
    }

    /// The node no longer accepts this page's auth token; a reload is
    /// scheduled (P14).
    pub fn auth_expired(detail: impl Into<String>) -> Self {
        Self::new(
            LABEL_REFRESHING,
            "Authentication expired. Refreshing the page.",
            detail,
        )
    }

    /// The auth token failed again right after a reload (P15).
    pub fn auth_reload_loop(detail: impl Into<String>) -> Self {
        Self::new(
            LABEL_DISCONNECTED,
            "Connection lost. Please refresh the page.",
            detail,
        )
    }

    /// A report from a free-form error text: its first sentence explains it,
    /// the whole text is the detail.
    pub fn from_text(label: &'static str, text: &str) -> Self {
        Self::new(label, first_sentence(text), text)
    }
}

/// A transport error as a report shows it, read from its payload once.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientErrorParts {
    pub label: &'static str,
    pub summary: String,
    /// See [`client_error_detail`].
    pub detail: String,
}

/// The label, one-sentence summary and copied detail of a transport error.
pub fn analyse_client_error(error: &client_api::Error) -> ClientErrorParts {
    #[cfg(target_family = "wasm")]
    if let client_api::Error::ConnectionError(value) = error {
        let payload = connection_payload!(value);
        let (label, summary) = classify_connection_payload(&payload);
        return ClientErrorParts {
            label,
            summary,
            detail: payload_detail(&payload),
        };
    }
    let (label, summary) = match error {
        client_api::Error::ConnectionClosed => (LABEL_DISCONNECTED, "Connection closed".into()),
        client_api::Error::ChannelClosed => (LABEL_DISCONNECTED, "Connection closed".into()),
        client_api::Error::Deserialization(e) => (
            LABEL_UNEXPECTED_MESSAGE,
            format!("The node's reply could not be decoded ({e})"),
        ),
        other => (LABEL_DISCONNECTED, first_sentence(&other.to_string())),
    };
    ClientErrorParts {
        label,
        summary,
        detail: format!("WebSocket error: {error}"),
    }
}

/// The raw text the copied diagnostics carry for a transport error: its
/// Display, with the request contents the browser `WebApi` embeds replaced by
/// the request's kind (a PUT carries the whole room state, a delegate request
/// the user's stored rooms).
pub fn client_error_detail(error: &client_api::Error) -> String {
    #[cfg(target_family = "wasm")]
    if let client_api::Error::ConnectionError(value) = error {
        return payload_detail(&connection_payload!(value));
    }
    format!("WebSocket error: {error}")
}

fn payload_detail(payload: &ConnectionPayload) -> String {
    format!("WebSocket error: request error: {}", payload.redacted)
}

/// The fields of the browser `WebApi`'s JSON error payload River reads.
#[derive(Debug)]
struct ConnectionPayload {
    source: String,
    origin: String,
    /// The `error` field as text: a string as is, an object's `message`, any
    /// other non-null value as compact JSON.
    error: Option<String>,
    /// The whole payload as JSON, its `request` replaced by the request's kind.
    redacted: String,
}

/// The label and summary for the browser `WebApi`'s error payload, by where
/// the payload says the failure came from.
fn classify_connection_payload(payload: &ConnectionPayload) -> (&'static str, String) {
    let text = payload.error.clone();
    match (payload.source.as_str(), payload.origin.as_str()) {
        ("close", _) => (LABEL_DISCONNECTED, "Connection closed".into()),
        (_, "send precondition check") => {
            let state = text
                .as_deref()
                .and_then(|t| t.strip_prefix("WebSocket is not open (state: "))
                .and_then(|s| s.strip_suffix(')'));
            let summary = match state {
                Some(state) => format!("WebSocket state: {state}"),
                None => "WebSocket is not open".into(),
            };
            (LABEL_DISCONNECTED, summary)
        }
        (_, "request sending") => (
            LABEL_DISCONNECTED,
            text.map(|t| format!("The browser refused to send the request: {t}"))
                .unwrap_or_else(|| "The browser refused to send the request".into()),
        ),
        (_, "chunk cap check") => (
            LABEL_INVALID_OPERATION,
            with_reason("The request is too large to send", text),
        ),
        (_, "request serialization") => (
            LABEL_INVALID_OPERATION,
            with_reason("The request could not be serialized", text),
        ),
        ("host response decoding", _) => (
            LABEL_UNEXPECTED_MESSAGE,
            "The node sent a message River can't read (not binary)".into(),
        ),
        ("host response deserialization", _) => (
            LABEL_UNEXPECTED_MESSAGE,
            with_reason("The node's reply could not be decoded", text),
        ),
        ("stream reassembly deserialization", _) | ("streaming reassembly", _) => (
            LABEL_UNEXPECTED_MESSAGE,
            with_reason(
                "A streamed reply from the node could not be reassembled",
                text,
            ),
        ),
        _ => (
            LABEL_DISCONNECTED,
            text.map(|t| first_sentence(&t))
                .unwrap_or_else(|| "Connection error".into()),
        ),
    }
}

fn with_reason(lead: &str, reason: Option<String>) -> String {
    match reason {
        Some(r) => format!("{lead} ({r})"),
        None => lead.to_string(),
    }
}

/// The leading variant names of a request's Debug, e.g.
/// `ContractOp(Put` for `ContractOp(Put { contract: …`.
fn request_kind(debug: &str) -> String {
    let end = debug
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '(' || c == ':'))
        .unwrap_or(debug.len());
    let kind: String = debug[..end]
        .trim_end_matches('(')
        .chars()
        .take(60)
        .collect();
    if kind.is_empty() {
        "request".into()
    } else {
        kind
    }
}

/// The text up to the end of its first sentence, capitalised, with no
/// trailing full stop: a summary is one short sentence.
fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|&(i, c)| c == '.' && text[i + 1..].starts_with(' '))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    capitalise_first(text[..end].trim_end_matches('.'))
}

fn capitalise_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `text` capped at [`MAX_RAW_CHARS`] characters, saying how many it dropped.
fn cap_raw(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_RAW_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_RAW_CHARS).collect();
    format!("{kept}… [{} characters omitted]", total - MAX_RAW_CHARS)
}

/// The copied diagnostics. Absent fields are left out; detail and stack are
/// capped by [`cap_raw`].
fn format_diagnostics(report: &ErrorReport, build: &str) -> String {
    let mut lines = vec![format!("River: {}", report.label)];
    if !report.summary.is_empty() {
        lines.push(report.summary.clone());
    }
    if !report.detail.is_empty() && report.detail != report.summary {
        lines.push(cap_raw(&report.detail));
    }
    if let Some(time) = &report.time {
        lines.push(format!("Time: {time}"));
    }
    if !build.is_empty() {
        lines.push(format!("Build: {build}"));
    }
    if let Some(stack) = report.stack.as_deref().filter(|s| !s.trim().is_empty()) {
        lines.push("Stack:".into());
        lines.push(cap_raw(stack.trim_end()));
    }
    lines.join("\n")
}

#[cfg(target_arch = "wasm32")]
fn capture_context() -> (Option<String>, Option<String>) {
    let time = String::from(js_sys::Date::new_0().to_iso_string());
    let stack = js_sys::Reflect::get(&js_sys::Error::new(""), &"stack".into())
        .ok()
        .and_then(|s| s.as_string());
    (Some(time), stack)
}

#[cfg(not(target_arch = "wasm32"))]
fn capture_context() -> (Option<String>, Option<String>) {
    (Some(chrono::Utc::now().to_rfc3339()), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(value: &serde_json::Value) -> (&'static str, String) {
        classify_connection_payload(&connection_payload!(value))
    }

    fn report(label: &'static str, summary: &str, detail: &str) -> ErrorReport {
        ErrorReport {
            label,
            summary: summary.into(),
            detail: detail.into(),
            time: Some("2026-11-05T01:02:03.000Z".into()),
            stack: Some("Error\n    at report (river.js:1:2)".into()),
        }
    }

    #[test]
    fn payloads_classify_by_source_and_origin() {
        let cases = [
            (json!({"error": "connection closed", "source": "close"}), LABEL_DISCONNECTED, "Connection closed"),
            (
                json!({"error": "WebSocket is not open (state: CLOSED)", "origin": "send precondition check", "request": "ContractOp(Get { key: 1 })"}),
                LABEL_DISCONNECTED,
                "WebSocket state: CLOSED",
            ),
            (
                json!({"error": "socket gone", "origin": "send precondition check"}),
                LABEL_DISCONNECTED,
                "WebSocket is not open",
            ),
            (
                json!({"error": {"name": "InvalidStateError"}, "origin": "request sending"}),
                LABEL_DISCONNECTED,
                "The browser refused to send the request: {\"name\":\"InvalidStateError\"}",
            ),
            (
                json!({"error": {"message": "Still in CONNECTING state."}, "origin": "request sending"}),
                LABEL_DISCONNECTED,
                "The browser refused to send the request: Still in CONNECTING state.",
            ),
            (json!({"error": null, "origin": "request sending"}), LABEL_DISCONNECTED, "The browser refused to send the request"),
            (
                json!({"error": "total_chunks 300 exceeds maximum 256", "origin": "chunk cap check"}),
                LABEL_INVALID_OPERATION,
                "The request is too large to send (total_chunks 300 exceeds maximum 256)",
            ),
            (
                json!({"error": "invalid type: map", "origin": "request serialization"}),
                LABEL_INVALID_OPERATION,
                "The request could not be serialized (invalid type: map)",
            ),
            (
                json!({"error": "unexpected non-binary websocket message: JsValue(\"hi\")", "source": "host response decoding"}),
                LABEL_UNEXPECTED_MESSAGE,
                "The node sent a message River can't read (not binary)",
            ),
            (
                json!({"error": "io error: unexpected end of file", "source": "host response deserialization"}),
                LABEL_UNEXPECTED_MESSAGE,
                "The node's reply could not be decoded (io error: unexpected end of file)",
            ),
            (
                json!({"error": "bad tag", "source": "stream reassembly deserialization"}),
                LABEL_UNEXPECTED_MESSAGE,
                "A streamed reply from the node could not be reassembled (bad tag)",
            ),
            (
                json!({"error": "duplicate chunk index 2 for stream 0", "source": "streaming reassembly"}),
                LABEL_UNEXPECTED_MESSAGE,
                "A streamed reply from the node could not be reassembled (duplicate chunk index 2 for stream 0)",
            ),
        ];
        for (payload, label, summary) in cases {
            assert_eq!(
                classify(&payload),
                (label, summary.to_string()),
                "{payload}"
            );
        }
    }

    #[test]
    fn unknown_payloads_fall_back_without_panicking() {
        for (payload, summary) in [
            (json!({}), "Connection error"),
            (json!("just a string"), "Connection error"),
            (json!({"error": ""}), "Connection error"),
            (json!({"error": 42, "source": "somewhere new"}), "42"),
            (
                json!({"error": "something new. With more", "source": "new"}),
                "Something new",
            ),
        ] {
            assert_eq!(
                classify(&payload),
                (LABEL_DISCONNECTED, summary.to_string()),
                "{payload}"
            );
        }
    }

    #[test]
    fn native_error_variants_classify() {
        let closed = ClientErrorParts {
            label: LABEL_DISCONNECTED,
            summary: "Connection closed".into(),
            detail: "WebSocket error: connection closed".into(),
        };
        assert_eq!(
            analyse_client_error(&client_api::Error::ConnectionClosed),
            closed
        );
        assert_eq!(
            analyse_client_error(&client_api::Error::ChannelClosed).summary,
            "Connection closed"
        );
        assert_eq!(
            client_error_detail(&client_api::Error::ConnectionClosed),
            closed.detail
        );
    }

    #[test]
    fn an_operation_prefixes_summary_and_detail() {
        let r = ErrorReport::from_client_error_during(
            "Couldn't fetch this room",
            &client_api::Error::ConnectionClosed,
        );
        assert_eq!(r.label, LABEL_DISCONNECTED);
        assert_eq!(r.summary, "Couldn't fetch this room: Connection closed");
        assert_eq!(
            r.detail,
            "Couldn't fetch this room: WebSocket error: connection closed"
        );
    }

    #[test]
    fn the_request_dump_is_replaced_by_its_kind() {
        let payload = json!({
            "error": "WebSocket is not open (state: CLOSED)",
            "origin": "send precondition check",
            "request": "ContractOp(Put { contract: [1, 2, 3], state: WrappedState([9, 9, 9]) })",
        });
        let read = connection_payload!(&payload);
        let redacted: serde_json::Value = serde_json::from_str(&read.redacted).unwrap();
        assert_eq!(redacted["request"], "ContractOp(Put (contents omitted)");
        assert_eq!(redacted["error"], payload["error"]);
        assert!(!read.redacted.contains("[9, 9, 9]"));
        assert_eq!(
            request_kind("DelegateOp(ApplicationMessages { key"),
            "DelegateOp(ApplicationMessages"
        );
        assert_eq!(request_kind("Disconnect { cause: None }"), "Disconnect");
        assert_eq!(request_kind(""), "request");
        // No request field: unchanged.
        let plain = json!({"error": "connection closed", "source": "close"});
        assert_eq!(connection_payload!(&plain).redacted, plain.to_string());
    }

    #[test]
    fn diagnostics_golden_example() {
        let r = report(
            LABEL_DISCONNECTED,
            "Connection closed",
            "WebSocket error: request error: {\"error\":\"connection closed\",\"source\":\"close\"}",
        );
        assert_eq!(
            format_diagnostics(&r, "2026-11-04T10:00:00Z"),
            "River: Disconnected\n\
             Connection closed\n\
             WebSocket error: request error: {\"error\":\"connection closed\",\"source\":\"close\"}\n\
             Time: 2026-11-05T01:02:03.000Z\n\
             Build: 2026-11-04T10:00:00Z\n\
             Stack:\n\
             Error\n    at report (river.js:1:2)"
        );
    }

    #[test]
    fn diagnostics_omit_absent_fields() {
        let mut r = report(LABEL_ROOM_NOT_FOUND, "Gone", "Gone");
        r.time = None;
        r.stack = None;
        assert_eq!(format_diagnostics(&r, ""), "River: Room not found\nGone");
    }

    #[test]
    fn raw_text_is_capped_with_the_omitted_length() {
        let long = "é".repeat(MAX_RAW_CHARS + 25);
        let capped = cap_raw(&long);
        assert!(
            capped.ends_with("… [25 characters omitted]"),
            "{}",
            &capped[capped.len() - 40..]
        );
        assert_eq!(capped.chars().filter(|&c| c == 'é').count(), MAX_RAW_CHARS);
        assert_eq!(cap_raw("short"), "short");

        let mut r = report(LABEL_DISCONNECTED, "s", &"x".repeat(MAX_RAW_CHARS * 2));
        r.stack = Some("y".repeat(MAX_RAW_CHARS + 1));
        let text = format_diagnostics(&r, "b");
        assert!(text.contains(&format!("… [{MAX_RAW_CHARS} characters omitted]")));
        assert!(text.contains("… [1 characters omitted]"));
    }

    #[test]
    fn summaries_are_one_capitalised_sentence() {
        assert_eq!(
            first_sentence("connection lost. Please refresh."),
            "Connection lost"
        );
        assert_eq!(first_sentence("v1.2 is old"), "V1.2 is old");
        assert_eq!(first_sentence("  trailing.  "), "Trailing");
        assert_eq!(first_sentence(""), "");
    }

    #[test]
    fn only_refreshing_reads_as_progress() {
        assert!(ErrorReport::auth_expired("x").is_progress());
        assert!(!ErrorReport::auth_reload_loop("x").is_progress());
        assert!(!ErrorReport::connect_timeout("x").is_progress());
    }

    #[test]
    fn a_subscribe_send_failure_copies_the_redacted_detail_never_the_raw_text() {
        use super::super::error::SynchronizerError;
        let e = SynchronizerError::SubscribeSendFailed {
            summary: "WebSocket state: CLOSED".into(),
            raw: "request error: RAW-ROOM-STATE".into(),
            detail: "WebSocket error: request error: ContractOp(Put (contents omitted)".into(),
        };
        let r = ErrorReport::from_synchronizer_error(&e);
        assert_eq!(r.label, LABEL_DISCONNECTED);
        assert_eq!(
            r.summary,
            "Couldn't subscribe to this room: WebSocket state: CLOSED"
        );
        let copied = r.diagnostics();
        assert!(
            copied.contains("ContractOp(Put (contents omitted)"),
            "{copied}"
        );
        assert!(!copied.contains("RAW-ROOM-STATE"), "{copied}");

        assert_eq!(
            ErrorReport::from_synchronizer_error(&SynchronizerError::ApiNotInitialized).summary,
            "Couldn't subscribe to this room: not connected to your Freenet node"
        );
    }
}
