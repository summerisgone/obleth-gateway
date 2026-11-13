//! Response collector for the GenAI export.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{CollectedOutput, OutputChoice, ToolCallOut};

/// Shared between the response stream and the cancellation guard.
#[derive(Clone)]
pub struct Collector {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    out: CollectedOutput,
    line_buf: Vec<u8>,
    used: usize,
    max: usize,
}

impl Collector {
    pub fn new(max_bytes: usize) -> Collector {
        Collector {
            inner: Arc::new(Mutex::new(Inner {
                out: CollectedOutput::default(),
                line_buf: Vec::new(),
                used: 0,
                max: max_bytes,
            })),
        }
    }

    pub fn feed_sse(&self, chunk: &[u8]) {
        let mut g = self.lock();
        g.line_buf.extend_from_slice(chunk);
        while let Some(pos) = g.line_buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = g.line_buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            if rest.is_empty() || rest == b"[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(rest) {
                g.apply(&v, true);
            }
        }
    }

    pub fn set_json(&self, body: &[u8]) {
        if let Ok(v) = serde_json::from_slice::<Value>(body) {
            self.lock().apply(&v, false);
        }
    }

    pub fn snapshot(&self) -> CollectedOutput {
        self.lock().out.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Inner {
    fn apply(&mut self, v: &Value, delta: bool) {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if self.out.id.is_none() {
                self.out.id = Some(id.to_string());
            }
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            if self.out.model.is_none() {
                self.out.model = Some(m.to_string());
            }
        }
        let Some(choices) = v.get("choices").and_then(Value::as_array) else {
            return;
        };
        for (pos, c) in choices.iter().enumerate() {
            let index = c
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(pos as u64) as u32;
            let slot = match self.out.choices.iter().position(|x| x.index == index) {
                Some(i) => i,
                None => {
                    self.out.choices.push(OutputChoice {
                        index,
                        ..Default::default()
                    });
                    self.out.choices.len() - 1
                }
            };
            let msg = c.get(if delta { "delta" } else { "message" });
            if let Some(m) = msg {
                if let Some(r) = m.get("role").and_then(Value::as_str) {
                    self.out.choices[slot].role = r.to_string();
                }
                if let Some(t) = m.get("content").and_then(Value::as_str) {
                    self.append(slot, Field::Text, t);
                }
                for key in ["reasoning_content", "reasoning"] {
                    if let Some(t) = m.get(key).and_then(Value::as_str) {
                        self.append(slot, Field::Reasoning, t);
                        break;
                    }
                }
                if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                    for (tpos, tc) in tcs.iter().enumerate() {
                        self.tool_call(slot, tpos, tc);
                    }
                }
            }
            if let Some(t) = c.get("text").and_then(Value::as_str) {
                self.append(slot, Field::Text, t);
            }
            if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                self.out.choices[slot].finish_reason = Some(f.to_string());
            }
        }
    }

    fn tool_call(&mut self, slot: usize, tpos: usize, tc: &Value) {
        let ti = tc
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(tpos as u64) as usize;
        // Bound the index so a hostile stream cannot allocate unboundedly.
        if ti > 1024 {
            return;
        }
        let calls = &mut self.out.choices[slot].tool_calls;
        while calls.len() <= ti {
            calls.push(ToolCallOut::default());
        }
        let f = tc.get("function");
        if let Some(id) = tc.get("id").and_then(Value::as_str) {
            calls[ti].id = id.to_string();
        }
        if let Some(n) = f.and_then(|f| f.get("name")).and_then(Value::as_str) {
            if !n.is_empty() {
                calls[ti].name = n.to_string();
            }
        }
        if let Some(a) = f.and_then(|f| f.get("arguments")).and_then(Value::as_str) {
            self.append(slot, Field::Args(ti), a);
        }
    }

    fn append(&mut self, slot: usize, field: Field, s: &str) {
        let remaining = self.max.saturating_sub(self.used);
        let take = if s.len() <= remaining {
            s.len()
        } else {
            self.out.truncated = true;
            let mut n = remaining;
            while !s.is_char_boundary(n) {
                n -= 1;
            }
            n
        };
        let piece = &s[..take];
        self.used += take;
        let ch = &mut self.out.choices[slot];
        match field {
            Field::Text => ch.text.push_str(piece),
            Field::Reasoning => ch.reasoning.push_str(piece),
            Field::Args(i) => ch.tool_calls[i].arguments.push_str(piece),
        }
    }
}

enum Field {
    Text,
    Reasoning,
    Args(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = b"data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    #[test]
    fn json_body() {
        let c = Collector::new(1000);
        c.set_json(br#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#);
        assert_eq!(
            c.snapshot(),
            CollectedOutput {
                id: Some("c1".into()),
                model: Some("m".into()),
                choices: vec![OutputChoice {
                    index: 0,
                    role: "assistant".into(),
                    text: "hi".into(),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                }],
                truncated: false
            }
        );
    }

    #[test]
    fn sse_whole() {
        let c = Collector::new(1000);
        c.feed_sse(STREAM);
        let s = c.snapshot();
        assert_eq!(s.choices[0].text, "hello");
        assert_eq!(s.id.as_deref(), Some("c1"));
        assert_eq!(s.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn split_chunk_lines() {
        let c = Collector::new(1000);
        for b in STREAM {
            c.feed_sse(&[*b]);
        }
        assert_eq!(c.snapshot().choices[0].text, "hello");
    }

    #[test]
    fn crlf_and_no_space() {
        let c = Collector::new(1000);
        c.feed_sse(b"data:{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\r\n\r\ndata:{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\r\n\r\ndata:[DONE]\r\n\r\n");
        assert_eq!(c.snapshot().choices[0].text, "ab");
    }

    #[test]
    fn tool_call_args_accumulate() {
        let c = Collector::new(1000);
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}\n\n");
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}\n\n");
        let tc = &c.snapshot().choices[0].tool_calls;
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].arguments, "{\"a\":1}");
        assert_eq!(tc[0].name, "f");
        assert_eq!(tc[0].id, "t1");
    }

    #[test]
    fn completions_text_sse() {
        let c = Collector::new(1000);
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"text\":\"ab\"}]}\n\ndata: {\"choices\":[{\"index\":0,\"text\":\"cd\"}]}\n\n");
        assert_eq!(c.snapshot().choices[0].text, "abcd");
    }

    #[test]
    fn two_choices() {
        let c = Collector::new(1000);
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}},{\"index\":1,\"delta\":{\"content\":\"x\"}}]}\n\n");
        c.feed_sse(b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"y\"}},{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n\n");
        let s = c.snapshot();
        assert_eq!(s.choices.len(), 2);
        assert_eq!(s.choices[0].text, "ab");
        assert_eq!(s.choices[1].text, "xy");
    }

    #[test]
    fn cap_sets_truncated() {
        let c = Collector::new(4);
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n\n");
        let s = c.snapshot();
        assert_eq!(s.choices[0].text, "hell");
        assert!(s.truncated);
    }

    #[test]
    fn cap_never_splits_utf8() {
        let c = Collector::new(4);
        c.feed_sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ab\u{20ac}\u{20ac}\"}}]}\n\n".as_bytes());
        let s = c.snapshot();
        assert_eq!(s.choices[0].text, "ab");
        assert!(s.truncated);
    }

    #[test]
    fn snapshot_mid_stream() {
        let c = Collector::new(1000);
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"del");
        assert_eq!(c.snapshot().choices[0].text, "par");
        c.feed_sse(b"ta\":{\"content\":\"t\"}}]}\n\n");
        assert_eq!(c.snapshot().choices[0].text, "part");
    }

    #[test]
    fn reasoning_collected() {
        let c = Collector::new(1000);
        c.feed_sse(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n");
        assert_eq!(c.snapshot().choices[0].reasoning, "think");
    }
}
