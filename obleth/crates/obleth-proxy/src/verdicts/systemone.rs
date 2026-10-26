//! `POST /v1/systemone` — the SystemOne wire format, served by verdicts.
//!
//! SystemOne is the typed-decision API other decision servers speak (TypeSafe
//! Jev and the open models that copy its interface), and the format decision
//! benchmarks drive. It asks the same thing as verdicts under other names:
//!
//! | SystemOne | verdicts |
//! | --- | --- |
//! | `noul` question, answer `{"noul": p_yes}` | `boolean`, `probabilities.true` |
//! | `choice`, answer `choice` + `probabilities` | `choice`, `value` + `probabilities` |
//! | `score`, answer `score` (probability-weighted level from 0) + `probabilities` keyed `"0"`.. | `score`, `expected_value` (from 1) + `probabilities` array |
//! | `answers`, `usage.input_tokens` / `output_tokens` | `verdicts`, `usage.prompt_tokens` / `completion_tokens` |
//!
//! A request is translated, sent through the verdicts handler (auth, model
//! resolution, admission, budgets and settlement all unchanged) in groups of
//! at most [`MAX_QUESTIONS`] questions, and its answers translated back. A
//! SystemOne server always answers, so every group asks for
//! `on_no_answer: "low_confidence"`.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Method, Request, Response, StatusCode};
use serde_json::{json, Map, Value};

use super::types::{value_to_json_bytes, MAX_CHOICE_OPTIONS, MAX_QUESTIONS};
use super::VERDICTS_PATH;
use crate::proxy::{error_json, BODY_LIMIT};
use crate::state::AppState;

pub(crate) const SYSTEMONE_PATH: &str = "/v1/systemone";

/// Phrases of an upstream context overflow. Reported as 422 with "maximum
/// context length" in the message, the wording SystemOne clients read as a
/// capacity limit (the request does not fit) rather than a server failure.
const CONTEXT_OVERFLOW: &[&str] = &[
    "maximum context length",
    "max_model_len",
    "longer than the maximum",
    "context length",
    "rendered state is too large",
];

pub async fn handler(State(state): State<AppState>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => return error_json(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let request: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return error_json(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };
    let groups = match to_verdicts_requests(&request) {
        Ok(groups) => groups,
        Err((status, msg)) => return error_json(status, &msg),
    };

    let calls = groups.iter().map(|group| {
        let mut builder = Request::builder().method(Method::POST).uri(VERDICTS_PATH);
        for (name, value) in parts.headers.iter() {
            if name != header::CONTENT_LENGTH {
                builder = builder.header(name, value);
            }
        }
        let req = builder
            .body(Body::from(group.to_string()))
            .expect("a request built from valid parts");
        super::handler(State(state.clone()), req)
    });
    let responses = futures_util::future::join_all(calls).await;

    let mut answers = Map::new();
    let mut model: Option<String> = None;
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let mut request_id = None;
    for resp in responses {
        let status = resp.status();
        request_id = request_id.or_else(|| resp.headers().get("x-obleth-request-id").cloned());
        let retry_after = resp.headers().get(header::RETRY_AFTER).cloned();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if status != StatusCode::OK {
            let mut resp = error_response(status, &parsed);
            if let Some(value) = retry_after {
                resp.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return resp;
        }
        model = model.or_else(|| parsed["model"].as_str().map(str::to_string));
        input_tokens += parsed["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        output_tokens += parsed["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        if let Some(verdicts) = parsed["verdicts"].as_object() {
            for (id, verdict) in verdicts {
                answers.insert(id.clone(), to_answer(verdict));
            }
        }
    }

    let out = json!({
        "model": model.unwrap_or_default(),
        "answers": answers,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    });
    let body = match value_to_json_bytes(&out) {
        Ok(bytes) => bytes,
        Err(e) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to serialize systemone response: {e}"),
            )
        }
    };
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| error_json(StatusCode::INTERNAL_SERVER_ERROR, "response build failed"));
    if let Some(id) = request_id {
        resp.headers_mut().insert("x-obleth-request-id", id);
    }
    resp
}

/// The verdicts request bodies for one SystemOne request: its questions
/// translated, in groups of at most [`MAX_QUESTIONS`] (in question-id order),
/// each carrying the request's `model`, `state` and `images`.
fn to_verdicts_requests(request: &Value) -> Result<Vec<Value>, (StatusCode, String)> {
    let bad = |msg: String| (StatusCode::BAD_REQUEST, msg);
    let questions = request
        .get("questions")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("`questions` must be an object of typed questions".into()))?;
    if questions.is_empty() {
        return Err(bad("`questions` must contain at least one question".into()));
    }
    let mut translated: BTreeMap<&String, Value> = BTreeMap::new();
    for (id, q) in questions {
        translated.insert(id, to_verdict_question(id, q)?);
    }
    let ids: Vec<&String> = translated.keys().copied().collect();
    Ok(ids
        .chunks(MAX_QUESTIONS)
        .map(|chunk| {
            let mut body = Map::new();
            for field in ["model", "state", "images"] {
                if let Some(v) = request.get(field) {
                    body.insert(field.into(), v.clone());
                }
            }
            body.insert(
                "questions".into(),
                Value::Object(
                    chunk
                        .iter()
                        .map(|id| ((*id).clone(), translated[id].clone()))
                        .collect(),
                ),
            );
            body.insert("on_no_answer".into(), json!("low_confidence"));
            Value::Object(body)
        })
        .collect())
}

/// One SystemOne question as a verdicts question. A `noul` becomes a
/// `boolean` with the same optional `true`/`false` criteria; `choice` and
/// `score` carry over as they are.
fn to_verdict_question(id: &str, q: &Value) -> Result<Value, (StatusCode, String)> {
    let kind = q.get("type").and_then(Value::as_str).unwrap_or_default();
    let mut out = q.as_object().cloned().unwrap_or_default();
    match kind {
        "noul" => {
            out.insert("type".into(), json!("boolean"));
        }
        "choice" => {
            let options = q
                .get("criteria")
                .and_then(Value::as_object)
                .map_or(0, Map::len);
            if options > MAX_CHOICE_OPTIONS {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "question '{id}': {options} options per choice (maximum {MAX_CHOICE_OPTIONS})"
                    ),
                ));
            }
        }
        "score" => {}
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("question '{id}': unknown type '{other}' (expected noul, choice or score)"),
            ))
        }
    }
    Ok(Value::Object(out))
}

/// One verdict as a SystemOne answer.
fn to_answer(verdict: &Value) -> Value {
    let confidence = verdict["confidence"].clone();
    let mut answer = match verdict["type"].as_str() {
        Some("boolean") => json!({
            "type": "noul",
            "noul": verdict["probabilities"]["true"],
            "confidence": confidence,
        }),
        Some("score") => {
            let levels: Map<String, Value> = verdict["probabilities"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .enumerate()
                        .map(|(i, p)| (i.to_string(), p.clone()))
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "type": "score",
                "score": verdict["expected_value"].as_f64().map(|v| v - 1.0),
                "probabilities": levels,
                "confidence": confidence,
            })
        }
        _ => json!({
            "type": "choice",
            "choice": verdict["value"],
            "probabilities": verdict["probabilities"],
            "confidence": confidence,
        }),
    };
    if let Some(answered) = verdict.get("answered") {
        answer["answered"] = answered.clone();
    }
    answer
}

/// A failed verdicts group as the SystemOne response: the same status and
/// body, except that a context overflow is a 422.
fn error_response(status: StatusCode, body: &Value) -> Response<Body> {
    let message = body["error"]["message"]
        .as_str()
        .unwrap_or("verdicts request failed");
    let lower = message.to_ascii_lowercase();
    if CONTEXT_OVERFLOW.iter().any(|p| lower.contains(p)) {
        return error_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("maximum context length: {message}"),
        );
    }
    error_json(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noul_becomes_boolean_and_the_rest_carry_over() {
        let req = json!({
            "model": "judge",
            "state": {"ticket": "charged twice"},
            "questions": {
                "urgent": {"type": "noul", "instructions": "Urgent?", "criteria": {"true": "money", "false": "can wait"}},
                "team": {"type": "choice", "instructions": "Team?", "criteria": {"billing": null, "tech": null}},
                "anger": {"type": "score", "instructions": "Anger?", "criteria": ["calm", "angry"]}
            }
        });
        let groups = to_verdicts_requests(&req).unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g["model"], "judge");
        assert_eq!(g["state"]["ticket"], "charged twice");
        assert_eq!(g["on_no_answer"], "low_confidence");
        assert_eq!(g["questions"]["urgent"]["type"], "boolean");
        assert_eq!(g["questions"]["urgent"]["criteria"]["true"], "money");
        assert_eq!(g["questions"]["team"]["type"], "choice");
        assert_eq!(g["questions"]["anger"]["criteria"][1], "angry");
    }

    #[test]
    fn more_questions_than_one_verdicts_request_takes_are_split() {
        let questions: Map<String, Value> = (0..70)
            .map(|i| {
                (
                    format!("q{i:02}"),
                    json!({"type": "noul", "instructions": "?"}),
                )
            })
            .collect();
        let groups = to_verdicts_requests(&json!({"state": "s", "questions": questions})).unwrap();
        let sizes: Vec<usize> = groups
            .iter()
            .map(|g| g["questions"].as_object().unwrap().len())
            .collect();
        assert_eq!(sizes, vec![32, 32, 6]);
        assert!(groups
            .iter()
            .all(|g| g["state"] == "s" && g.get("model").is_none()));
    }

    #[test]
    fn too_many_options_is_a_capacity_limit() {
        let criteria: Map<String, Value> =
            (0..256).map(|i| (format!("o{i}"), Value::Null)).collect();
        let req = json!({"state": "s", "questions": {"q": {"type": "choice", "instructions": "?", "criteria": criteria}}});
        let (status, msg) = to_verdicts_requests(&req).unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(msg.contains("options per choice"), "{msg}");
    }

    #[test]
    fn an_unknown_type_is_rejected() {
        let req = json!({"state": "s", "questions": {"q": {"type": "rank", "instructions": "?"}}});
        assert_eq!(
            to_verdicts_requests(&req).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn answers_take_the_systemone_shapes() {
        let noul = to_answer(
            &json!({"type": "boolean", "value": true, "probabilities": {"true": 0.9, "false": 0.1}, "confidence": 0.5}),
        );
        assert_eq!(
            noul,
            json!({"type": "noul", "noul": 0.9, "confidence": 0.5})
        );

        let choice = to_answer(
            &json!({"type": "choice", "value": "billing", "probabilities": {"billing": 0.8, "tech": 0.2}, "confidence": 0.4}),
        );
        assert_eq!(choice["choice"], "billing");
        assert_eq!(choice["probabilities"]["tech"], 0.2);

        let score = to_answer(
            &json!({"type": "score", "value": 2, "probabilities": [0.1, 0.7, 0.2], "confidence": 0.3, "expected_value": 2.1}),
        );
        assert_eq!(score["type"], "score");
        assert!((score["score"].as_f64().unwrap() - 1.1).abs() < 1e-9);
        assert_eq!(
            score["probabilities"],
            json!({"0": 0.1, "1": 0.7, "2": 0.2})
        );

        let unanswered = to_answer(
            &json!({"type": "boolean", "value": true, "probabilities": {"true": 0.5, "false": 0.5}, "confidence": 0.0, "answered": false}),
        );
        assert_eq!(unanswered["answered"], false);
    }

    #[test]
    fn a_context_overflow_is_a_422() {
        let resp = error_response(
            StatusCode::BAD_GATEWAY,
            &json!({"error": {"message": "question 'q' on model 'm': upstream returned 400 Bad Request: This model's maximum context length is 8192 tokens"}}),
        );
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let other = error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error": {"message": "timed out waiting for model capacity"}}),
        );
        assert_eq!(other.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
