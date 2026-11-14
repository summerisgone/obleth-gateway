use obleth_config::PiiProfile;
use serde_json::{json, Value};

use super::PiiError;

pub(super) async fn call(
    http: &reqwest::Client,
    url: &str,
    texts: &[String],
    profile: &PiiProfile,
) -> Result<Vec<String>, PiiError> {
    let mut body = json!({ "texts": texts });
    if !profile.entities.is_empty() {
        body["entities"] = json!(profile.entities);
    }
    if let Some(l) = &profile.language {
        body["language"] = json!(l);
    }
    if let Some(t) = profile.score_threshold {
        body["score_threshold"] = json!(t);
    }
    let resp = http
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|_| PiiError::Http("request failed".into()))?;
    if !resp.status().is_success() {
        return Err(PiiError::Http(format!("status {}", resp.status().as_u16())));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|_| PiiError::BadReply("not json".into()))?;
    let out: Vec<String> = v
        .get("texts")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .map(|x| x.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| PiiError::BadReply("missing texts".into()))?;
    if out.len() != texts.len() {
        return Err(PiiError::BadReply("length mismatch".into()));
    }
    Ok(out)
}
