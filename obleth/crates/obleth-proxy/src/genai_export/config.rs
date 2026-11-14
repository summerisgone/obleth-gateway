//! GenAI export configuration parsed from `OBLETH_GENAI_*` environment variables.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone)]
pub struct GenAiConfig {
    pub traces_url: String,
    pub headers: Vec<(String, String)>,
    pub content_ttl: Duration,
    pub queue_max_bytes: usize,
    pub flush: Duration,
    pub max_request_bytes: usize,
    pub max_content_bytes: usize,
    pub scrub_default: String,
    pub scrub_url: Option<String>,
    pub scrub_model: Option<String>,
    pub scrub_timeout: Duration,
    pub scrub_concurrency: usize,
    pub user_id_hmac_key: Option<Vec<u8>>,
    pub conversation_sources: Vec<String>,
}

// Manual Debug: header values and the HMAC key are secrets.
impl std::fmt::Debug for GenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenAiConfig")
            .field("traces_url", &self.traces_url)
            .field("headers", &self.headers.len())
            .field("content_ttl", &self.content_ttl)
            .field("queue_max_bytes", &self.queue_max_bytes)
            .field("scrub_default", &self.scrub_default)
            .field("user_id_hmac_key", &self.user_id_hmac_key.is_some())
            .finish_non_exhaustive()
    }
}

fn get<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn num<T: std::str::FromStr>(
    env: &HashMap<String, String>,
    name: &str,
    default: T,
) -> Result<T, String> {
    match get(env, name) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|_| format!("{name}: invalid number")),
    }
}

impl GenAiConfig {
    /// `Ok(None)` when `OBLETH_GENAI_OTLP_ENDPOINT` is unset: the feature is off.
    pub fn from_env_map(env: &HashMap<String, String>) -> Result<Option<GenAiConfig>, String> {
        let Some(endpoint) = get(env, "OBLETH_GENAI_OTLP_ENDPOINT") else {
            return Ok(None);
        };
        let traces_url = format!("{}/v1/traces", endpoint.trim_end_matches('/'));

        let headers = get(env, "OBLETH_GENAI_OTLP_HEADERS")
            .map(|h| {
                h.split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .filter(|(k, _)| !k.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let ttl_hours: u64 = num(env, "OBLETH_GENAI_CONTENT_TTL_HOURS", 24)?;
        let scrub_default = match get(env, "OBLETH_GENAI_SCRUB") {
            None => "regex",
            Some(m @ ("regex" | "off" | "ner" | "llm")) => m,
            Some(other) => return Err(format!("OBLETH_GENAI_SCRUB: unknown mode {other}")),
        }
        .to_string();
        let (pii, pii_error) = crate::pii::PiiEnv::from_env_map(env);
        if let Some(e) = pii_error {
            return Err(e);
        }
        if matches!(scrub_default.as_str(), "ner" | "llm") && pii.ner_url.is_none() {
            return Err("OBLETH_PII_NER_URL: required for ner and llm modes".to_string());
        }
        let conversation_sources = get(env, "OBLETH_GENAI_CONVERSATION_SOURCES")
            .unwrap_or("client")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Some(GenAiConfig {
            traces_url,
            headers,
            content_ttl: Duration::from_secs(ttl_hours.saturating_mul(3600)),
            queue_max_bytes: num(env, "OBLETH_GENAI_QUEUE_MAX_BYTES", 67_108_864)?,
            flush: Duration::from_millis(num(env, "OBLETH_GENAI_FLUSH_MS", 2000)?),
            max_request_bytes: num(env, "OBLETH_GENAI_MAX_REQUEST_BYTES", 4_194_304)?,
            max_content_bytes: num(env, "OBLETH_GENAI_MAX_CONTENT_BYTES", 262_144)?,
            scrub_default,
            scrub_url: pii.ner_url,
            scrub_model: get(env, "OBLETH_GENAI_SCRUB_MODEL").map(str::to_string),
            scrub_timeout: Duration::from_millis(pii.scrub_timeout_ms.unwrap_or(5000)),
            scrub_concurrency: pii.concurrency,
            user_id_hmac_key: get(env, "OBLETH_GENAI_USER_ID_HMAC_KEY")
                .map(|k| k.as_bytes().to_vec()),
            conversation_sources,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EP: &str = "OBLETH_GENAI_OTLP_ENDPOINT";

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn config_defaults() {
        assert!(GenAiConfig::from_env_map(&HashMap::new()).unwrap().is_none());
        let cfg = GenAiConfig::from_env_map(&env(&[(EP, "https://lf/api/public/otel")]))
            .unwrap()
            .unwrap();
        assert_eq!(cfg.traces_url, "https://lf/api/public/otel/v1/traces");
        assert_eq!(cfg.content_ttl, Duration::from_secs(86400));
        assert_eq!(cfg.scrub_default, "regex");
        assert!(cfg.headers.is_empty());
        assert_eq!(cfg.queue_max_bytes, 67_108_864);
        assert_eq!(cfg.flush, Duration::from_millis(2000));
        assert_eq!(cfg.max_request_bytes, 4_194_304);
        assert_eq!(cfg.max_content_bytes, 262_144);
        assert_eq!(cfg.scrub_timeout, Duration::from_millis(5000));
        assert_eq!(cfg.scrub_concurrency, 8);
        assert_eq!(cfg.conversation_sources, vec!["client".to_string()]);
        assert!(cfg.user_id_hmac_key.is_none());

        let err = GenAiConfig::from_env_map(&env(&[(EP, "https://lf"), ("OBLETH_GENAI_SCRUB", "bogus")]))
            .unwrap_err();
        assert_eq!(err, "OBLETH_GENAI_SCRUB: unknown mode bogus");
    }

    #[test]
    fn config_headers_parse() {
        let cfg = GenAiConfig::from_env_map(&env(&[
            (EP, "https://lf/"),
            ("OBLETH_GENAI_OTLP_HEADERS", "a=b,c=d=e"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.traces_url, "https://lf/v1/traces");
        assert_eq!(
            cfg.headers,
            vec![("a".into(), "b".into()), ("c".into(), "d=e".into())]
        );
    }

    #[test]
    fn ner_requires_url() {
        let e = GenAiConfig::from_env_map(&env(&[(EP, "https://lf"), ("OBLETH_GENAI_SCRUB", "ner")]));
        assert!(e.is_err());
        let ok = GenAiConfig::from_env_map(&env(&[
            (EP, "https://lf"),
            ("OBLETH_GENAI_SCRUB", "llm"),
            ("OBLETH_GENAI_SCRUB_URL", "http://s"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(ok.scrub_default, "llm");
    }

    #[test]
    fn config_uses_pii_env_aliases() {
        let c = GenAiConfig::from_env_map(&env(&[
            (EP, "https://lf"),
            ("OBLETH_GENAI_SCRUB", "ner"),
            ("OBLETH_GENAI_SCRUB_URL", "http://old"),
            ("OBLETH_GENAI_SCRUB_CONCURRENCY", "3"),
            ("OBLETH_GENAI_SCRUB_TIMEOUT_MS", "70"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.scrub_url.as_deref(), Some("http://old"));
        assert_eq!(c.scrub_concurrency, 3);
        assert_eq!(c.scrub_timeout, Duration::from_millis(70));
        let c = GenAiConfig::from_env_map(&env(&[
            (EP, "https://lf"),
            ("OBLETH_GENAI_SCRUB", "ner"),
            ("OBLETH_PII_NER_URL", "http://new"),
            ("OBLETH_GENAI_SCRUB_URL", "http://old"),
            ("OBLETH_PII_NER_CONCURRENCY", "5"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.scrub_url.as_deref(), Some("http://new"));
        assert_eq!(c.scrub_concurrency, 5);
        assert!(GenAiConfig::from_env_map(&env(&[(EP, "https://lf"), ("OBLETH_PII_NER_CONCURRENCY", "0")])).is_err());
    }
}
