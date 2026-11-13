//! Mapping from GenAI events to OTLP spans.

use opentelemetry_proto::tonic::common::v1::{any_value::Value as AV, AnyValue, KeyValue};
use opentelemetry_proto::tonic::trace::v1::{span::SpanKind, status::StatusCode, Span, Status};
use serde_json::{json, Map, Value};

use super::{ApiSurface, CollectedOutput, Content, GenAiEvent, RequestParams};

fn text_part(s: &str) -> Value {
    json!({"type": "text", "content": s})
}

fn tok(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

// Only token-shaped pieces survive: free text in a url or mime never reaches the span.
fn host_only(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host = auth.rsplit('@').next().unwrap_or("");
            if tok(scheme, "+.-") && tok(host, ".-:[]_") {
                format!("{scheme}://{host}")
            } else {
                "unknown".to_string()
            }
        }
        None => "unknown".to_string(),
    }
}

fn image_part(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        let mime = rest.split([';', ',']).next().unwrap_or("");
        let ok = mime.split_once('/').is_some_and(|(a, b)| tok(a, "+.-") && tok(b, "+.-"));
        let mime = if ok { mime } else { "unknown" };
        json!({"type": "blob", "modality": "image", "mime_type": mime})
    } else {
        json!({"type": "uri", "modality": "image", "uri": host_only(url)})
    }
}

fn content_parts(c: &Value) -> Vec<Value> {
    match c {
        Value::String(s) if !s.is_empty() => vec![text_part(s)],
        Value::Array(items) => items
            .iter()
            .filter_map(|it| match it {
                Value::String(s) => Some(text_part(s)),
                Value::Object(o) => match o.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        o.get("text").and_then(Value::as_str).map(text_part)
                    }
                    Some("image_url") | Some("input_image") => {
                        let u = o.get("image_url").or_else(|| o.get("url"))?;
                        let url = u.as_str().or_else(|| u.get("url").and_then(Value::as_str))?;
                        Some(image_part(url))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

fn parse_args(s: &str) -> Value {
    serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.to_string()))
}

fn f64_of(b: &Value, k: &str) -> Option<f64> {
    b.get(k).and_then(Value::as_f64)
}

fn i64_of(b: &Value, k: &str) -> Option<i64> {
    b.get(k).and_then(Value::as_i64)
}

pub fn parse_request(surface: ApiSurface, body: &[u8]) -> Result<(RequestParams, Content), String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "invalid json".to_string())?;
    if !v.is_object() {
        return Err("invalid json".to_string());
    }
    let mut p = RequestParams {
        temperature: f64_of(&v, "temperature"),
        top_p: f64_of(&v, "top_p"),
        top_k: f64_of(&v, "top_k"),
        max_tokens: i64_of(&v, "max_completion_tokens").or_else(|| i64_of(&v, "max_tokens")),
        seed: i64_of(&v, "seed"),
        frequency_penalty: f64_of(&v, "frequency_penalty"),
        presence_penalty: f64_of(&v, "presence_penalty"),
        choice_count: i64_of(&v, "n"),
        stream: v.get("stream").and_then(Value::as_bool).unwrap_or(false),
        output_type: None,
    };
    p.output_type = match v.pointer("/response_format/type").and_then(Value::as_str) {
        Some("json_object") | Some("json_schema") => Some("json".to_string()),
        Some("text") => Some("text".to_string()),
        _ => None,
    };
    let mut c = Content::default();
    c.user = v
        .get("user")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/metadata/user_id").and_then(Value::as_str))
        .map(str::to_string);

    if surface == ApiSurface::Completions {
        let prompts: Vec<&Value> = match v.get("prompt") {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(p) => vec![p],
            None => vec![],
        };
        for pr in prompts {
            if let Some(s) = pr.as_str() {
                c.input
                    .push(json!({"role": "user", "parts": [text_part(s)]}));
            }
        }
        return Ok((p, c));
    }

    for m in v
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m.get("content").unwrap_or(&Value::Null);
        match role {
            "system" | "developer" => c.system.extend(content_parts(content)),
            "tool" => {
                let id = m.get("tool_call_id").cloned().unwrap_or(Value::Null);
                c.input.push(json!({"role": "tool", "parts": [
                    {"type": "tool_call_response", "id": id, "response": content}
                ]}));
            }
            _ => {
                let mut parts = content_parts(content);
                if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let f = tc.get("function").unwrap_or(&Value::Null);
                        let args = match f.get("arguments") {
                            Some(Value::String(s)) => parse_args(s),
                            Some(o) => o.clone(),
                            None => Value::Null,
                        };
                        parts.push(json!({
                            "type": "tool_call",
                            "id": tc.get("id").cloned().unwrap_or(Value::Null),
                            "name": f.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": args,
                        }));
                    }
                }
                let mut o = Map::new();
                o.insert("role".into(), Value::String(role.to_string()));
                o.insert("parts".into(), Value::Array(parts));
                c.input.push(Value::Object(o));
            }
        }
    }
    Ok((p, c))
}

fn semconv_finish(r: Option<&str>) -> String {
    match r {
        Some("tool_calls") | Some("function_call") => "tool_call".to_string(),
        Some(o) => o.to_string(),
        None => String::new(),
    }
}

pub fn output_messages(out: &CollectedOutput) -> Vec<Value> {
    out.choices
        .iter()
        .map(|ch| {
            let mut parts = Vec::new();
            if !ch.reasoning.is_empty() {
                parts.push(json!({"type": "reasoning", "content": ch.reasoning}));
            }
            if !ch.text.is_empty() {
                parts.push(text_part(&ch.text));
            }
            for t in &ch.tool_calls {
                parts.push(json!({
                    "type": "tool_call", "id": t.id, "name": t.name,
                    "arguments": parse_args(&t.arguments),
                }));
            }
            let role = if ch.role.is_empty() { "assistant" } else { &ch.role };
            json!({
                "role": role,
                "parts": parts,
                "finish_reason": semconv_finish(ch.finish_reason.as_deref()),
            })
        })
        .collect()
}

fn json_len(v: &[Value]) -> usize {
    serde_json::to_string(v).map_or(0, |s| s.len())
}

fn collect_texts<'a>(v: &'a mut Value, out: &mut Vec<&'a mut String>) {
    match v {
        Value::Object(o) => {
            if matches!(
                o.get("type").and_then(Value::as_str),
                Some("text") | Some("reasoning")
            ) {
                if let Some(Value::String(s)) = o.get_mut("content") {
                    out.push(s);
                }
            } else {
                for child in o.values_mut() {
                    collect_texts(child, out);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| collect_texts(x, out)),
        _ => {}
    }
}

fn cut_str(s: &mut String, by: usize) {
    let mut n = s.len().saturating_sub(by.max(1));
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s.truncate(n);
}

fn truncate_vec(v: &mut Vec<Value>, max: usize, can_drop: bool) -> bool {
    let mut changed = false;
    loop {
        let size = json_len(v);
        if size <= max {
            return changed;
        }
        let excess = size - max;
        let mut texts = Vec::new();
        v.iter_mut().for_each(|x| collect_texts(x, &mut texts));
        let longest = texts.into_iter().max_by_key(|s| s.len()).filter(|s| !s.is_empty());
        if let Some(s) = longest {
            cut_str(s, excess);
            changed = true;
        } else if can_drop && !v.is_empty() {
            v.remove(0);
            changed = true;
        } else {
            return changed;
        }
    }
}

pub fn truncate_content(c: &mut Content, max_bytes: usize) -> (bool, bool) {
    let sys = truncate_vec(&mut c.system, max_bytes, false);
    let input = truncate_vec(&mut c.input, max_bytes, true);
    let output = truncate_vec(&mut c.output, max_bytes, false);
    (input || sys, output)
}

fn kv(k: &str, v: AV) -> KeyValue {
    KeyValue {
        key: k.to_string(),
        value: Some(AnyValue { value: Some(v) }),
        ..Default::default()
    }
}

fn s(k: &str, v: &str) -> KeyValue {
    kv(k, AV::StringValue(v.to_string()))
}

pub fn build_span(
    ev: &GenAiEvent,
    params: Option<&RequestParams>,
    content: Option<&Content>,
    scrubbed: &str,
    flags: &[(&str, bool)],
) -> Span {
    let m = &ev.meta;
    let op = ev.surface.operation_name();
    let provider = if m.provider.is_empty() { "openai" } else { m.provider.as_str() };
    let mut a: Vec<KeyValue> = Vec::new();
    a.push(s("gen_ai.operation.name", op));
    a.push(s("gen_ai.provider.name", provider));
    a.push(s("gen_ai.request.model", &m.client_model));
    a.push(s("langfuse.observation.model", &m.client_model));
    if let Some(o) = &ev.output {
        if let Some(mo) = &o.model {
            a.push(s("gen_ai.response.model", mo));
        }
        if let Some(id) = &o.id {
            a.push(s("gen_ai.response.id", id));
        }
        let fr: Vec<AnyValue> = o
            .choices
            .iter()
            .filter_map(|c| c.finish_reason.as_deref())
            .map(|r| AnyValue { value: Some(AV::StringValue(semconv_finish(Some(r)))) })
            .collect();
        if !fr.is_empty() {
            a.push(kv(
                "gen_ai.response.finish_reasons",
                AV::ArrayValue(opentelemetry_proto::tonic::common::v1::ArrayValue { values: fr }),
            ));
        }
    }
    if let Some(p) = params {
        let fl = |k: &str, v: Option<f64>, a: &mut Vec<KeyValue>| {
            if let Some(v) = v {
                a.push(kv(k, AV::DoubleValue(v)));
            }
        };
        let il = |k: &str, v: Option<i64>, a: &mut Vec<KeyValue>| {
            if let Some(v) = v {
                a.push(kv(k, AV::IntValue(v)));
            }
        };
        fl("gen_ai.request.temperature", p.temperature, &mut a);
        fl("gen_ai.request.top_p", p.top_p, &mut a);
        fl("gen_ai.request.top_k", p.top_k, &mut a);
        il("gen_ai.request.max_tokens", p.max_tokens, &mut a);
        il("gen_ai.request.seed", p.seed, &mut a);
        fl("gen_ai.request.frequency_penalty", p.frequency_penalty, &mut a);
        fl("gen_ai.request.presence_penalty", p.presence_penalty, &mut a);
        il("gen_ai.request.choice.count", p.choice_count, &mut a);
        if p.stream {
            a.push(kv("gen_ai.request.stream", AV::BoolValue(true)));
        }
        if let Some(t) = &p.output_type {
            a.push(s("gen_ai.output.type", t));
        }
    }
    if let Some(r) = &m.reasoning_effort {
        a.push(s("gen_ai.request.reasoning.level", r));
    }
    if let Some(t) = ev.ttft_unix_nanos {
        let secs = t.saturating_sub(ev.start_unix_nanos) as f64 / 1e9;
        a.push(kv("gen_ai.response.time_to_first_chunk", AV::DoubleValue(secs)));
        let dt = chrono::DateTime::from_timestamp_nanos(t as i64);
        a.push(s(
            "langfuse.observation.completion_start_time",
            &dt.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ));
    }
    let u = &m.usage;
    for (k, v) in [
        ("gen_ai.usage.input_tokens", u.input),
        ("gen_ai.usage.output_tokens", u.output),
        ("gen_ai.usage.cache_read.input_tokens", u.cache_read),
        ("gen_ai.usage.reasoning.output_tokens", u.reasoning),
    ] {
        if let Some(v) = v {
            a.push(kv(k, AV::IntValue(v.min(i64::MAX as u64) as i64)));
        }
    }
    if !m.session_id.is_empty() {
        a.push(s("gen_ai.conversation.id", &m.session_id));
    }
    if let Some(h) = &m.server_address {
        a.push(s("server.address", h));
    }
    if let Some(p) = m.server_port {
        a.push(kv("server.port", AV::IntValue(p as i64)));
    }
    if let Some(e) = &m.error_type {
        a.push(s("error.type", e));
    }
    if provider == "openai" {
        match ev.surface {
            ApiSurface::Responses => a.push(s("openai.api.type", "responses")),
            ApiSurface::Completions => {}
            _ => a.push(s("openai.api.type", "chat_completions")),
        }
    }
    a.push(s("obleth.api.surface", ev.surface.as_str()));
    a.push(s("obleth.request_id", &m.request_id));
    a.push(s("obleth.tenant_id", &m.tenant_id));
    a.push(s("obleth.key_id", &m.key_id));
    if !m.session_source.is_empty() {
        a.push(s("obleth.session.source", &m.session_source));
    }
    a.push(s("obleth.routed_model", &m.routed_model));
    a.push(s("obleth.model_variant", &m.model_variant));
    a.push(s("obleth.cache_status", &m.cache_status));
    a.push(kv("obleth.tool_loop.turns", AV::IntValue(m.tool_loop_turns as i64)));
    if let Some(t) = &m.ops_trace_id {
        a.push(s("obleth.ops.trace_id", t));
    }
    if let Some(t) = &m.ops_span_id {
        a.push(s("obleth.ops.span_id", t));
    }
    a.push(s("obleth.content.scrubbed", scrubbed));
    for (k, on) in flags {
        if *on {
            a.push(kv(k, AV::BoolValue(true)));
        }
    }
    if let Some(d) = ev.dropped {
        a.push(s("obleth.content.dropped", d));
    }
    if let Some(c) = content {
        let enc = |v: &[Value]| serde_json::to_string(v).unwrap_or_default();
        if !c.system.is_empty() {
            a.push(s("gen_ai.system_instructions", &enc(&c.system)));
        }
        if !c.input.is_empty() {
            a.push(s("gen_ai.input.messages", &enc(&c.input)));
        }
        if !c.output.is_empty() {
            a.push(s("gen_ai.output.messages", &enc(&c.output)));
        }
        if let Some(u) = &c.user {
            a.push(s("user.id", u));
        }
    }
    let status = m.error_type.as_ref().map(|e| Status {
        message: e.clone(),
        code: StatusCode::Error as i32,
    });
    Span {
        trace_id: rand::random::<[u8; 16]>().to_vec(),
        span_id: rand::random::<[u8; 8]>().to_vec(),
        name: format!("{op} {}", m.client_model),
        kind: SpanKind::Client as i32,
        start_time_unix_nano: ev.start_unix_nanos,
        end_time_unix_nano: ev.end_unix_nanos,
        attributes: a,
        status,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genai_export::{EventMeta, OutputChoice, ToolCallOut};

    fn text_of(c: &Content) -> String {
        serde_json::to_string(&c.input).unwrap()
    }

    #[test]
    fn chat_example() {
        let (p, c) = parse_request(
            ApiSurface::Chat,
            br#"{"model":"m","messages":[{"role":"system","content":"S"},{"role":"user","content":"hi"}],"temperature":0.2,"user":"u1"}"#,
        )
        .unwrap();
        assert_eq!(p.temperature, Some(0.2));
        assert_eq!(c.system, vec![json!({"type":"text","content":"S"})]);
        assert_eq!(
            c.input,
            vec![json!({"role":"user","parts":[{"type":"text","content":"hi"}]})]
        );
        assert_eq!(c.user.as_deref(), Some("u1"));
    }

    #[test]
    fn completions_example() {
        let (_, c) = parse_request(ApiSurface::Completions, br#"{"model":"m","prompt":"p"}"#).unwrap();
        assert_eq!(
            c.input,
            vec![json!({"role":"user","parts":[{"type":"text","content":"p"}]})]
        );
    }

    #[test]
    fn output_example() {
        let out = CollectedOutput {
            choices: vec![OutputChoice {
                role: "assistant".into(),
                text: "ok".into(),
                finish_reason: Some("tool_calls".into()),
                tool_calls: vec![ToolCallOut {
                    id: "c1".into(),
                    name: "f".into(),
                    arguments: "{\"a\":1}".into(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            output_messages(&out),
            vec![json!({"role":"assistant","parts":[{"type":"text","content":"ok"},
                {"type":"tool_call","id":"c1","name":"f","arguments":{"a":1}}],"finish_reason":"tool_call"})]
        );
    }

    #[test]
    fn blob_has_no_content() {
        let (_, c) = parse_request(
            ApiSurface::Chat,
            br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}]}"#,
        )
        .unwrap();
        let t = text_of(&c);
        assert!(t.contains(r#""mime_type":"image/png""#));
        assert!(!t.contains("AAAA"));
        assert!(!t.contains("content"));
    }

    #[test]
    fn uri_keeps_only_host() {
        let (_, c) = parse_request(
            ApiSurface::Chat,
            br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://x.io/a?sig=1"}}]}]}"#,
        )
        .unwrap();
        assert_eq!(c.input[0]["parts"][0]["uri"], "https://x.io");
    }

    #[test]
    fn url_and_mime_free_text_not_leaked() {
        let (_, c) = parse_request(
            ApiSurface::Chat,
            br#"{"messages":[{"role":"user","content":[
              {"type":"image_url","image_url":{"url":"data:bob@x.io;base64,AA"}},
              {"type":"image_url","image_url":{"url":"John Smith 555-1234://h"}},
              {"type":"image_url","image_url":{"url":"https://John Smith/a"}}]}]}"#,
        )
        .unwrap();
        let t = text_of(&c);
        assert!(!t.contains("bob") && !t.contains("John") && !t.contains("555"));
        assert_eq!(c.input[0]["parts"][0]["mime_type"], "unknown");
        assert_eq!(c.input[0]["parts"][1]["uri"], "unknown");
        assert_eq!(c.input[0]["parts"][2]["uri"], "unknown");
    }

    #[test]
    fn messages_top_k() {
        let (p, _) = parse_request(ApiSurface::Messages, br#"{"messages":[],"top_k":5}"#).unwrap();
        assert_eq!(p.top_k, Some(5.0));
    }

    #[test]
    fn n2_two_outputs() {
        let out = CollectedOutput {
            choices: vec![OutputChoice::default(), OutputChoice { index: 1, ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(output_messages(&out).len(), 2);
    }

    #[test]
    fn truncate_keeps_structure() {
        let big = "a".repeat(200 * 1024);
        let mut c = Content {
            input: vec![
                json!({"role":"user","parts":[{"type":"text","content":big.clone()}]}),
                json!({"role":"user","parts":[{"type":"text","content":big}]}),
            ],
            ..Default::default()
        };
        let r = truncate_content(&mut c, 256 * 1024);
        assert_eq!(r, (true, false));
        let s = serde_json::to_string(&c.input).unwrap();
        assert!(s.len() <= 256 * 1024);
        assert!(serde_json::from_str::<Value>(&s).unwrap().is_array());
    }

    #[test]
    fn truncate_multibyte_and_drop_oldest() {
        let mut c = Content {
            input: (0..50)
                .map(|_| json!({"role":"user","parts":[{"type":"text","content":"é".repeat(50)}]}))
                .collect(),
            ..Default::default()
        };
        let r = truncate_content(&mut c, 500);
        assert!(r.0);
        assert!(json_len(&c.input) <= 500);
    }

    fn ev() -> GenAiEvent {
        GenAiEvent {
            meta: EventMeta { client_model: "gpt".into(), provider: "".into(), ..Default::default() },
            ..Default::default()
        }
    }

    fn find<'a>(sp: &'a Span, k: &str) -> Option<&'a AV> {
        sp.attributes
            .iter()
            .find(|x| x.key == k)
            .and_then(|x| x.value.as_ref())
            .and_then(|v| v.value.as_ref())
    }

    #[test]
    fn span_root_and_attrs() {
        let sp = build_span(&ev(), Some(&RequestParams::default()), None, "regex", &[]);
        assert!(sp.parent_span_id.is_empty());
        assert_eq!(sp.trace_id.len(), 16);
        assert_eq!(sp.span_id.len(), 8);
        assert_eq!(sp.name, "chat gpt");
        assert!(matches!(find(&sp, "gen_ai.provider.name"), Some(AV::StringValue(v)) if v == "openai"));
        assert!(find(&sp, "gen_ai.request.stream").is_none());
        assert!(sp.status.is_none());
    }

    #[test]
    fn encoding_escapes_content() {
        let c = Content {
            input: vec![json!({"role":"user","parts":[{"type":"text","content":"a\"b\n\u{1F600}\\"}]})],
            ..Default::default()
        };
        let sp = build_span(&ev(), None, Some(&c), "none", &[("obleth.content.input_truncated", true)]);
        let Some(AV::StringValue(s)) = find(&sp, "gen_ai.input.messages") else { panic!() };
        let back: Value = serde_json::from_str(s).unwrap();
        assert_eq!(back[0]["parts"][0]["content"], "a\"b\n\u{1F600}\\");
        assert!(find(&sp, "obleth.content.input_truncated").is_some());
    }

    #[test]
    fn non_utf8_body_is_err() {
        assert!(parse_request(ApiSurface::Chat, b"{\"model\":\"\xff\xfe\"}").is_err());
    }

    #[test]
    fn deep_arguments_ok() {
        let mut args = String::from("1");
        for _ in 0..64 {
            args = format!("{{\"a\":{args}}}");
        }
        let body = json!({"messages":[{"role":"assistant","content":null,
            "tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":args}}]}]});
        let (_, c) = parse_request(ApiSurface::Chat, body.to_string().as_bytes()).unwrap();
        assert!(c.input[0]["parts"][0]["arguments"].is_object());
        let out = CollectedOutput {
            choices: vec![OutputChoice {
                tool_calls: vec![ToolCallOut { id: "c".into(), name: "f".into(), arguments: args }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let m = output_messages(&out);
        assert!(m[0]["parts"][0]["arguments"].is_object());
        let sp = build_span(&ev(), None, Some(&Content { output: m, ..Default::default() }), "none", &[]);
        assert!(find(&sp, "gen_ai.output.messages").is_some());
    }

    #[test]
    fn error_status_and_ttft() {
        let mut e = ev();
        e.meta.error_type = Some("timeout".into());
        e.start_unix_nanos = 1_000_000_000;
        e.ttft_unix_nanos = Some(2_500_000_000);
        let sp = build_span(&e, None, None, "none", &[]);
        assert_eq!(sp.status.as_ref().unwrap().code, StatusCode::Error as i32);
        assert!(matches!(find(&sp, "gen_ai.response.time_to_first_chunk"), Some(AV::DoubleValue(v)) if (*v - 1.5).abs() < 1e-9));
        assert!(matches!(find(&sp, "langfuse.observation.completion_start_time"), Some(AV::StringValue(v)) if v.starts_with("1970-01-01T00:00:02.5")));
    }
}
