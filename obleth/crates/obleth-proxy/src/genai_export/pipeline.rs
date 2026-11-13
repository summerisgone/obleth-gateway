//! Worker pipeline for the GenAI export: event -> scrubbed, truncated span.
//!
//! Content never reaches `tracing` or error strings here.

use std::sync::Arc;

use opentelemetry_proto::tonic::trace::v1::Span;

use super::config::GenAiConfig;
use super::exporter::ProcessFn;
use super::mapping::{build_span, output_messages, parse_request, truncate_content};
use super::scrub::{conversation_id, hmac_hex, Scrubber};
use super::GenAiEvent;

pub async fn process(mut ev: GenAiEvent, cfg: Arc<GenAiConfig>, scrubber: Arc<Scrubber>) -> Span {
    let key = cfg.user_id_hmac_key.as_deref();

    // Session: PII-safe id, and "derived" only when enabled.
    if !ev.meta.session_id.is_empty() {
        let derived_off = ev.meta.session_source == "derived"
            && !cfg.conversation_sources.iter().any(|s| s == "derived");
        match (derived_off, conversation_id(&ev.meta.session_id, key)) {
            (false, Some(id)) => ev.meta.session_id = id,
            _ => {
                ev.meta.session_id.clear();
                ev.meta.session_source.clear();
            }
        }
    }

    let mut params = None;
    let mut content = None;
    let mut scrubbed = "none";
    let mut out_truncated = ev.output.as_ref().is_some_and(|o| o.truncated);
    let mut in_truncated = false;

    if ev.dropped.is_none() {
        if let Some(body) = ev.request_body.as_ref() {
            match parse_request(ev.surface, body) {
                Err(_) => ev.dropped = Some("parse_failed"),
                Ok((p, mut c)) => {
                    if let Some(o) = &ev.output {
                        c.output = output_messages(o);
                    }
                    c.user = match (c.user.take(), key) {
                        (Some(u), Some(k)) => Some(hmac_hex(k, &u)),
                        _ => None,
                    };
                    match scrubber.scrub(&mut c, ev.meta.pii_guardrail).await {
                        Err(_) => ev.dropped = Some("scrub_failed"),
                        Ok(mode) => {
                            scrubbed = mode;
                            let (i, o) = truncate_content(&mut c, cfg.max_content_bytes);
                            in_truncated = i;
                            out_truncated |= o;
                            params = Some(p);
                            content = Some(c);
                        }
                    }
                }
            }
        }
    }

    build_span(
        &ev,
        params.as_ref(),
        content.as_ref(),
        scrubbed,
        &[
            ("obleth.content.input_truncated", in_truncated),
            ("obleth.content.output_truncated", out_truncated && content.is_some()),
        ],
    )
}

pub fn process_fn(cfg: Arc<GenAiConfig>, scrubber: Arc<Scrubber>) -> ProcessFn {
    Arc::new(move |ev| {
        let (c, s) = (cfg.clone(), scrubber.clone());
        Box::pin(process(ev, c, s))
    })
}

#[cfg(test)]
mod tests {
    use super::super::config::ScrubMode;
    use super::super::{ApiSurface, CollectedOutput, OutputChoice};
    use super::*;
    use std::time::Duration;

    fn cfg(mode: ScrubMode, key: Option<&[u8]>, sources: &[&str]) -> Arc<GenAiConfig> {
        Arc::new(GenAiConfig {
            traces_url: "http://127.0.0.1:1/v1/traces".into(),
            headers: vec![],
            content_ttl: Duration::from_secs(60),
            queue_max_bytes: 1 << 20,
            flush: Duration::from_secs(1),
            max_request_bytes: 1 << 20,
            max_content_bytes: 1 << 18,
            scrub: mode,
            scrub_url: Some("http://127.0.0.1:1".into()),
            scrub_model: None,
            scrub_timeout: Duration::from_millis(300),
            scrub_concurrency: 2,
            user_id_hmac_key: key.map(|k| k.to_vec()),
            conversation_sources: sources.iter().map(|s| s.to_string()).collect(),
        })
    }

    fn event(body: &str) -> GenAiEvent {
        let mut ev = GenAiEvent {
            surface: ApiSurface::Chat,
            request_body: Some(bytes::Bytes::from(body.to_string())),
            ..Default::default()
        };
        ev.meta.client_model = "m".into();
        ev.output = Some(CollectedOutput {
            choices: vec![OutputChoice { role: "assistant".into(), text: "ok".into(), ..Default::default() }],
            ..Default::default()
        });
        ev
    }

    async fn run(ev: GenAiEvent, c: Arc<GenAiConfig>) -> Span {
        let sc = Arc::new(Scrubber::new(c.clone(), reqwest::Client::new()));
        process(ev, c, sc).await
    }

    fn attr(sp: &Span, k: &str) -> Option<String> {
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        sp.attributes.iter().find(|a| a.key == k).and_then(|a| match a.value.as_ref()?.value.as_ref()? {
            Value::StringValue(s) => Some(s.clone()),
            Value::BoolValue(b) => Some(b.to_string()),
            _ => Some(String::new()),
        })
    }

    const BODY: &str = r#"{"model":"m","messages":[{"role":"user","content":"mail a@b.io"}],"user":"u1"}"#;

    #[tokio::test]
    async fn pii_in_input_redacted_in_span() {
        let sp = run(event(BODY), cfg(ScrubMode::Regex, None, &["client"])).await;
        let inp = attr(&sp, "gen_ai.input.messages").unwrap();
        assert!(inp.contains("[REDACTED:EMAIL]") && !inp.contains("a@b.io"));
        assert!(attr(&sp, "gen_ai.output.messages").unwrap().contains("ok"));
        assert_eq!(attr(&sp, "obleth.content.scrubbed").as_deref(), Some("regex"));
    }

    #[tokio::test]
    async fn scrub_failure_drops_content() {
        let sp = run(event(BODY), cfg(ScrubMode::Ner, None, &[])).await;
        assert!(attr(&sp, "gen_ai.input.messages").is_none());
        assert!(attr(&sp, "gen_ai.output.messages").is_none());
        assert_eq!(attr(&sp, "obleth.content.dropped").as_deref(), Some("scrub_failed"));
        assert_eq!(attr(&sp, "obleth.content.scrubbed").as_deref(), Some("none"));
    }

    #[tokio::test]
    async fn parse_failure_marked() {
        let sp = run(event("not json"), cfg(ScrubMode::Regex, None, &[])).await;
        assert_eq!(attr(&sp, "obleth.content.dropped").as_deref(), Some("parse_failed"));
    }

    #[tokio::test]
    async fn user_id_needs_key() {
        let sp = run(event(BODY), cfg(ScrubMode::Regex, None, &[])).await;
        assert!(attr(&sp, "user.id").is_none());
        let sp = run(event(BODY), cfg(ScrubMode::Regex, Some(b"k"), &[])).await;
        assert_eq!(attr(&sp, "user.id").unwrap(), hmac_hex(b"k", "u1"));
    }

    #[tokio::test]
    async fn derived_session_off_by_default() {
        let mut ev = event(BODY);
        ev.meta.session_id = "sess-1".into();
        ev.meta.session_source = "derived".into();
        let sp = run(ev.clone(), cfg(ScrubMode::Regex, None, &["client"])).await;
        assert!(attr(&sp, "gen_ai.conversation.id").is_none());
        assert!(attr(&sp, "obleth.session.source").is_none());
        let sp = run(ev, cfg(ScrubMode::Regex, None, &["client", "derived"])).await;
        assert_eq!(attr(&sp, "gen_ai.conversation.id").as_deref(), Some("sess-1"));
        assert_eq!(attr(&sp, "obleth.session.source").as_deref(), Some("derived"));
    }

    #[tokio::test]
    async fn pii_session_without_key_blanked() {
        let mut ev = event(BODY);
        ev.meta.session_id = "a@b.io".into();
        ev.meta.session_source = "client".into();
        let sp = run(ev, cfg(ScrubMode::Regex, None, &["client"])).await;
        assert!(attr(&sp, "gen_ai.conversation.id").is_none());
    }

    #[tokio::test]
    async fn dropped_event_has_no_content() {
        let mut ev = event(BODY);
        ev.dropped = Some("queue_full");
        let sp = run(ev, cfg(ScrubMode::Regex, None, &[])).await;
        assert!(attr(&sp, "gen_ai.input.messages").is_none());
        assert!(attr(&sp, "gen_ai.output.messages").is_none());
        assert_eq!(attr(&sp, "obleth.content.dropped").as_deref(), Some("queue_full"));
    }

    #[tokio::test]
    async fn collector_truncated_flags_output() {
        let mut ev = event(BODY);
        ev.output.as_mut().unwrap().truncated = true;
        let sp = run(ev, cfg(ScrubMode::Regex, None, &[])).await;
        assert_eq!(attr(&sp, "obleth.content.output_truncated").as_deref(), Some("true"));
    }
}
