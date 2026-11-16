//! Byte-bounded queue, batching and OTLP/HTTP delivery of GenAI spans. Nothing touches disk.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prometheus::{IntCounter, IntCounterVec, IntGauge, Opts, Registry};
use prost::Message;
use tokio::sync::{mpsc, oneshot};

use super::config::GenAiConfig;
use super::GenAiEvent;

pub type ProcessFn = Arc<dyn Fn(GenAiEvent) -> BoxFuture<'static, Span> + Send + Sync>;

const MAX_BATCH_SPANS: usize = 256;
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const CONTENT_ATTRS: [&str; 3] = [
    "gen_ai.input.messages",
    "gen_ai.output.messages",
    "gen_ai.system_instructions",
];

struct Metrics {
    exported: IntCounter,
    dropped: IntCounterVec,
    queue_bytes: IntGauge,
}

impl Metrics {
    fn new(registry: &Registry) -> Metrics {
        let m = Metrics {
            exported: IntCounter::new("obleth_genai_exported_total", "GenAI spans exported")
                .expect("valid metric"),
            dropped: IntCounterVec::new(
                Opts::new("obleth_genai_dropped_total", "GenAI spans dropped"),
                &["reason"],
            )
            .expect("valid metric"),
            queue_bytes: IntGauge::new("obleth_genai_queue_bytes", "GenAI export queue bytes")
                .expect("valid metric"),
        };
        // Already registered (second exporter in one process): keep local, unregistered metrics.
        let _ = registry.register(Box::new(m.exported.clone()));
        let _ = registry.register(Box::new(m.dropped.clone()));
        let _ = registry.register(Box::new(m.queue_bytes.clone()));
        m
    }
}

struct Shared {
    cfg: Arc<GenAiConfig>,
    http: reqwest::Client,
    metrics: Metrics,
    queue_bytes: AtomicUsize,
    closing: AtomicBool,
}

impl Shared {
    fn release(&self, n: usize) {
        let _ = self
            .queue_bytes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
                Some(c.saturating_sub(n))
            });
        self.metrics
            .queue_bytes
            .set(self.queue_bytes.load(Ordering::SeqCst) as i64);
    }

    fn drop_items(&self, items: &[Item], reason: &str) {
        self.metrics
            .dropped
            .with_label_values(&[reason])
            .inc_by(items.len() as u64);
        for it in items {
            self.release(it.bytes);
        }
    }
}

struct Item {
    span: Span,
    bytes: usize,
    enqueued: Instant,
}

enum Msg {
    Event(GenAiEvent, usize),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct ExporterHandle {
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
}

pub struct Exporter;

impl Exporter {
    /// Must be called inside a tokio runtime.
    pub fn start(
        cfg: Arc<GenAiConfig>,
        http: reqwest::Client,
        process: ProcessFn,
        registry: &Registry,
    ) -> ExporterHandle {
        let shared = Arc::new(Shared {
            cfg,
            http,
            metrics: Metrics::new(registry),
            queue_bytes: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(worker(shared.clone(), process, rx));
        ExporterHandle { tx, shared }
    }
}

impl ExporterHandle {
    /// Trace profile used when neither the tenant nor `pii.traces` names one.
    pub fn scrub_default(&self) -> &str {
        &self.shared.cfg.scrub_default
    }

    /// Never blocks. Over the byte limit the event is still queued, without content.
    pub fn try_send(&self, mut ev: GenAiEvent) -> bool {
        if ev.enqueued_at.is_none() {
            ev.enqueued_at = Some(Instant::now());
        }
        let mut bytes = ev.approx_bytes();
        let cur = self.shared.queue_bytes.load(Ordering::SeqCst);
        if cur.saturating_add(bytes) > self.shared.cfg.queue_max_bytes {
            ev.request_body = None;
            ev.output = None;
            ev.dropped = Some("queue_full");
            bytes = ev.approx_bytes();
            self.shared
                .metrics
                .dropped
                .with_label_values(&["queue_full"])
                .inc();
        }
        self.shared.queue_bytes.fetch_add(bytes, Ordering::SeqCst);
        self.shared
            .metrics
            .queue_bytes
            .set(self.shared.queue_bytes.load(Ordering::SeqCst) as i64);
        if self.tx.send(Msg::Event(ev, bytes)).is_err() {
            self.shared.release(bytes);
            return false;
        }
        true
    }

    /// Flushes what is pending, then stops the worker; gives up after `timeout`.
    pub async fn shutdown(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(Msg::Shutdown(ack_tx)).is_err() {
            return;
        }
        let _ = tokio::time::timeout(timeout, ack_rx).await;
    }
}

struct Batcher {
    shared: Arc<Shared>,
    process: ProcessFn,
    pending: Vec<Item>,
    pending_size: usize,
}

async fn worker(shared: Arc<Shared>, process: ProcessFn, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut b = Batcher {
        shared: shared.clone(),
        process,
        pending: Vec::new(),
        pending_size: 0,
    };
    let mut tick = tokio::time::interval(shared.cfg.flush.max(Duration::from_millis(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let width = shared.cfg.scrub_concurrency.max(1);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                None => {
                    b.flush().await;
                    break;
                }
                Some(Msg::Event(ev, bytes)) => {
                    let mut group = vec![(ev, bytes)];
                    let mut stop = None;
                    while group.len() < width {
                        match rx.try_recv() {
                            Ok(Msg::Event(ev, bytes)) => group.push((ev, bytes)),
                            Ok(Msg::Shutdown(ack)) => {
                                stop = Some(ack);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    b.accept_group(group).await;
                    if let Some(ack) = stop {
                        b.finish(&mut rx, ack).await;
                        break;
                    }
                }
                Some(Msg::Shutdown(ack)) => {
                    b.finish(&mut rx, ack).await;
                    break;
                }
            },
            _ = tick.tick() => b.flush().await,
        }
    }
}

impl Batcher {
    async fn finish(&mut self, rx: &mut mpsc::UnboundedReceiver<Msg>, ack: oneshot::Sender<()>) {
        self.shared.closing.store(true, Ordering::SeqCst);
        let width = self.shared.cfg.scrub_concurrency.max(1);
        let mut group = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let Msg::Event(ev, bytes) = m {
                group.push((ev, bytes));
                if group.len() >= width {
                    self.accept_group(std::mem::take(&mut group)).await;
                }
            }
        }
        self.accept_group(group).await;
        self.flush().await;
        let _ = ack.send(());
    }

    /// Processes up to `scrub_concurrency` events at once; spans keep arrival order.
    async fn accept_group(&mut self, group: Vec<(GenAiEvent, usize)>) {
        use futures::StreamExt;
        let mut live = Vec::with_capacity(group.len());
        for (ev, bytes) in group {
            let enqueued = ev.enqueued_at.unwrap_or_else(Instant::now);
            if enqueued.elapsed() > self.shared.cfg.content_ttl {
                self.shared
                    .metrics
                    .dropped
                    .with_label_values(&["ttl"])
                    .inc();
                self.shared.release(bytes);
                continue;
            }
            live.push((ev, bytes, enqueued));
        }
        let process = self.process.clone();
        let width = self.shared.cfg.scrub_concurrency.max(1);
        let mut spans = futures::stream::iter(live.into_iter().map(|(ev, bytes, enqueued)| {
            let fut = (process)(ev);
            async move { (fut.await, bytes, enqueued) }
        }))
        .buffered(width);
        while let Some((span, bytes, enqueued)) = spans.next().await {
            self.pending_size += span.encoded_len();
            self.pending.push(Item {
                span,
                bytes,
                enqueued,
            });
            if self.pending.len() >= MAX_BATCH_SPANS || self.pending_size >= MAX_BATCH_BYTES {
                self.flush().await;
            }
        }
    }

    async fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let items = std::mem::take(&mut self.pending);
        self.pending_size = 0;
        send_items(&self.shared, items).await;
    }
}

fn str_kv(key: &str, v: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.to_string())),
        }),
        ..Default::default()
    }
}

fn build_request(cfg: &GenAiConfig, spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![
                    str_kv("service.name", "obleth"),
                    KeyValue {
                        key: "obleth.content.ttl_hours".to_string(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::IntValue(
                                (cfg.content_ttl.as_secs() / 3600) as i64,
                            )),
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "obleth.genai".to_string(),
                    ..Default::default()
                }),
                spans,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

fn strip_content(span: &mut Span) {
    span.attributes
        .retain(|kv| !CONTENT_ATTRS.contains(&kv.key.as_str()));
    span.attributes
        .retain(|kv| kv.key != "obleth.content.dropped");
    span.attributes
        .push(str_kv("obleth.content.dropped", "too_large"));
}

fn already_stripped(span: &Span) -> bool {
    span.attributes.iter().any(|kv| {
        kv.key == "obleth.content.dropped"
            && matches!(
                kv.value.as_ref().and_then(|v| v.value.as_ref()),
                Some(any_value::Value::StringValue(s)) if s == "too_large"
            )
    })
}

async fn send_items(shared: &Shared, items: Vec<Item>) {
    let mut stack = vec![items];
    while let Some(group) = stack.pop() {
        send_group(shared, group, &mut stack).await;
    }
}

enum Outcome {
    Done,
    Retry,
    TooLarge,
    Reject,
}

async fn post(shared: &Shared, items: &[Item]) -> (Outcome, usize) {
    let req = build_request(&shared.cfg, items.iter().map(|i| i.span.clone()).collect());
    let mut rb = shared
        .http
        .post(&shared.cfg.traces_url)
        .header("content-type", "application/x-protobuf")
        .body(req.encode_to_vec());
    for (k, v) in &shared.cfg.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(_) => return (Outcome::Retry, 0),
    };
    let status = resp.status();
    if status.is_success() {
        let rejected = match resp.bytes().await {
            Ok(b) => ExportTraceServiceResponse::decode(b.as_ref())
                .ok()
                .and_then(|r| r.partial_success)
                .map(|p| p.rejected_spans.max(0) as usize)
                .unwrap_or(0),
            Err(_) => 0,
        };
        return (Outcome::Done, rejected.min(items.len()));
    }
    let outcome = match status.as_u16() {
        413 => Outcome::TooLarge,
        429 | 500..=599 => Outcome::Retry,
        _ => Outcome::Reject,
    };
    (outcome, 0)
}

async fn send_group(shared: &Shared, mut items: Vec<Item>, stack: &mut Vec<Vec<Item>>) {
    let mut backoff = BACKOFF_START;
    loop {
        let ttl = shared.cfg.content_ttl;
        let (live, expired): (Vec<Item>, Vec<Item>) =
            items.into_iter().partition(|i| i.enqueued.elapsed() <= ttl);
        shared.drop_items(&expired, "ttl");
        items = live;
        if items.is_empty() {
            return;
        }
        let (outcome, rejected) = post(shared, &items).await;
        match outcome {
            Outcome::Done => {
                shared
                    .metrics
                    .exported
                    .inc_by((items.len() - rejected) as u64);
                if rejected > 0 {
                    shared
                        .metrics
                        .dropped
                        .with_label_values(&["rejected"])
                        .inc_by(rejected as u64);
                }
                for it in &items {
                    shared.release(it.bytes);
                }
                return;
            }
            Outcome::TooLarge => {
                if items.len() > 1 {
                    let tail = items.split_off(items.len() / 2);
                    stack.push(tail);
                    stack.push(items);
                    return;
                }
                if already_stripped(&items[0].span) {
                    shared.drop_items(&items, "rejected");
                    return;
                }
                strip_content(&mut items[0].span);
            }
            Outcome::Reject => {
                shared.drop_items(&items, "rejected");
                return;
            }
            Outcome::Retry => {
                if shared.closing.load(Ordering::SeqCst) {
                    shared.drop_items(&items, "shutdown");
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genai_export::{CollectedOutput, OutputChoice};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Responder = Arc<dyn Fn(usize, &ExportTraceServiceRequest) -> StatusCode + Send + Sync>;

    struct Mock {
        url: String,
        reqs: Arc<Mutex<Vec<(HeaderMap, ExportTraceServiceRequest)>>>,
    }

    #[derive(Clone)]
    struct MockState {
        reqs: Arc<Mutex<Vec<(HeaderMap, ExportTraceServiceRequest)>>>,
        responder: Responder,
    }

    async fn handle(
        State(st): State<MockState>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> StatusCode {
        let req = ExportTraceServiceRequest::decode(body.as_ref()).unwrap();
        let n = {
            let mut g = st.reqs.lock().unwrap();
            g.push((headers, req.clone()));
            g.len() - 1
        };
        (st.responder)(n, &req)
    }

    async fn mock(responder: Responder) -> Mock {
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let st = MockState {
            reqs: reqs.clone(),
            responder,
        };
        let app = axum::Router::new()
            .route("/v1/traces", axum::routing::post(handle))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Mock {
            url: format!("http://{addr}"),
            reqs,
        }
    }

    fn ok() -> Responder {
        Arc::new(|_, _| StatusCode::OK)
    }

    fn cfg(url: &str, tweak: impl FnOnce(&mut GenAiConfig)) -> Arc<GenAiConfig> {
        let env: HashMap<String, String> = [
            ("OBLETH_GENAI_OTLP_ENDPOINT".to_string(), url.to_string()),
            (
                "OBLETH_GENAI_OTLP_HEADERS".to_string(),
                "authorization=Basic xyz".to_string(),
            ),
        ]
        .into();
        let mut c = GenAiConfig::from_env_map(&env).unwrap().unwrap();
        c.flush = Duration::from_millis(20);
        tweak(&mut c);
        Arc::new(c)
    }

    fn named_process() -> ProcessFn {
        Arc::new(|ev: GenAiEvent| {
            Box::pin(async move {
                let mut s = Span {
                    name: ev.meta.request_id.clone(),
                    ..Default::default()
                };
                if ev.dropped.is_none() {
                    s.attributes.push(str_kv("gen_ai.input.messages", "[]"));
                }
                s
            })
        })
    }

    fn ev(id: &str) -> GenAiEvent {
        let mut e = GenAiEvent::default();
        e.meta.request_id = id.to_string();
        e
    }

    fn start(c: Arc<GenAiConfig>, p: ProcessFn) -> (ExporterHandle, Registry) {
        let reg = Registry::new();
        (Exporter::start(c, reqwest::Client::new(), p, &reg), reg)
    }

    async fn wait_for(mut f: impl FnMut() -> bool) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    fn dropped(h: &ExporterHandle, reason: &str) -> u64 {
        h.shared.metrics.dropped.with_label_values(&[reason]).get()
    }

    fn span_count(m: &Mock) -> usize {
        m.reqs
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r.resource_spans[0].scope_spans[0].spans.len())
            .sum()
    }

    #[tokio::test]
    async fn posts_protobuf_to_traces_url() {
        let m = mock(ok()).await;
        let (h, reg) = start(cfg(&m.url, |_| {}), named_process());
        assert!(h.try_send(ev("r1")));
        wait_for(|| h.shared.metrics.exported.get() == 1).await;
        let g = m.reqs.lock().unwrap();
        assert_eq!(g.len(), 1);
        let (headers, req) = &g[0];
        assert_eq!(headers["content-type"], "application/x-protobuf");
        assert_eq!(headers["authorization"], "Basic xyz");
        let rs = &req.resource_spans[0];
        assert_eq!(
            rs.scope_spans[0].scope.as_ref().unwrap().name,
            "obleth.genai"
        );
        assert_eq!(rs.scope_spans[0].spans.len(), 1);
        assert_eq!(rs.scope_spans[0].spans[0].name, "r1");
        assert!(rs
            .resource
            .as_ref()
            .unwrap()
            .attributes
            .iter()
            .any(|kv| kv.key == "service.name"));
        assert!(reg
            .gather()
            .iter()
            .any(|f| f.name() == "obleth_genai_exported_total"));
    }

    #[tokio::test]
    async fn retries_503_then_ok() {
        let m = mock(Arc::new(|n, _| {
            if n == 0 {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            }
        }))
        .await;
        let (h, _) = start(cfg(&m.url, |_| {}), named_process());
        h.try_send(ev("r1"));
        wait_for(|| h.shared.metrics.exported.get() == 1).await;
        assert_eq!(m.reqs.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn split_on_413() {
        let m = mock(Arc::new(|_, r| {
            if r.resource_spans[0].scope_spans[0].spans.len() > 1 {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::OK
            }
        }))
        .await;
        let (h, _) = start(
            cfg(&m.url, |c| c.flush = Duration::from_millis(200)),
            named_process(),
        );
        h.try_send(ev("a"));
        h.try_send(ev("b"));
        wait_for(|| h.shared.metrics.exported.get() == 2).await;
        let sizes: Vec<usize> = m
            .reqs
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r.resource_spans[0].scope_spans[0].spans.len())
            .collect();
        assert_eq!(sizes, vec![2, 1, 1]);
    }

    #[tokio::test]
    async fn single_413_resent_without_content() {
        let m = mock(Arc::new(|_, r| {
            let s = &r.resource_spans[0].scope_spans[0].spans[0];
            if s.attributes
                .iter()
                .any(|kv| kv.key == "gen_ai.input.messages")
            {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::OK
            }
        }))
        .await;
        let (h, _) = start(cfg(&m.url, |_| {}), named_process());
        h.try_send(ev("a"));
        wait_for(|| h.shared.metrics.exported.get() == 1).await;
        let g = m.reqs.lock().unwrap();
        assert_eq!(g.len(), 2);
        let s = &g[1].1.resource_spans[0].scope_spans[0].spans[0];
        assert!(!s
            .attributes
            .iter()
            .any(|kv| kv.key == "gen_ai.input.messages"));
        assert!(already_stripped(s));
    }

    #[tokio::test]
    async fn single_413_always_terminates() {
        let m = mock(Arc::new(|_, _| StatusCode::PAYLOAD_TOO_LARGE)).await;
        let (h, _) = start(cfg(&m.url, |_| {}), named_process());
        h.try_send(ev("a"));
        wait_for(|| dropped(&h, "rejected") == 1).await;
        assert_eq!(m.reqs.lock().unwrap().len(), 2);
        wait_for(|| h.shared.queue_bytes.load(Ordering::SeqCst) == 0).await;
    }

    #[tokio::test]
    async fn drop_on_400() {
        let m = mock(Arc::new(|_, _| StatusCode::BAD_REQUEST)).await;
        let (h, _) = start(cfg(&m.url, |_| {}), named_process());
        h.try_send(ev("a"));
        wait_for(|| dropped(&h, "rejected") == 1).await;
        assert_eq!(h.shared.metrics.exported.get(), 0);
        assert_eq!(m.reqs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn queue_full_strips_content() {
        let m = mock(ok()).await;
        let seen: Arc<Mutex<Vec<GenAiEvent>>> = Arc::default();
        let s2 = seen.clone();
        let p: ProcessFn = Arc::new(move |e: GenAiEvent| {
            s2.lock().unwrap().push(e);
            Box::pin(async { Span::default() })
        });
        let (h, _) = start(cfg(&m.url, |c| c.queue_max_bytes = 10), p);
        let mut e = ev("a");
        e.request_body = Some(bytes::Bytes::from_static(b"{}"));
        e.output = Some(CollectedOutput {
            choices: vec![OutputChoice {
                text: "x".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        h.try_send(e);
        wait_for(|| !seen.lock().unwrap().is_empty()).await;
        let g = seen.lock().unwrap();
        assert!(g[0].request_body.is_none());
        assert!(g[0].output.is_none());
        assert_eq!(g[0].dropped, Some("queue_full"));
        assert_eq!(dropped(&h, "queue_full"), 1);
    }

    #[tokio::test]
    async fn ttl_expired_dropped() {
        let m = mock(ok()).await;
        let (h, _) = start(
            cfg(&m.url, |c| c.content_ttl = Duration::from_secs(1)),
            named_process(),
        );
        let mut e = ev("a");
        e.enqueued_at = Instant::now().checked_sub(Duration::from_secs(5));
        h.try_send(e);
        wait_for(|| dropped(&h, "ttl") == 1).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(m.reqs.lock().unwrap().len(), 0);
        assert_eq!(h.shared.queue_bytes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_flushes_pending() {
        let m = mock(ok()).await;
        let (h, _) = start(
            cfg(&m.url, |c| c.flush = Duration::from_secs(60)),
            named_process(),
        );
        h.try_send(ev("a"));
        h.shutdown(Duration::from_secs(5)).await;
        assert_eq!(span_count(&m), 1);
        assert_eq!(h.shared.metrics.exported.get(), 1);
    }

    #[tokio::test]
    async fn partial_success_counts_rejected() {
        let m = mock(Arc::new(|_, _| StatusCode::OK)).await;
        // Mock replies with an empty body; a rejected count needs a body, so use a custom server.
        drop(m);
        let app = axum::Router::new().route(
            "/v1/traces",
            axum::routing::post(|| async {
                ExportTraceServiceResponse {
                    partial_success: Some(
                        opentelemetry_proto::tonic::collector::trace::v1::ExportTracePartialSuccess {
                            rejected_spans: 1,
                            error_message: String::new(),
                        },
                    ),
                }
                .encode_to_vec()
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let (h, _) = start(cfg(&url, |_| {}), named_process());
        h.try_send(ev("a"));
        h.try_send(ev("b"));
        wait_for(|| dropped(&h, "rejected") == 1).await;
        assert_eq!(h.shared.metrics.exported.get(), 1);
    }

    #[tokio::test]
    async fn queue_bytes_returns_to_zero() {
        let m = mock(ok()).await;
        let (h, _) = start(
            cfg(&m.url, |c| c.flush = Duration::from_millis(5)),
            named_process(),
        );
        for i in 0..100 {
            let mut e = ev(&format!("r{i}"));
            e.request_body = Some(bytes::Bytes::from(vec![b'x'; 100]));
            assert!(h.try_send(e));
            if i % 10 == 9 {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        }
        wait_for(|| h.shared.metrics.exported.get() == 100).await;
        wait_for(|| h.shared.queue_bytes.load(Ordering::SeqCst) == 0).await;
        assert_eq!(h.shared.metrics.queue_bytes.get(), 0);
        assert_eq!(span_count(&m), 100);
    }

    #[tokio::test]
    async fn exporter_parallel_scrub() {
        let m = mock(ok()).await;
        let p: ProcessFn = Arc::new(|ev: GenAiEvent| {
            Box::pin(async move {
                let ms = if ev.meta.request_id == "a" { 250 } else { 200 };
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Span {
                    name: ev.meta.request_id.clone(),
                    ..Default::default()
                }
            })
        });
        let (h, _) = start(cfg(&m.url, |c| c.scrub_concurrency = 4), p);
        let t = Instant::now();
        for id in ["a", "b", "c", "d"] {
            h.try_send(ev(id));
        }
        wait_for(|| span_count(&m) == 4).await;
        assert!(
            t.elapsed() < Duration::from_millis(600),
            "{:?}",
            t.elapsed()
        );
        let names: Vec<String> = m
            .reqs
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, r)| {
                r.resource_spans[0].scope_spans[0]
                    .spans
                    .iter()
                    .map(|s| s.name.clone())
            })
            .collect();
        assert_eq!(names, vec!["a", "b", "c", "d"]);
    }
}
