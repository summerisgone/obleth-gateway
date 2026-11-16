//! Content scrubbing for the GenAI export.
//!
//! Scrubbed text never goes to `tracing` and never into error strings: every
//! error here is a short static description.

use std::sync::Arc;

use obleth_config::{PiiDetectorKind, TraceProfile};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::GenAiConfig;
use super::Content;
use crate::boons::guardrails::scan_pii;
use crate::pii::{Lane, PiiDetector, PiiError};

const LLM_SYSTEM_PROMPT: &str = "You redact personal data. You receive a JSON array of strings. \
Replace every name, address, phone number, e-mail, identifier and secret with a [REDACTED:TYPE] tag. \
Reply with only a JSON array of strings of the same length, in the same order, with no other text.";

pub struct Scrubber {
    cfg: Arc<GenAiConfig>,
    http: reqwest::Client,
    detector: Arc<PiiDetector>,
}

impl Scrubber {
    pub fn new(
        cfg: Arc<GenAiConfig>,
        http: reqwest::Client,
        detector: Arc<PiiDetector>,
    ) -> Scrubber {
        Scrubber {
            cfg,
            http,
            detector,
        }
    }

    /// Returns the scrub marker applied. Any Err means the caller must drop the content.
    pub async fn scrub(&self, c: &mut Content, tp: &TraceProfile) -> Result<&'static str, String> {
        let profile = tp.profile.as_ref().ok_or("unknown profile")?;
        let mut texts = self
            .detector
            .redact(&collect(c), profile, Lane::Traces)
            .await
            .map_err(|e| {
                match e {
                    PiiError::NotConfigured => "ner not configured",
                    PiiError::Busy => "busy",
                    PiiError::Timeout => "scrub timeout",
                    PiiError::Http(_) => "ner request failed",
                    PiiError::BadReply(_) => "ner bad reply",
                }
                .to_string()
            })?;
        let mut label = match profile.detector {
            PiiDetectorKind::Off => "none",
            PiiDetectorKind::Regex => "regex",
            PiiDetectorKind::Ner => "ner",
        };
        if tp.name.split('+').next() == Some("llm") {
            texts = tokio::time::timeout(self.cfg.scrub_timeout, self.call_llm(&texts))
                .await
                .map_err(|_| "scrub timeout".to_string())??;
            label = "llm";
        }
        apply(c, texts);
        Ok(label)
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
        let parsed: Value =
            serde_json::from_str(content.trim()).map_err(|_| "llm bad reply".to_string())?;
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
    for v in c
        .system
        .iter_mut()
        .chain(c.input.iter_mut())
        .chain(c.output.iter_mut())
    {
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
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match ty.as_str() {
        "text" | "reasoning" => {
            if let Some(Value::String(s)) = obj.get_mut("content") {
                f(s);
            }
        }
        "tool_call_response" => {
            if let Some(Value::String(s)) = obj.get_mut("id") {
                f(s);
            }
            if let Some(r) = obj.get_mut("response") {
                visit_json(r, f);
            }
        }
        "tool_call" => {
            for k in ["id", "name"] {
                if let Some(Value::String(s)) = obj.get_mut(k) {
                    f(s);
                }
            }
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
    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    use crate::boons::BoonEngine;
    use crate::pii::PiiEnv;
    use obleth_config::{
        effective_trace_profile, BoonSettings, GuardrailsAction, GuardrailsPolicy, PiiProfile,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn cfg(url: Option<String>) -> GenAiConfig {
        GenAiConfig {
            traces_url: String::new(),
            headers: vec![],
            content_ttl: Duration::from_secs(86400),
            queue_max_bytes: 1 << 26,
            flush: Duration::from_millis(2000),
            max_request_bytes: 4 << 20,
            max_content_bytes: 262144,
            scrub_default: "regex".into(),
            scrub_url: url,
            scrub_model: Some("m".into()),
            scrub_timeout: Duration::from_millis(300),
            scrub_concurrency: 8,
            user_id_hmac_key: None,
            conversation_sources: vec!["client".into()],
        }
    }

    fn detector_with(
        url: Option<String>,
        conc: usize,
        settings: BoonSettings,
    ) -> (Arc<PiiDetector>, Arc<arc_swap::ArcSwap<BoonSettings>>) {
        let h = Arc::new(arc_swap::ArcSwap::from_pointee(settings));
        let env = PiiEnv {
            ner_url: url,
            concurrency: conc,
            scrub_timeout_ms: Some(300),
            warnings: vec![],
        };
        (
            Arc::new(PiiDetector::new(&env, reqwest::Client::new(), h.clone())),
            h,
        )
    }

    fn scrubber_url(c: GenAiConfig, ner: Option<String>) -> Scrubber {
        let (d, _) = detector_with(ner, 8, BoonSettings::default());
        Scrubber::new(Arc::new(c), reqwest::Client::new(), d)
    }

    fn scrubber(c: GenAiConfig) -> Scrubber {
        let u = c.scrub_url.clone();
        scrubber_url(c, u)
    }

    fn tp(name: &str, kind: PiiDetectorKind) -> TraceProfile {
        TraceProfile {
            name: name.into(),
            profile: Some(PiiProfile {
                detector: kind,
                entities: vec![],
                language: None,
                score_threshold: None,
            }),
            strengthened: false,
        }
    }

    fn regex_tp() -> TraceProfile {
        tp("regex", PiiDetectorKind::Regex)
    }

    fn text_content(t: &str) -> Content {
        Content {
            input: vec![json!({"role":"user","parts":[{"type":"text","content":t}]})],
            ..Default::default()
        }
    }

    fn first_text(c: &Content) -> String {
        c.input[0]["parts"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn serve(app: axum::Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn counting_ner(hits: Arc<AtomicUsize>, delay_ms: u64) -> String {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
                hits.fetch_add(1, Ordering::SeqCst);
                async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    axum::Json(v)
                }
            }),
        );
        format!("{}/", serve(app).await)
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
        let s = scrubber(cfg(None));
        let mut c = text_content("mail a@b.io");
        assert_eq!(s.scrub(&mut c, &regex_tp()).await, Ok("regex"));
        assert_eq!(first_text(&c), "mail [REDACTED:EMAIL]");
    }

    #[tokio::test]
    async fn off_leaves_text() {
        let s = scrubber(cfg(None));
        let mut c = text_content("mail a@b.io");
        assert_eq!(
            s.scrub(&mut c, &tp("off", PiiDetectorKind::Off)).await,
            Ok("none")
        );
        assert_eq!(first_text(&c), "mail a@b.io");
    }

    #[tokio::test]
    async fn unknown_profile_scrub_failed() {
        let s = scrubber(cfg(None));
        let mut c = text_content("mail a@b.io");
        let gone = TraceProfile {
            name: "gone".into(),
            profile: None,
            strengthened: false,
        };
        assert!(s.scrub(&mut c, &gone).await.is_err());
        assert_eq!(first_text(&c), "mail a@b.io");
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
        let s = scrubber(cfg(Some(format!("{url}/"))));
        let mut c = text_content("hello a@b.io");
        c.system = vec![json!({"type":"text","content":"sys"})];
        assert_eq!(
            s.scrub(&mut c, &tp("ner", PiiDetectorKind::Ner)).await,
            Ok("ner")
        );
        assert_eq!(first_text(&c), "HELLO [REDACTED:EMAIL]");
        assert_eq!(c.system[0]["content"], "SYS");
    }

    #[tokio::test]
    async fn ner_without_url_fails() {
        let s = scrubber(cfg(None));
        let mut c = text_content("x");
        assert!(s
            .scrub(&mut c, &tp("ner", PiiDetectorKind::Ner))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn ner_wrong_len_fails() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async { axum::Json(json!({"texts": []})) }),
        );
        let url = serve(app).await;
        let s = scrubber(cfg(Some(format!("{url}/"))));
        let mut c = text_content("secret words");
        let r = s.scrub(&mut c, &tp("ner", PiiDetectorKind::Ner)).await;
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
        let s = scrubber(cfg(Some(format!("{url}/"))));
        let mut c = text_content("x");
        assert!(s
            .scrub(&mut c, &tp("ner", PiiDetectorKind::Ner))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn busy_when_no_permit() {
        let url = counting_ner(Arc::new(AtomicUsize::new(0)), 150).await;
        let (d, _) = detector_with(Some(url), 1, BoonSettings::default());
        let s = Arc::new(Scrubber::new(
            Arc::new(cfg(None)),
            reqwest::Client::new(),
            d,
        ));
        let t = tp("ner", PiiDetectorKind::Ner);
        let s2 = s.clone();
        let t2 = t.clone();
        let first = tokio::spawn(async move {
            let mut c = text_content("x");
            s2.scrub(&mut c, &t2).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut c = text_content("x");
        assert_eq!(s.scrub(&mut c, &t).await, Err("busy".to_string()));
        assert_eq!(first.await.unwrap(), Ok("ner"));
    }

    #[tokio::test]
    async fn no_restart_traces_and_tenant_override() {
        let hits = Arc::new(AtomicUsize::new(0));
        let url = counting_ner(hits.clone(), 0).await;
        let engine = BoonEngine::new(BoonSettings::default());
        let det = Arc::new(PiiDetector::new(
            &PiiEnv {
                ner_url: Some(url),
                concurrency: 4,
                scrub_timeout_ms: None,
                warnings: vec![],
            },
            reqwest::Client::new(),
            engine.settings_handle(),
        ));
        let sc = Scrubber::new(Arc::new(cfg(None)), reqwest::Client::new(), det);
        let resolve = |tenant: Option<&str>, policy: Option<&GuardrailsPolicy>| {
            effective_trace_profile(&engine.settings().pii, tenant, "regex", policy)
        };

        let mut c = text_content("a@b.io");
        assert_eq!(sc.scrub(&mut c, &resolve(None, None)).await, Ok("regex"));
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        let mut b = BoonSettings::default();
        b.pii.traces = Some("ner".into());
        engine.update(b);
        let mut c = text_content("a@b.io");
        assert_eq!(sc.scrub(&mut c, &resolve(None, None)).await, Ok("ner"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let mut c = text_content("a@b.io");
        assert_eq!(
            sc.scrub(&mut c, &resolve(Some("off"), None)).await,
            Ok("none")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(first_text(&c), "a@b.io");

        let policy = GuardrailsPolicy {
            action: GuardrailsAction::Redact,
            input_scanners: vec!["pii".into()],
            output_scanners: vec![],
            guard_model: None,
            ban_keywords: vec![],
            fail_open: true,
            pii_profile: None,
        };
        let mut c = text_content("a@b.io");
        let strengthened = resolve(Some("off"), Some(&policy));
        assert_eq!(strengthened.name, "off+regex");
        assert_eq!(sc.scrub(&mut c, &strengthened).await, Ok("regex"));
        assert_eq!(first_text(&c), "[REDACTED:EMAIL]");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let mut c = text_content("a@b.io");
        assert_eq!(
            sc.scrub(&mut c, &resolve(Some("ner"), None)).await,
            Ok("ner")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn llm_legacy_runs_after_detector() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
                h.fetch_add(1, Ordering::SeqCst);
                async move {
                    let seen = v["messages"][1]["content"].as_str().unwrap().to_string();
                    assert!(seen.contains("[REDACTED:EMAIL]") && !seen.contains("a@b.io"));
                    let out = serde_json::to_string(&["hi [REDACTED:EMAIL]"]).unwrap();
                    axum::Json(json!({"choices":[{"message":{"content": out}}]}))
                }
            }),
        );
        let url = serve(app).await;
        let s = scrubber(cfg(Some(url)));
        let mut c = text_content("hi a@b.io");
        assert_eq!(
            s.scrub(&mut c, &tp("llm", PiiDetectorKind::Regex)).await,
            Ok("llm")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(first_text(&c), "hi [REDACTED:EMAIL]");
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
        let s = scrubber(cfg(Some(url)));
        let mut c = text_content("abcdefghij");
        assert!(s
            .scrub(&mut c, &tp("llm", PiiDetectorKind::Regex))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn llm_ok_same_length() {
        let url = llm_server(10).await;
        let s = scrubber(cfg(Some(url)));
        let mut c = text_content("abcdefghij");
        assert_eq!(
            s.scrub(&mut c, &tp("llm", PiiDetectorKind::Regex)).await,
            Ok("llm")
        );
    }

    #[tokio::test]
    async fn arguments_leaves_scrubbed() {
        let s = scrubber(cfg(None));
        let mut c = Content {
            output: vec![json!({"role":"assistant","name":"bob@x.io","parts":[
                {"type":"tool_call","id":"1","name":"f","arguments":{"to":"a@b.io","n":1}},
                {"type":"tool_call_response","id":"1","response":"call 123-45-6789"},
                {"type":"reasoning","content":"c@d.io"}
            ]})],
            ..Default::default()
        };
        s.scrub(&mut c, &regex_tp()).await.unwrap();
        let m = &c.output[0];
        assert_eq!(m["name"], "[REDACTED:EMAIL]");
        assert_eq!(m["parts"][0]["arguments"]["to"], "[REDACTED:EMAIL]");
        assert_eq!(m["parts"][0]["arguments"]["n"], 1);
        assert_eq!(m["parts"][1]["response"], "call [REDACTED:SSN]");
        assert_eq!(m["parts"][2]["content"], "[REDACTED:EMAIL]");
    }

    #[tokio::test]
    async fn tool_ids_and_names_scrubbed() {
        let s = scrubber(cfg(None));
        let mut c = Content {
            input: vec![
                json!({"role":"tool","parts":[{"type":"tool_call_response","id":"a@b.io","response":"x"}]}),
            ],
            output: vec![
                json!({"role":"assistant","parts":[{"type":"tool_call","id":"c@d.io","name":"e@f.io","arguments":{}}]}),
            ],
            ..Default::default()
        };
        s.scrub(&mut c, &regex_tp()).await.unwrap();
        let t = serde_json::to_string(&(&c.input, &c.output)).unwrap();
        assert!(!t.contains('@'), "{t}");
    }

    #[tokio::test]
    async fn unicode_and_quotes_survive() {
        let s = scrubber(cfg(None));
        let mut c = text_content("\"привет\" \u{1F600} \\ \n a@b.io");
        s.scrub(&mut c, &regex_tp()).await.unwrap();
        assert_eq!(
            first_text(&c),
            "\"привет\" \u{1F600} \\ \n [REDACTED:EMAIL]"
        );
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
