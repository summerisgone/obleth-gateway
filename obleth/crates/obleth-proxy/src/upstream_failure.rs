//! Failures an upstream reports inside a successful response.
//!
//! Once an OpenAI-compatible server has sent `200` and started a stream, the
//! only way left to report a failure is in the body: an SSE event carrying an
//! `error` object (what vLLM and OpenAI send when generation fails after the
//! stream opened), a typed `error` or `response.failed` event, or a choice
//! that ends with `finish_reason` `error` or `abort` (the server gave up on
//! the request). A buffered JSON response can likewise be a `200` whose body
//! is an error object.
//!
//! Such a response is a failed call. The gateway still passes its bytes
//! through untouched, but records it with an error status, bills only what
//! was produced, never caches it, and keeps it out of the completion-length
//! averages.

use serde_json::Value;

/// Status recorded for a failure that carries no HTTP code of its own.
pub(crate) const DEFAULT_FAILURE_STATUS: u16 = 502;

/// Error payloads are small; an SSE event or JSON body above this is
/// generated content, and is not inspected.
const MAX_INSPECTED_BYTES: usize = 64 * 1024;

/// Longest upstream message kept for logs and alerts.
const MAX_MESSAGE_CHARS: usize = 300;

/// A failure the upstream reported inside a non-error HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamFailure {
    /// The status to record: the error's own HTTP code when it carries one
    /// (400 to 599), otherwise [`DEFAULT_FAILURE_STATUS`].
    pub(crate) status: u16,
    /// The upstream's own description, shortened.
    pub(crate) message: String,
}

impl UpstreamFailure {
    fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status: status.unwrap_or(DEFAULT_FAILURE_STATUS),
            message: shorten(message.into()),
        }
    }
}

/// The failure one decoded payload reports, if any: a whole JSON body, or the
/// `data:` of one SSE event.
///
/// - a top-level `error` that is set (an object or a message; `null` and
///   empty values are not errors),
/// - a typed `error` or `response.failed` event,
/// - a choice whose `finish_reason` is `error` or `abort`.
pub(crate) fn payload_failure(value: &Value) -> Option<UpstreamFailure> {
    let object = value.as_object()?;
    if let Some(error) = object.get("error").filter(|e| is_set(e)) {
        let status = error_status(error).or_else(|| error_status(value));
        return Some(UpstreamFailure::new(status, error_message(error)));
    }
    match object.get("type").and_then(Value::as_str) {
        Some("error") => {
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error event");
            return Some(UpstreamFailure::new(error_status(value), message));
        }
        Some("response.failed") => {
            let error = value.pointer("/response/error").filter(|e| is_set(e));
            return Some(UpstreamFailure::new(
                error.and_then(error_status),
                error.map_or_else(|| "response failed".to_string(), error_message),
            ));
        }
        _ => {}
    }
    object
        .get("choices")?
        .as_array()?
        .iter()
        .find_map(|choice| {
            let reason = choice.get("finish_reason")?.as_str()?;
            matches!(reason, "error" | "abort")
                .then(|| UpstreamFailure::new(None, format!("generation ended with `{reason}`")))
        })
}

fn is_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(_) => true,
    }
}

/// An HTTP status the object carries in `code`, `status` or `status_code`,
/// as a number or a numeric string. OpenAI's own string codes
/// (`"server_error"`) are not statuses and are ignored.
fn error_status(value: &Value) -> Option<u16> {
    let object = value.as_object()?;
    ["code", "status", "status_code"].iter().find_map(|field| {
        let status = match object.get(*field)? {
            Value::Number(n) => n.as_u64()?,
            Value::String(s) => s.trim().parse().ok()?,
            _ => return None,
        };
        (400..=599).contains(&status).then_some(status as u16)
    })
}

fn error_message(error: &Value) -> String {
    match error {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| o.get("type").and_then(Value::as_str))
            .map_or_else(|| error.to_string(), str::to_string),
        other => other.to_string(),
    }
}

fn shorten(message: String) -> String {
    match message.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((at, _)) => format!("{}...", &message[..at]),
        None => message,
    }
}

/// Whether a payload could report a failure at all. Cheap enough to run on
/// every event of a long stream, so only the rare candidate is parsed.
fn may_report_failure(payload: &[u8]) -> bool {
    [&b"\"error\""[..], b"\"abort\"", b"response.failed"]
        .iter()
        .any(|needle| contains(payload, needle))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Watches the raw bytes of a response for a reported failure, as they pass
/// through to the client. Never alters or holds the stream.
///
/// SSE bodies are read event by event; the first failure wins. A JSON body is
/// judged whole at [`FailureWatch::finish`]. Events and bodies past
/// [`MAX_INSPECTED_BYTES`] are generated content and are skipped.
pub(crate) struct FailureWatch {
    sse: bool,
    /// SSE: the line being read. JSON: the body so far.
    pending: Vec<u8>,
    /// SSE: `data:` of the event being read.
    data: Vec<u8>,
    /// SSE: the event was named `error` (`event: error`).
    error_event: bool,
    /// The current line or JSON body outgrew the cap.
    pending_oversized: bool,
    /// SSE: the current event outgrew the cap.
    event_oversized: bool,
    failure: Option<UpstreamFailure>,
}

impl FailureWatch {
    /// A watch for a response of this content type, or `None` for bodies that
    /// cannot carry a JSON error (audio, images, plain text).
    pub(crate) fn for_content_type(content_type: &str) -> Option<Self> {
        let content_type = content_type.to_ascii_lowercase();
        let sse = content_type.contains("text/event-stream");
        (sse || content_type.contains("json")).then(|| Self {
            sse,
            pending: Vec::new(),
            data: Vec::new(),
            error_event: false,
            pending_oversized: false,
            event_oversized: false,
            failure: None,
        })
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if self.failure.is_some() {
            return;
        }
        if !self.sse {
            self.extend_pending(chunk);
            return;
        }
        let mut rest = chunk;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            self.extend_pending(&rest[..newline]);
            self.end_line();
            if self.failure.is_some() {
                return;
            }
            rest = &rest[newline + 1..];
        }
        self.extend_pending(rest);
    }

    /// The failure the response reported, if any.
    pub(crate) fn finish(mut self) -> Option<UpstreamFailure> {
        if self.failure.is_some() {
            return self.failure;
        }
        if self.sse {
            // A last event the upstream never terminated with a blank line.
            if !self.pending.is_empty() || self.pending_oversized {
                self.end_line();
            }
            self.dispatch();
            return self.failure;
        }
        if self.pending_oversized || !may_report_failure(&self.pending) {
            return None;
        }
        serde_json::from_slice::<Value>(&self.pending)
            .ok()
            .and_then(|body| payload_failure(&body))
    }

    fn extend_pending(&mut self, bytes: &[u8]) {
        if self.pending_oversized || bytes.is_empty() {
            return;
        }
        if self.pending.len() + bytes.len() > MAX_INSPECTED_BYTES {
            self.pending_oversized = true;
            self.pending = Vec::new();
            return;
        }
        self.pending.extend_from_slice(bytes);
    }

    fn end_line(&mut self) {
        if std::mem::take(&mut self.pending_oversized) {
            // Only generated content runs this long; the event it belongs to
            // is not an error report.
            self.pending.clear();
            self.event_oversized = true;
            return;
        }
        let line = match self.pending.split_last() {
            Some((b'\r', rest)) => rest,
            _ => &self.pending[..],
        };
        if line.is_empty() {
            self.pending.clear();
            self.dispatch();
            return;
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if !self.event_oversized {
                if self.data.len() + data.len() + 1 > MAX_INSPECTED_BYTES {
                    self.event_oversized = true;
                    self.data.clear();
                } else {
                    if !self.data.is_empty() {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(data);
                }
            }
        } else if let Some(name) = line.strip_prefix(b"event:") {
            if name.trim_ascii() == b"error" {
                self.error_event = true;
            }
        }
        // Comments (`:`), `id:` and `retry:` lines carry nothing to judge.
        self.pending.clear();
    }

    /// Judge the event read so far, then start the next one. The buffers are
    /// cleared rather than replaced, so a long stream reuses them.
    fn dispatch(&mut self) {
        let error_event = std::mem::take(&mut self.error_event);
        let oversized = std::mem::take(&mut self.event_oversized);
        let data = &self.data[..];
        let failure = if error_event {
            let reported = serde_json::from_slice::<Value>(data)
                .ok()
                .and_then(|v| payload_failure(&v));
            Some(reported.unwrap_or_else(|| {
                UpstreamFailure::new(None, String::from_utf8_lossy(data).trim().to_string())
            }))
        } else if oversized || data.is_empty() || data == b"[DONE]" || !may_report_failure(data) {
            None
        } else {
            serde_json::from_slice::<Value>(data)
                .ok()
                .and_then(|value| payload_failure(&value))
        };
        self.data.clear();
        if failure.is_some() {
            self.failure = failure;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn watch_sse(wire: &[u8]) -> Option<UpstreamFailure> {
        let mut watch = FailureWatch::for_content_type("text/event-stream; charset=utf-8")
            .expect("an SSE watch");
        watch.push(wire);
        watch.finish()
    }

    /// Every split of `wire` into two pushes reports the same thing.
    fn watch_sse_split_everywhere(wire: &str) -> Option<UpstreamFailure> {
        let whole = watch_sse(wire.as_bytes());
        for cut in 0..=wire.len() {
            let mut watch = FailureWatch::for_content_type("text/event-stream").unwrap();
            watch.push(&wire.as_bytes()[..cut]);
            watch.push(&wire.as_bytes()[cut..]);
            assert_eq!(watch.finish(), whole, "split at byte {cut}");
        }
        whole
    }

    fn content(text: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"object": "chat.completion.chunk",
                   "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
        )
    }

    #[test]
    fn an_openai_compatible_error_event_after_output_is_a_failure_with_its_code() {
        // The shape vLLM's OpenAI server sends when generation fails after the
        // stream opened (Python's `json.dumps`, so spaces after separators).
        let wire = format!(
            "{}{}data: {}\n\ndata: [DONE]\n\n",
            content("Hello"),
            content(", wor"),
            r#"{"error": {"message": "Internal server error", "type": "InternalServerError", "param": null, "code": 500}}"#
        );
        let failure = watch_sse_split_everywhere(&wire).expect("a failure");
        assert_eq!(failure.status, 500);
        assert_eq!(failure.message, "Internal server error");
    }

    #[test]
    fn an_error_event_before_any_output_keeps_a_client_error_code() {
        let wire = concat!(
            "data: {\"error\": {\"object\": \"error\", \"message\": \"bad tool schema\", ",
            "\"type\": \"BadRequestError\", \"param\": null, \"code\": 400}}\n\n",
            "data: [DONE]\n\n"
        );
        let failure = watch_sse_split_everywhere(wire).unwrap();
        assert_eq!(failure.status, 400);
        assert_eq!(failure.message, "bad tool schema");
    }

    #[test]
    fn errors_without_an_http_code_record_bad_gateway() {
        for payload in [
            // OpenAI-style string code.
            json!({"error": {"message": "The server had an error", "type": "server_error", "code": "server_error"}}),
            // A bare message.
            json!({"error": "Request failed during generation: out of memory", "error_type": "generation"}),
            // A typed error event (Responses-style, and Anthropic-style).
            json!({"type": "error", "code": "server_error", "message": "boom", "sequence_number": 4}),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ] {
            let failure = watch_sse(format!("data: {payload}\n\n").as_bytes())
                .unwrap_or_else(|| panic!("{payload} is a failure"));
            assert_eq!(failure.status, DEFAULT_FAILURE_STATUS, "{payload}");
            assert!(!failure.message.is_empty());
        }
    }

    #[test]
    fn a_numeric_string_code_and_a_top_level_status_are_honoured() {
        let failure =
            payload_failure(&json!({"error": {"message": "slow down", "code": "429"}})).unwrap();
        assert_eq!(failure.status, 429);
        let failure = payload_failure(&json!({"error": "unavailable", "status": 503})).unwrap();
        assert_eq!(failure.status, 503);
        // Out-of-range numbers are not statuses.
        let failure = payload_failure(&json!({"error": {"message": "x", "code": 7}})).unwrap();
        assert_eq!(failure.status, DEFAULT_FAILURE_STATUS);
    }

    #[test]
    fn an_abort_or_error_finish_reason_is_a_failure_and_ordinary_ones_are_not() {
        for reason in ["abort", "error"] {
            let wire = format!(
                "{}data: {}\n\ndata: [DONE]\n\n",
                content("partial"),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
            );
            let failure = watch_sse_split_everywhere(&wire).expect("a failure");
            assert_eq!(failure.status, DEFAULT_FAILURE_STATUS);
        }
        for reason in ["stop", "length", "tool_calls", "content_filter"] {
            let wire = format!(
                "{}data: {}\n\ndata: [DONE]\n\n",
                content("done"),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
            );
            assert_eq!(watch_sse(wire.as_bytes()), None, "{reason}");
        }
    }

    #[test]
    fn a_named_error_event_is_a_failure_whatever_its_data() {
        let failure = watch_sse(b"event: error\ndata: upstream went away\n\n").unwrap();
        assert_eq!(failure.status, DEFAULT_FAILURE_STATUS);
        assert_eq!(failure.message, "upstream went away");
        let failure = watch_sse(
            b"event: error\r\ndata: {\"error\": {\"message\": \"nope\", \"code\": 503}}\r\n\r\n",
        )
        .unwrap();
        assert_eq!(failure.status, 503);
    }

    #[test]
    fn a_clean_stream_is_not_a_failure_even_when_its_text_talks_about_errors() {
        let wire = format!(
            "{}{}{}data: {}\n\ndata: [DONE]\n\n",
            // Quotes inside content are escaped, but a candidate can still
            // turn up; parsing settles it.
            content("say \"error"),
            content("\" or \"abort\" and {\"error\": 1}"),
            ": keep-alive comment\n\n",
            json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}, "error": null})
        );
        assert_eq!(watch_sse_split_everywhere(&wire), None);
    }

    #[test]
    fn the_first_failure_wins() {
        let wire = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"error": {"message": "first", "code": 500}}),
            json!({"error": {"message": "second", "code": 400}}),
        );
        assert_eq!(watch_sse(wire.as_bytes()).unwrap().message, "first");
    }

    #[test]
    fn an_unterminated_final_error_event_still_counts() {
        let failure =
            watch_sse(br#"data: {"error": {"message": "cut", "code": 500}}"#).expect("a failure");
        assert_eq!(failure.status, 500);
    }

    #[test]
    fn oversized_events_are_skipped_without_losing_the_next_one() {
        let big = "x".repeat(MAX_INSPECTED_BYTES + 10);
        let wire = format!(
            "{}data: {}\n\n",
            content(&big),
            json!({"error": {"message": "after a long event", "code": 500}})
        );
        // Delivered in small pieces, as a real stream would be.
        let mut watch = FailureWatch::for_content_type("text/event-stream").unwrap();
        for piece in wire.as_bytes().chunks(1000) {
            watch.push(piece);
        }
        assert_eq!(watch.finish().unwrap().message, "after a long event");
        // An oversized error-looking event is generated content, not a report.
        let wire = format!("data: {{\"error\": \"{big}\"}}\n\n");
        assert_eq!(watch_sse(wire.as_bytes()), None);
    }

    #[test]
    fn a_json_body_that_is_an_error_object_is_a_failure() {
        let body = json!({"error": {"message": "engine dead", "type": "InternalServerError", "code": 500}});
        let mut watch = FailureWatch::for_content_type("application/json").unwrap();
        for byte in body.to_string().as_bytes() {
            watch.push(&[*byte]);
        }
        let failure = watch.finish().unwrap();
        assert_eq!(
            (failure.status, failure.message.as_str()),
            (500, "engine dead")
        );
    }

    #[test]
    fn ordinary_json_bodies_are_not_failures() {
        for body in [
            json!({"object": "chat.completion",
                   "choices": [{"index": 0, "message": {"role": "assistant", "content": "an \"error\""},
                                "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 1, "completion_tokens": 2}}),
            json!({"object": "response", "status": "completed", "error": null, "output": []}),
            json!({"object": "list", "data": [{"embedding": [0.1, 0.2]}]}),
        ] {
            let mut watch = FailureWatch::for_content_type("application/json").unwrap();
            watch.push(body.to_string().as_bytes());
            assert_eq!(watch.finish(), None, "{body}");
        }
        // Not JSON at all.
        let mut watch = FailureWatch::for_content_type("application/json").unwrap();
        watch.push(b"{\"error\": ");
        assert_eq!(watch.finish(), None);
    }

    #[test]
    fn an_aborted_json_completion_and_a_failed_response_object_are_failures() {
        let aborted = json!({"object": "chat.completion",
            "choices": [{"index": 0, "message": {"content": "par"}, "finish_reason": "abort"}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 1}});
        assert!(payload_failure(&aborted).is_some());
        let failed = json!({"type": "response.failed",
            "response": {"status": "failed", "error": {"code": "server_error", "message": "no"}}});
        assert_eq!(payload_failure(&failed).unwrap().message, "no");
    }

    #[test]
    fn bodies_that_cannot_carry_a_json_error_are_not_watched() {
        for content_type in ["audio/mpeg", "image/png", "text/plain", ""] {
            assert!(FailureWatch::for_content_type(content_type).is_none());
        }
        assert!(FailureWatch::for_content_type("application/json; charset=utf-8").is_some());
        assert!(FailureWatch::for_content_type("Text/Event-Stream").is_some());
    }

    #[test]
    fn a_json_body_past_the_cap_is_not_judged() {
        let mut watch = FailureWatch::for_content_type("application/json").unwrap();
        watch.push(br#"{"error": {"message": "x", "code": 500}, "pad": ""#);
        watch.push(&vec![b'y'; MAX_INSPECTED_BYTES]);
        watch.push(br#""}"#);
        assert_eq!(watch.finish(), None);
    }

    #[test]
    fn long_messages_are_shortened() {
        let failure = payload_failure(&json!({"error": "e".repeat(1000)})).unwrap();
        assert_eq!(failure.message.chars().count(), MAX_MESSAGE_CHARS + 3);
    }
}

/// Bare HTTP/1.1 upstreams, for tests that need a connection to fail in ways a
/// framework server will not.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// How the upstream drops a connection it fails.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Hangup {
        /// A TCP reset.
        Reset,
        /// An orderly close.
        Close,
    }

    /// An upstream at the returned base URL (`.../v1`) whose first `failures`
    /// connections read the whole request and then hang up before sending a
    /// byte of response; every later one answers `200` with `body` as JSON.
    /// The counter is the number of connections accepted.
    pub(crate) async fn spawn_failing_upstream(
        failures: usize,
        hangup: Hangup,
        body: String,
    ) -> (Arc<AtomicUsize>, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                tokio::spawn(async move {
                    read_request(&mut socket).await;
                    if n < failures {
                        if matches!(hangup, Hangup::Reset) {
                            let _ = socket.set_zero_linger();
                        }
                        drop(socket);
                        return;
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (accepted, format!("http://{addr}/v1"))
    }

    /// Read one request: its headers, then `content-length` bytes of body.
    async fn read_request(socket: &mut TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + length {
                    return;
                }
            }
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }
}

/// The whole pipeline against fake upstreams: how a failure reported inside a
/// `200` is recorded, billed and kept out of the cache, and that a connection
/// the upstream drops before answering is retried. Needs the test datastores
/// (`OBLETH_TEST_DATABASE_URL`, `OBLETH_TEST_REDIS_URL`); skipped without
/// them.
#[cfg(test)]
mod pipeline {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{header, Method, Request, Response, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use moka::future::Cache;
    use obleth_config::{ResolvedKey, ResolvedModel};
    use serde_json::{json, Value};
    use uuid::Uuid;

    use super::test_support::{spawn_failing_upstream, Hangup};
    use crate::state::AppState;

    /// Price of one output token; input is free, so spend counts output only.
    const OUTPUT_RATE: f64 = 0.001;

    #[derive(Default)]
    struct Upstream {
        hits: Mutex<HashMap<String, usize>>,
    }

    impl Upstream {
        fn hits(&self, model: &str) -> usize {
            self.hits.lock().unwrap().get(model).copied().unwrap_or(0)
        }
    }

    fn sse(events: &[Value]) -> Response<Body> {
        let mut text: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        text.push_str("data: [DONE]\n\n");
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(text))
            .unwrap()
    }

    fn delta(text: &str) -> Value {
        json!({"id": "c1", "object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
    }

    /// What the fake upstream answers, by the model it is asked for.
    fn reply(model: &str) -> Response<Body> {
        match model {
            "fails-mid-stream" => sse(&[
                delta("Hello"),
                delta(" world"),
                json!({"error": {"message": "Internal server error",
                                 "type": "InternalServerError", "param": null, "code": 500}}),
            ]),
            "streams-cleanly" => sse(&[
                delta("Hello"),
                delta(" world"),
                json!({"id": "c1", "object": "chat.completion.chunk",
                       "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                json!({"id": "c1", "object": "chat.completion.chunk", "choices": [],
                       "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}),
            ]),
            "errors-in-json" => Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"error": {"message": "engine unavailable",
                                     "type": "ServiceUnavailableError", "code": 503}})
                    .to_string(),
                ))
                .unwrap(),
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        }
    }

    async fn spawn_upstream() -> (Arc<Upstream>, String) {
        let upstream = Arc::new(Upstream::default());
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |State(up): State<Arc<Upstream>>, Json(body): Json<Value>| async move {
                        let model = body["model"].as_str().unwrap_or_default().to_string();
                        *up.hits.lock().unwrap().entry(model.clone()).or_default() += 1;
                        reply(&model)
                    },
                ),
            )
            .with_state(upstream.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (upstream, format!("http://{addr}/v1"))
    }

    struct Gateway {
        state: AppState,
        wal_dir: std::path::PathBuf,
    }

    impl Drop for Gateway {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.wal_dir);
        }
    }

    impl Gateway {
        async fn register(&self, route: ResolvedModel) {
            self.state
                .model_cache
                .insert(route.model_name.clone(), Arc::new(route))
                .await;
        }

        /// A key for a fresh tenant with a lifetime cost cap, so its spend is
        /// readable from the term counters.
        async fn tenant(&self) -> (String, ResolvedKey) {
            let mut key = crate::boons::test_support::test_key();
            key.key_id = Uuid::new_v4();
            key.tenant_id = Uuid::new_v4();
            key.tenant_name = format!("t-{}", key.tenant_id);
            key.budget_cost_usd = Some(1000.0);
            key.budget_period = Some("lifetime".into());
            let secret = format!("sk-failure-{}", Uuid::new_v4());
            self.state
                .key_cache
                .insert(obleth_config::hash_api_key(&secret), Arc::new(key.clone()))
                .await;
            (secret, key)
        }

        async fn spent(&self, key: &ResolvedKey) -> f64 {
            let period = crate::proxy::term_period_key(key, chrono::Utc::now()).expect("capped");
            self.state
                .redis
                .term_usage_read(&key.tenant_id, &period)
                .await
                .expect("term usage")
                .1
        }

        /// Send a chat request and read the whole response, which is when a
        /// stream settles.
        async fn chat(&self, secret: &str, body: &Value) -> (StatusCode, bool, String) {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let resp = crate::proxy::proxy_handler(State(self.state.clone()), req).await;
            let status = resp.status();
            let cache_hit = resp
                .headers()
                .get("x-obleth-cache")
                .is_some_and(|v| v == "hit");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .expect("body");
            (
                status,
                cache_hit,
                String::from_utf8_lossy(&bytes).into_owned(),
            )
        }

        /// Settled requests by status class, from the gateway's own metrics.
        fn requests_with_status(&self, class: &str) -> u64 {
            let label = format!("status=\"{class}\"");
            self.state
                .metrics
                .encode()
                .lines()
                .filter(|l| l.starts_with("obleth_requests_total{") && l.contains(&label))
                .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
                .sum::<f64>() as u64
        }

        fn output_tokens(&self) -> u64 {
            self.state
                .metrics
                .encode()
                .lines()
                .find_map(|l| l.strip_prefix("obleth_output_tokens_total "))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(0.0) as u64
        }
    }

    async fn gateway() -> Option<Gateway> {
        let (Ok(db), Ok(redis_url)) = (
            std::env::var("OBLETH_TEST_DATABASE_URL"),
            std::env::var("OBLETH_TEST_REDIS_URL"),
        ) else {
            eprintln!("skipping: set OBLETH_TEST_DATABASE_URL and OBLETH_TEST_REDIS_URL to run");
            return None;
        };
        let store = obleth_store::Store::connect(&db).await.expect("postgres");
        let redis = obleth_redis::RedisStore::connect(&redis_url)
            .await
            .expect("redis");
        let wal_dir = std::env::temp_dir().join(format!("obleth-failure-wal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&wal_dir).unwrap();
        let telemetry = obleth_telemetry::TelemetrySink::start(
            "http://127.0.0.1:1",
            "obleth_test",
            "default",
            "",
            wal_dir.join("usage.wal").to_str().unwrap(),
            Duration::from_millis(100),
            true,
        )
        .await
        .expect("telemetry sink");
        let http = reqwest::Client::new();
        let state = AppState {
            redis,
            fairshare: obleth_fairshare::FairShare::start(
                Arc::new(obleth_fairshare::StaticCapacity::new(64)),
                obleth_config::FairshareAlgorithm::default(),
                64,
            ),
            tokenizer: Arc::new(obleth_tokenizer::HeuristicTokenizer::new()),
            telemetry,
            http: http.clone(),
            upstream_base: "http://127.0.0.1:1/v1".into(),
            upstream_timeout: Duration::from_secs(30),
            key_cache: Cache::builder().build(),
            model_cache: Cache::builder().build(),
            mcp_cache: Cache::builder().build(),
            model_registry: crate::router::ModelRegistry::new(),
            families: crate::reasoning::FamilyRegistry::new(),
            classifier: crate::classifier::Classifier::new(Default::default()),
            output_stats: Default::default(),
            boons: crate::boons::BoonEngine::new(Default::default()),
            tool_cache: Cache::builder().build(),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            fail_open: false,
            alerts: obleth_admin::AlertDispatcher::new(http, Default::default()),
            session_id_derivation: false,
            compressor: None,
            energy: crate::energy::EnergyEngine::new(Default::default()),
            jwt: None,
            knowledge: Arc::new(crate::knowledge::KnowledgeIndex::new()),
            video_jobs: crate::videos::VideoJobStore::new(store, Default::default()),
        };
        Some(Gateway { state, wal_dir })
    }

    /// A cached, output-priced model served by the fake upstream.
    fn route(name: &str, base: &str) -> ResolvedModel {
        let mut route = crate::boons::test_support::test_route();
        route.model_name = name.into();
        route.upstream_model = name.into();
        route.api_base = base.into();
        route.cache_enabled = true;
        route.cache_ttl_secs = 60;
        route.output_cost_per_token = OUTPUT_RATE;
        route
    }

    fn request(model: &str, stream: bool) -> Value {
        json!({
            "model": model,
            "stream": stream,
            "max_tokens": 8192,
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    #[tokio::test]
    async fn an_error_event_inside_a_200_stream_is_a_failure_billed_for_its_output() {
        let Some(gw) = gateway().await else { return };
        let (upstream, base) = spawn_upstream().await;
        gw.register(route("fails-mid-stream", &base)).await;
        let (secret, key) = gw.tenant().await;
        let body = request("fails-mid-stream", true);

        let (status, _, text) = gw.chat(&secret, &body).await;
        // The client gets the stream as the upstream sent it, error included.
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("Internal server error"), "{text}");
        // Recorded with the error's own code, and billed for the text that
        // was delivered ("Hello world": 11 characters, 2 tokens), never for
        // the 8192 `max_tokens` the request allowed.
        assert_eq!(gw.requests_with_status("5xx"), 1);
        assert_eq!(gw.requests_with_status("2xx"), 0);
        assert_eq!(gw.output_tokens(), 2);
        assert!((gw.spent(&key).await - 2.0 * OUTPUT_RATE).abs() < 1e-9);
        // Not a sample of how long this model's answers run.
        assert!(!gw
            .state
            .output_stats
            .snapshot()
            .contains_key("fails-mid-stream"));
        // Never cached: the same request goes to the upstream again.
        let (_, cache_hit, _) = gw.chat(&secret, &body).await;
        assert!(!cache_hit);
        assert_eq!(upstream.hits("fails-mid-stream"), 2);
    }

    #[tokio::test]
    async fn a_clean_stream_settles_its_usage_and_is_cached() {
        let Some(gw) = gateway().await else { return };
        let (upstream, base) = spawn_upstream().await;
        gw.register(route("streams-cleanly", &base)).await;
        let (secret, key) = gw.tenant().await;
        let body = request("streams-cleanly", true);

        let (status, _, _) = gw.chat(&secret, &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(gw.requests_with_status("2xx"), 1);
        assert_eq!(gw.output_tokens(), 2);
        assert!((gw.spent(&key).await - 2.0 * OUTPUT_RATE).abs() < 1e-9);
        assert_eq!(
            gw.state.output_stats.snapshot().get("streams-cleanly"),
            Some(&2.0)
        );
        let (_, cache_hit, _) = gw.chat(&secret, &body).await;
        assert!(cache_hit);
        assert_eq!(upstream.hits("streams-cleanly"), 1);
    }

    #[tokio::test]
    async fn a_200_whose_json_body_is_an_error_is_a_failure_billed_nothing() {
        let Some(gw) = gateway().await else { return };
        let (upstream, base) = spawn_upstream().await;
        gw.register(route("errors-in-json", &base)).await;
        let (secret, key) = gw.tenant().await;
        let body = request("errors-in-json", false);

        let (status, _, text) = gw.chat(&secret, &body).await;
        assert_eq!(status, StatusCode::OK, "passed through as sent");
        assert!(text.contains("engine unavailable"), "{text}");
        assert_eq!(gw.requests_with_status("5xx"), 1);
        assert_eq!(gw.output_tokens(), 0);
        assert_eq!(gw.spent(&key).await, 0.0);
        assert!(!gw
            .state
            .output_stats
            .snapshot()
            .contains_key("errors-in-json"));
        let (_, cache_hit, _) = gw.chat(&secret, &body).await;
        assert!(!cache_hit);
        assert_eq!(upstream.hits("errors-in-json"), 2);
    }

    #[tokio::test]
    async fn a_connection_dropped_before_response_headers_is_retried() {
        let Some(gw) = gateway().await else { return };
        let answer = json!({
            "id": "c1", "object": "chat.completion", "model": "flaky",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
        })
        .to_string();
        // With retries configured, and with none (the one fresh-connection
        // retry a dropped connection always gets).
        for (max_retries, hangup) in [(1, Hangup::Reset), (0, Hangup::Reset), (0, Hangup::Close)] {
            let (accepted, base) = spawn_failing_upstream(1, hangup, answer.clone()).await;
            let name = format!("flaky-{max_retries}-{hangup:?}");
            let mut flaky = route(&name, &base);
            flaky.cache_enabled = false;
            flaky.max_retries = max_retries;
            flaky.retry_backoff_ms = 0;
            gw.register(flaky).await;
            let (secret, _) = gw.tenant().await;

            let (status, _, text) = gw.chat(&secret, &request(&name, false)).await;
            assert_eq!(status, StatusCode::OK, "{name}: {text}");
            assert_eq!(accepted.load(Ordering::SeqCst), 2, "{name}");
        }
    }
}
