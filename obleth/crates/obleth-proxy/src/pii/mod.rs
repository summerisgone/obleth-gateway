//! One PII detection layer shared by guardrails (model path) and GenAI export (traces).

pub mod ner;
pub mod regex;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use obleth_config::{BoonSettings, PiiDetectorKind, PiiProfile};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Model,
    Traces,
    /// Off the request path (guardrails `log_only`): the traces timeout, no semaphore.
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiiError {
    NotConfigured,
    Busy,
    Timeout,
    Http(String),
    BadReply(String),
}

impl std::fmt::Display for PiiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PiiError::NotConfigured => write!(f, "ner service not configured"),
            PiiError::Busy => write!(f, "ner service busy"),
            PiiError::Timeout => write!(f, "ner timeout"),
            PiiError::Http(e) => write!(f, "ner http error: {e}"),
            PiiError::BadReply(e) => write!(f, "ner bad reply: {e}"),
        }
    }
}

impl std::error::Error for PiiError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiEnv {
    pub ner_url: Option<String>,
    pub concurrency: usize,
    pub scrub_timeout_ms: Option<u64>,
    pub warnings: Vec<String>,
}

impl PiiEnv {
    /// Always returns a usable env plus the first config error, if any. A bad
    /// concurrency leaves NER unavailable; a bad traces timeout only drops the
    /// timeout (it affects traces alone), so model-path NER stays on. Either
    /// error means GenAI export must stay off. Alias warnings are kept either way.
    pub fn from_env_map(env: &HashMap<String, String>) -> (PiiEnv, Option<String>) {
        let get = |k: &str| env.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let mut warnings = Vec::new();
        let mut pick = |new: &str, old: &str| -> Option<String> {
            match (get(new), get(old)) {
                (Some(n), Some(_)) => {
                    warnings.push(format!("{old} is ignored: {new} is set"));
                    Some(n.to_string())
                }
                (Some(v), None) | (None, Some(v)) => Some(v.to_string()),
                (None, None) => None,
            }
        };
        let ner_url = pick("OBLETH_PII_NER_URL", "OBLETH_GENAI_SCRUB_URL");
        let conc = pick(
            "OBLETH_PII_NER_CONCURRENCY",
            "OBLETH_GENAI_SCRUB_CONCURRENCY",
        );
        let concurrency = match conc {
            None => 8,
            Some(v) => match v.parse::<usize>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    let e =
                        format!("OBLETH_PII_NER_CONCURRENCY must be an integer >= 1, got {v:?}");
                    let env = PiiEnv {
                        ner_url: None,
                        concurrency: 8,
                        scrub_timeout_ms: None,
                        warnings,
                    };
                    return (env, Some(e));
                }
            },
        };
        let (scrub_timeout_ms, traces_error) = match get("OBLETH_GENAI_SCRUB_TIMEOUT_MS") {
            None => (None, None),
            Some(v) => match v.parse::<u64>() {
                Ok(n) => (Some(n), None),
                Err(_) => (
                    None,
                    Some(format!(
                        "OBLETH_GENAI_SCRUB_TIMEOUT_MS must be an integer, got {v:?}"
                    )),
                ),
            },
        };
        let env = PiiEnv {
            ner_url,
            concurrency,
            scrub_timeout_ms,
            warnings,
        };
        (env, traces_error)
    }
}

pub struct PiiDetector {
    url: Option<String>,
    http: reqwest::Client,
    sem: Semaphore,
    env_timeout_ms: Option<u64>,
    settings: Arc<ArcSwap<BoonSettings>>,
}

impl PiiDetector {
    pub fn new(
        env: &PiiEnv,
        http: reqwest::Client,
        settings: Arc<ArcSwap<BoonSettings>>,
    ) -> PiiDetector {
        PiiDetector {
            url: env.ner_url.clone(),
            http,
            sem: Semaphore::new(env.concurrency.max(1)),
            env_timeout_ms: env.scrub_timeout_ms,
            settings,
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Arc<PiiDetector> {
        let (env, _) = PiiEnv::from_env_map(&HashMap::new());
        Arc::new(PiiDetector::new(
            &env,
            reqwest::Client::new(),
            Arc::new(ArcSwap::from_pointee(BoonSettings::default())),
        ))
    }

    #[cfg(test)]
    pub fn ner_configured(&self) -> bool {
        self.url.is_some()
    }

    pub async fn redact(
        &self,
        texts: &[String],
        profile: &PiiProfile,
        lane: Lane,
    ) -> Result<Vec<String>, PiiError> {
        match profile.detector {
            PiiDetectorKind::Off => Ok(texts.to_vec()),
            PiiDetectorKind::Regex => Ok(texts.iter().map(|t| regex::scan_pii(t).1).collect()),
            PiiDetectorKind::Ner => {
                let url = self.url.as_deref().ok_or(PiiError::NotConfigured)?;
                let redacted: Vec<String> = texts.iter().map(|t| regex::scan_pii(t).1).collect();
                let snap = self.settings.load();
                let (_permit, ms) = match lane {
                    Lane::Traces => (
                        Some(self.sem.try_acquire().map_err(|_| PiiError::Busy)?),
                        snap.pii
                            .ner
                            .timeout_ms
                            .or(self.env_timeout_ms)
                            .unwrap_or(5000),
                    ),
                    Lane::Model => (None, snap.pii.ner.model_timeout_ms),
                    Lane::Background => (
                        None,
                        snap.pii
                            .ner
                            .timeout_ms
                            .or(self.env_timeout_ms)
                            .unwrap_or(5000),
                    ),
                };
                drop(snap);
                tokio::time::timeout(
                    Duration::from_millis(ms),
                    ner::call(&self.http, url, &redacted, profile),
                )
                .await
                .map_err(|_| PiiError::Timeout)?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn prof(kind: PiiDetectorKind) -> PiiProfile {
        PiiProfile {
            detector: kind,
            entities: vec![],
            language: None,
            score_threshold: None,
        }
    }

    type Seen = Arc<Mutex<Vec<Value>>>;

    // delay_ms: reply latency; wrong_len: reply with one extra text.
    async fn fake(delay_ms: u64, wrong_len: bool) -> (String, Seen) {
        let seen: Seen = Default::default();
        async fn h(
            State((seen, delay, wrong)): State<(Seen, u64, bool)>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            seen.lock().unwrap().push(body.clone());
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let mut texts = body["texts"].clone();
            if wrong {
                texts.as_array_mut().unwrap().push(json!("extra"));
            }
            Json(json!({ "texts": texts }))
        }
        let app = Router::new()
            .route("/", post(h))
            .with_state((seen.clone(), delay_ms, wrong_len));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}/"), seen)
    }

    fn detector(url: Option<String>, conc: usize) -> (PiiDetector, Arc<ArcSwap<BoonSettings>>) {
        let e = PiiEnv {
            ner_url: url,
            concurrency: conc,
            scrub_timeout_ms: None,
            warnings: vec![],
        };
        let s = Arc::new(ArcSwap::from_pointee(BoonSettings::default()));
        (PiiDetector::new(&e, reqwest::Client::new(), s.clone()), s)
    }

    fn texts(t: &str) -> Vec<String> {
        vec![t.to_string()]
    }

    #[test]
    fn env_alias_url() {
        let (e, err) = PiiEnv::from_env_map(&env(&[("OBLETH_GENAI_SCRUB_URL", "http://a")]));
        assert!(err.is_none());
        assert_eq!(e.ner_url.as_deref(), Some("http://a"));
        assert_eq!(e.concurrency, 8);
        assert_eq!(e.scrub_timeout_ms, None);
        assert!(e.warnings.is_empty());
    }

    #[test]
    fn env_new_wins_with_warning() {
        let (e, _) = PiiEnv::from_env_map(&env(&[
            ("OBLETH_PII_NER_URL", "http://b"),
            ("OBLETH_GENAI_SCRUB_URL", "http://a"),
        ]));
        assert_eq!(e.ner_url.as_deref(), Some("http://b"));
        assert_eq!(e.warnings.len(), 1);
    }

    #[test]
    fn env_bad_concurrency_and_timeout() {
        assert!(
            PiiEnv::from_env_map(&env(&[("OBLETH_PII_NER_CONCURRENCY", "0")]))
                .1
                .is_some()
        );
        assert!(
            PiiEnv::from_env_map(&env(&[("OBLETH_GENAI_SCRUB_CONCURRENCY", "x")]))
                .1
                .is_some()
        );
        let (e, err) = PiiEnv::from_env_map(&env(&[
            ("OBLETH_PII_NER_URL", "http://n"),
            ("OBLETH_GENAI_SCRUB_TIMEOUT_MS", "x"),
        ]));
        assert!(err.unwrap().contains("OBLETH_GENAI_SCRUB_TIMEOUT_MS"));
        assert_eq!(e.ner_url.as_deref(), Some("http://n"));
        assert_eq!(e.scrub_timeout_ms, None);
        let (e, err) = PiiEnv::from_env_map(&env(&[
            ("OBLETH_PII_NER_URL", ""),
            ("OBLETH_GENAI_SCRUB_TIMEOUT_MS", "70"),
        ]));
        assert!(err.is_none());
        assert_eq!(e.ner_url, None);
        assert_eq!(e.scrub_timeout_ms, Some(70));
    }

    #[tokio::test]
    async fn regex_and_off() {
        let (d, _) = detector(None, 1);
        let out = d
            .redact(
                &texts("mail a@b.co"),
                &prof(PiiDetectorKind::Regex),
                Lane::Model,
            )
            .await;
        assert_eq!(out, Ok(texts("mail [REDACTED:EMAIL]")));
        let out = d
            .redact(&texts("x"), &prof(PiiDetectorKind::Off), Lane::Traces)
            .await;
        assert_eq!(out, Ok(texts("x")));
        let out = d
            .redact(&texts("x"), &prof(PiiDetectorKind::Ner), Lane::Traces)
            .await;
        assert_eq!(out, Err(PiiError::NotConfigured));
        assert!(!d.ner_configured());
    }

    #[tokio::test]
    async fn ner_body_has_optional_fields() {
        let (url, seen) = fake(0, false).await;
        let (d, _) = detector(Some(url), 2);
        d.redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Model)
            .await
            .unwrap();
        let p = PiiProfile {
            detector: PiiDetectorKind::Ner,
            entities: vec!["PERSON".into()],
            language: Some("ru".into()),
            score_threshold: Some(0.5),
        };
        d.redact(&texts("a"), &p, Lane::Model).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], json!({"texts": ["a"]}));
        assert_eq!(
            seen[1],
            json!({"texts": ["a"], "entities": ["PERSON"], "language": "ru", "score_threshold": 0.5})
        );
    }

    #[tokio::test]
    async fn ner_wrong_len_fails() {
        let (url, _) = fake(0, true).await;
        let (d, _) = detector(Some(url), 2);
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Traces)
            .await;
        assert!(matches!(r, Err(PiiError::BadReply(_))));
    }

    #[tokio::test]
    async fn traces_busy_no_wait() {
        let (url, _) = fake(0, false).await;
        let (d, _) = detector(Some(url), 1);
        let _held = d.sem.try_acquire().unwrap();
        let t = std::time::Instant::now();
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Traces)
            .await;
        assert_eq!(r, Err(PiiError::Busy));
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn model_lane_no_semaphore() {
        let (url, _) = fake(0, false).await;
        let (d, _) = detector(Some(url), 1);
        let _held = d.sem.try_acquire().unwrap();
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Model)
            .await;
        assert_eq!(r, Ok(texts("a")));
    }

    #[tokio::test]
    async fn timeouts_follow_snapshot() {
        let (url, _) = fake(300, false).await;
        let (d, s) = detector(Some(url), 2);
        let mut b = BoonSettings::default();
        b.pii.ner.model_timeout_ms = 50;
        s.store(Arc::new(b.clone()));
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Model)
            .await;
        assert_eq!(r, Err(PiiError::Timeout));
        b.pii.ner.model_timeout_ms = 2000;
        s.store(Arc::new(b));
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Model)
            .await;
        assert_eq!(r, Ok(texts("a")));
    }

    #[tokio::test]
    async fn traces_timeout_order() {
        let (url, _) = fake(300, false).await;
        let (mut d, s) = detector(Some(url), 2);
        d.env_timeout_ms = Some(50);
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Traces)
            .await;
        assert_eq!(r, Err(PiiError::Timeout));
        let mut b = BoonSettings::default();
        b.pii.ner.timeout_ms = Some(2000);
        s.store(Arc::new(b));
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Traces)
            .await;
        assert_eq!(r, Ok(texts("a")));
    }

    #[tokio::test]
    async fn background_lane_uses_traces_timeout_without_semaphore() {
        let (url, _) = fake(200, false).await;
        let (d, s) = detector(Some(url), 1);
        let _held = d.sem.try_acquire().unwrap();
        let mut b = BoonSettings::default();
        b.pii.ner.model_timeout_ms = 20;
        b.pii.ner.timeout_ms = Some(2000);
        s.store(Arc::new(b));
        let r = d
            .redact(&texts("a"), &prof(PiiDetectorKind::Ner), Lane::Background)
            .await;
        assert_eq!(r, Ok(texts("a")));
    }

    #[tokio::test]
    async fn regex_runs_before_ner() {
        let (url, seen) = fake(0, false).await;
        let (d, _) = detector(Some(url), 2);
        let out = d
            .redact(
                &texts("mail a@b.co"),
                &prof(PiiDetectorKind::Ner),
                Lane::Traces,
            )
            .await
            .unwrap();
        assert_eq!(out, texts("mail [REDACTED:EMAIL]"));
        assert_eq!(seen.lock().unwrap()[0]["texts"][0], "mail [REDACTED:EMAIL]");
    }
}
