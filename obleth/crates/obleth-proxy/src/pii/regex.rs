use std::sync::OnceLock;

use regex::Regex;

struct PiiPattern {
    label: &'static str,
    re: Regex,
}

static PII_PATTERNS: OnceLock<Vec<PiiPattern>> = OnceLock::new();

fn pii_patterns() -> &'static Vec<PiiPattern> {
    PII_PATTERNS.get_or_init(|| {
        vec![
            PiiPattern {
                label: "SSN",
                re: Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
            },
            PiiPattern {
                label: "EMAIL",
                re: Regex::new(r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b").unwrap(),
            },
            PiiPattern {
                label: "PHONE",
                re: Regex::new(
                    r"(?:\+?1[-.\s]?)?\(?[2-9]\d{2}\)?[-.\s]?[2-9]\d{2}[-.\s]?\d{4}\b",
                )
                .unwrap(),
            },
            PiiPattern {
                label: "CREDIT_CARD",
                re: Regex::new(
                    r"\b(?:4[0-9]{12}(?:[0-9]{3})?|5[1-5][0-9]{14}|3[47][0-9]{13}|6(?:011|5[0-9]{2})[0-9]{12})\b",
                )
                .unwrap(),
            },
        ]
    })
}

pub fn scan_pii(text: &str) -> (bool, String) {
    let mut out = text.to_string();
    let mut flagged = false;
    for p in pii_patterns() {
        if p.re.is_match(&out) {
            let tag = format!("[REDACTED:{}]", p.label);
            out = p.re.replace_all(&out, tag.as_str()).into_owned();
            flagged = true;
        }
    }
    (flagged, out)
}
