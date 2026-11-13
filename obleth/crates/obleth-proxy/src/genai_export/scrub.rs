//! Content scrubbing for the GenAI export.
//!
//! Scrubbed text never goes to `tracing` and never into error strings: every
//! error here is a short static description.

use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::config::{GenAiConfig, ScrubMode};
use super::Content;
use crate::boons::guardrails::scan_pii;

const LLM_SYSTEM_PROMPT: &str = "You redact personal data. You receive a JSON array of strings. \
Replace every name, address, phone number, e-mail, identifier and secret with a [REDACTED:TYPE] tag. \
Reply with only a JSON array of strings of the same length, in the same order, with no other text.";

pub struct Scrubber {
    cfg: Arc<GenAiConfig>,
    http: reqwest::Client,
    sem: Semaphore,
}

impl Scrubber {
    pub fn new(cfg: Arc<GenAiConfig>, http: reqwest::Client) -> Scrubber {
        let sem = Semaphore::new(cfg.scrub_concurrency.max(1));
        Scrubber { cfg, http, sem }
    }

    /// Returns the scrub mode label applied. Any Err means the caller must drop the content.
    pub async fn scrub(&self, c: &mut Content, force_regex: bool) -> Result<&'static str, String> {
        let mode = self.cfg.scrub;
        if mode == ScrubMode::Off && !force_regex {
            return Ok("none");
        }
        let mut texts = collect(c);
        for t in texts.iter_mut() {
            *t = scan_pii(t).1;
        }
        let label = match mode {
            ScrubMode::Off | ScrubMode::Regex => "regex",
            ScrubMode::Ner => "ner",
            ScrubMode::Llm => "llm",
        };
        if label == "ner" || label == "llm" {
            let _permit = self.sem.try_acquire().map_err(|_| "busy".to_string())?;
            let fut = async {
                if label == "ner" {
                    self.call_ner(&texts).await
                } else {
                    self.call_llm(&texts).await
                }
            };
            texts = tokio::time::timeout(self.cfg.scrub_timeout, fut)
                .await
                .map_err(|_| "scrub timeout".to_string())??;
        }
        apply(c, texts);
        Ok(label)
    }

    async fn call_ner(&self, texts: &[String]) -> Result<Vec<String>, String> {
        let url = self.cfg.scrub_url.as_deref().ok_or("scrub url missing")?;
        let resp = self
            .http
            .post(url)
            .json(&serde_json::json!({ "texts": texts }))
            .send()
            .await
            .map_err(|_| "ner request failed".to_string())?;
        if !resp.status().is_success() {
            return Err("ner status".into());
        }
        let v: Value = resp.json().await.map_err(|_| "ner bad reply".to_string())?;
        let out = string_array(v.get("texts")).ok_or("ner bad reply")?;
        if out.len() != texts.len() {
            return Err("ner length mismatch".into());
        }
        Ok(out)
    }

    async fn call_llm(&self, texts: &[String]) -> Result<Vec<String>, String> {
        let base = self.cfg.scrub_url.as_deref().ok_or("scrub url missing")?;
        let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
        let model = self.cfg.scrub_model.clone().unwrap_or_default();
        let user = serde_json::to_string(texts).map_err(|_| "llm encode".to_string())?;
        let body = serde_json::json!({
            "model": model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": LLM_SYSTEM_PROMPT},
                {"role": "user", "content": user},
            ],
        });
        let resp = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|_| "llm request failed".to_string())?;
        if !resp.status().is_success() {
            return Err("llm status".into());
        }
        let v: Value = resp.json().await.map_err(|_| "llm bad reply".to_string())?;
        let content = v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or("llm bad reply")?;
        let parsed: Value = serde_json::from_str(content.trim()).map_err(|_| "llm bad reply".to_string())?;
        let out = string_array(Some(&parsed)).ok_or("llm bad reply")?;
        if out.len() != texts.len() {
            return Err("llm length mismatch".into());
        }
        let in_len: usize = texts.iter().map(String::len).sum();
        let out_len: usize = out.iter().map(String::len).sum();
        if out_len as f64 > in_len as f64 * 1.2 {
            return Err("llm reply too long".into());
        }
        Ok(out)
    }
}

fn string_array(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_str().map(str::to_owned))
        .collect()
}

/// Visits every scrubbable string leaf in a fixed order.
fn walk(c: &mut Content, f: &mut dyn FnMut(&mut String)) {
    for v in c.system.iter_mut().chain(c.input.iter_mut()).chain(c.output.iter_mut()) {
        visit_node(v, f);
    }
}

fn visit_node(v: &mut Value, f: &mut dyn FnMut(&mut String)) {
    let Some(obj) = v.as_object_mut() else { return };
    if let Some(Value::String(s)) = obj.get_mut("name") {
        f(s);
    }
    if let Some(Value::Array(parts)) = obj.get_mut("parts") {
        for p in parts.iter_mut() {
            visit_part(p, f);
        }
    } else {
        visit_part(v, f);
    }
}

fn visit_part(v: &mut Value, f: &mut dyn FnMut(&mut String)) {
    let Some(obj) = v.as_object_mut() else { return };
    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("").to_owned();
    match ty.as_str() {
        "text" | "reasoning" => {
            if let Some(Value::String(s)) = obj.get_mut("content") {
                f(s);
            }
        }
        "tool_call_response" => {
            if let Some(r) = obj.get_mut("response") {
                visit_json(r, f);
            }
        }
        "tool_call" => {
            if let Some(a) = obj.get_mut("arguments") {
                visit_json(a, f);
            }
        }
        _ => {}
    }
}

/// String leaves of arbitrary JSON (object keys are left alone).
fn visit_json(v: &mut Value, f: &mut dyn FnMut(&mut String)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| visit_json(x, f)),
        Value::Object(o) => o.values_mut().for_each(|x| visit_json(x, f)),
        _ => {}
    }
}

fn collect(c: &mut Content) -> Vec<String> {
    let mut out = Vec::new();
    walk(c, &mut |s| out.push(s.clone()));
    out
}

fn apply(c: &mut Content, texts: Vec<String>) {
    let mut it = texts.into_iter();
    walk(c, &mut |s| {
        if let Some(t) = it.next() {
            *s = t;
        }
    });
}

pub fn hmac_hex(key: &[u8], msg: &str) -> String {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg.as_bytes());
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(ih);
    outer.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn conversation_id(raw: &str, key: Option<&[u8]>) -> Option<String> {
    if scan_pii(raw).0 {
        key.map(|k| hmac_hex(k, raw))
    } else {
        Some(raw.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn cfg(mode: ScrubMode, url: Option<String>) -> GenAiConfig {
        GenAiConfig {
            traces_url: String::new(),
            headers: vec![],
            content_ttl: Duration::from_secs(86400),
            queue_max_bytes: 1 << 26,
            flush: Duration::from_millis(2000),
            max_request_bytes: 4 << 20,
            max_content_bytes: 262144,
            scrub: mode,
            scrub_url: url,
            scrub_model: Some("m".into()),
            scrub_timeout: Duration::from_millis(300),
            scrub_concurrency: 8,
            user_id_hmac_key: None,
            conversation_sources: vec!["client".into()],
        }
    }

    // Fake: always the local struct literal; the real GenAiConfig is the same type.
    fn scrubber(c: GenAiConfig) -> Scrubber {
        Scrubber::new(Arc::new(c), reqwest::Client::new())
    }

    fn text_content(t: &str) -> Content {
        Content {
            input: vec![json!({"role":"user","parts":[{"type":"text","content":t}]})],
            ..Default::default()
        }
    }

    fn first_text(c: &Content) -> String {
        c.input[0]["parts"][0]["content"].as_str().unwrap().to_string()
    }

    async fn serve(app: axum::Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[test]
    fn hmac_known_vector() {
        assert_eq!(
            hmac_hex(b"key", "The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        // key longer than the block size is hashed first
        assert_eq!(hmac_hex(&[7u8; 100], "x").len(), 64);
    }

    #[tokio::test]
    async fn regex_redacts_email() {
        let s = scrubber(cfg(ScrubMode::Regex, None));
        let mut c = text_content("mail a@b.io");
        assert_eq!(s.scrub(&mut c, false).await, Ok("regex"));
        assert_eq!(first_text(&c), "mail [REDACTED:EMAIL]");
    }

    #[tokio::test]
    async fn off_and_force_regex() {
        let s = scrubber(cfg(ScrubMode::Off, None));
        let mut c = text_content("mail a@b.io");
        assert_eq!(s.scrub(&mut c, false).await, Ok("none"));
        assert_eq!(first_text(&c), "mail a@b.io");
        assert_eq!(s.scrub(&mut c, true).await, Ok("regex"));
        assert_eq!(first_text(&c), "mail [REDACTED:EMAIL]");
    }

    #[tokio::test]
    async fn ner_applies_texts() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|axum::Json(v): axum::Json<Value>| async move {
                let t: Vec<String> = v["texts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_str().unwrap().to_uppercase())
                    .collect();
                axum::Json(json!({ "texts": t }))
            }),
        );
        let url = serve(app).await;
        let s = scrubber(cfg(ScrubMode::Ner, Some(format!("{url}/"))));
        let mut c = text_content("hello a@b.io");
        c.system = vec![json!({"type":"text","content":"sys"})];
        assert_eq!(s.scrub(&mut c, false).await, Ok("ner"));
        assert_eq!(first_text(&c), "HELLO [REDACTED:EMAIL]");
        assert_eq!(c.system[0]["content"], "SYS");
    }

    #[tokio::test]
    async fn force_regex_keeps_ner() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { axum::Json(v) }
            }),
        );
        let url = serve(app).await;
        let s = scrubber(cfg(ScrubMode::Ner, Some(format!("{url}/"))));
        let mut c = text_content("hi a@b.io");
        assert_eq!(s.scrub(&mut c, true).await, Ok("ner"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(first_text(&c), "hi [REDACTED:EMAIL]");
    }

    #[tokio::test]
    async fn ner_wrong_len_fails() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async { axum::Json(json!({"texts": []})) }),
        );
        let url = serve(app).await;
        let s = scrubber(cfg(ScrubMode::Ner, Some(format!("{url}/"))));
        let mut c = text_content("secret words");
        let r = s.scrub(&mut c, false).await;
        assert!(r.is_err());
        assert!(!r.unwrap_err().contains("secret"));
    }

    #[tokio::test]
    async fn ner_timeout_fails() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                axum::Json(json!({"texts": ["x"]}))
            }),
        );
        let url = serve(app).await;
        let s = scrubber(cfg(ScrubMode::Ner, Some(format!("{url}/"))));
        let mut c = text_content("x");
        assert!(s.scrub(&mut c, false).await.is_err());
    }

    #[tokio::test]
    async fn busy_when_no_permit() {
        let mut cf = cfg(ScrubMode::Ner, Some("http://127.0.0.1:1/".into()));
        cf.scrub_concurrency = 1;
        let s = scrubber(cf);
        let _held = s.sem.try_acquire().unwrap();
        let mut c = text_content("x");
        assert_eq!(s.scrub(&mut c, false).await, Err("busy".to_string()));
    }

    async fn llm_server(factor_num: usize) -> String {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| async move {
                assert_eq!(v["model"], "m");
                let texts: Vec<String> =
                    serde_json::from_str(v["messages"][1]["content"].as_str().unwrap()).unwrap();
                let out: Vec<String> = texts
                    .iter()
                    .map(|t| t.repeat(1).chars().cycle().take(t.len() * factor_num / 10).collect())
                    .collect();
                axum::Json(json!({"choices":[{"message":{"content": serde_json::to_string(&out).unwrap()}}]}))
            }),
        );
        serve(app).await
    }

    #[tokio::test]
    async fn llm_too_long_fails() {
        let url = llm_server(13).await;
        let s = scrubber(cfg(ScrubMode::Llm, Some(url)));
        let mut c = text_content("abcdefghij");
        assert!(s.scrub(&mut c, false).await.is_err());
    }

    #[tokio::test]
    async fn llm_ok_same_length() {
        let url = llm_server(10).await;
        let s = scrubber(cfg(ScrubMode::Llm, Some(url)));
        let mut c = text_content("abcdefghij");
        assert_eq!(s.scrub(&mut c, false).await, Ok("llm"));
    }

    #[tokio::test]
    async fn arguments_leaves_scrubbed() {
        let s = scrubber(cfg(ScrubMode::Regex, None));
        let mut c = Content {
            output: vec![json!({"role":"assistant","name":"bob@x.io","parts":[
                {"type":"tool_call","id":"1","name":"f","arguments":{"to":"a@b.io","n":1}},
                {"type":"tool_call_response","id":"1","response":"call 123-45-6789"},
                {"type":"reasoning","content":"c@d.io"}
            ]})],
            ..Default::default()
        };
        s.scrub(&mut c, false).await.unwrap();
        let m = &c.output[0];
        assert_eq!(m["name"], "[REDACTED:EMAIL]");
        assert_eq!(m["parts"][0]["arguments"]["to"], "[REDACTED:EMAIL]");
        assert_eq!(m["parts"][0]["arguments"]["n"], 1);
        assert_eq!(m["parts"][1]["response"], "call [REDACTED:SSN]");
        assert_eq!(m["parts"][2]["content"], "[REDACTED:EMAIL]");
    }

    #[tokio::test]
    async fn unicode_and_quotes_survive() {
        let s = scrubber(cfg(ScrubMode::Regex, None));
        let mut c = text_content("\"привет\" \u{1F600} \\ \n a@b.io");
        s.scrub(&mut c, false).await.unwrap();
        assert_eq!(first_text(&c), "\"привет\" \u{1F600} \\ \n [REDACTED:EMAIL]");
    }

    #[test]
    fn conversation_id_pii_hashed() {
        let h = conversation_id("a@b.io", Some(b"k")).unwrap();
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(conversation_id("a@b.io", None), None);
        assert_eq!(conversation_id("sess-1", None), Some("sess-1".into()));
    }
}
