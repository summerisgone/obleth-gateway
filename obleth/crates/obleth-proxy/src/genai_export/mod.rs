pub mod collector;
pub mod config;
pub mod exporter;
pub mod mapping;
pub mod pipeline;
pub mod scrub;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApiSurface {
    #[default]
    Chat,
    Completions,
    Responses,
    Messages,
}

impl ApiSurface {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiSurface::Chat => "chat",
            ApiSurface::Completions => "completions",
            ApiSurface::Responses => "responses",
            ApiSurface::Messages => "messages",
        }
    }

    pub fn operation_name(&self) -> &'static str {
        match self {
            ApiSurface::Completions => "text_completion",
            _ => "chat",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GenAiCapture {
    pub body: Option<bytes::Bytes>,
    pub surface: ApiSurface,
    pub dropped: Option<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCallOut {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputChoice {
    pub index: u32,
    pub role: String,
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCallOut>,
    pub finish_reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollectedOutput {
    pub id: Option<String>,
    pub model: Option<String>,
    pub choices: Vec<OutputChoice>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Content {
    pub system: Vec<serde_json::Value>,
    pub input: Vec<serde_json::Value>,
    pub output: Vec<serde_json::Value>,
    pub user: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<f64>,
    pub max_tokens: Option<i64>,
    pub seed: Option<i64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub choice_count: Option<i64>,
    pub stream: bool,
    pub output_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub reasoning: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventMeta {
    pub request_id: String,
    pub tenant_id: String,
    pub key_id: String,
    pub client_model: String,
    pub routed_model: String,
    pub model_variant: String,
    pub provider: String,
    pub server_address: Option<String>,
    pub server_port: Option<u16>,
    pub session_id: String,
    pub session_source: String,
    pub reasoning_effort: Option<String>,
    pub cache_status: String,
    pub tool_loop_turns: u32,
    pub ops_trace_id: Option<String>,
    pub ops_span_id: Option<String>,
    pub usage: Usage,
    pub error_type: Option<String>,
    pub scrub: obleth_config::TraceProfile,
}

impl Default for EventMeta {
    fn default() -> Self {
        EventMeta {
            request_id: String::new(),
            tenant_id: String::new(),
            key_id: String::new(),
            client_model: String::new(),
            routed_model: String::new(),
            model_variant: String::new(),
            provider: String::new(),
            server_address: None,
            server_port: None,
            session_id: String::new(),
            session_source: String::new(),
            reasoning_effort: None,
            cache_status: String::new(),
            tool_loop_turns: 0,
            ops_trace_id: None,
            ops_span_id: None,
            usage: Usage::default(),
            error_type: None,
            // No profile: content is dropped as scrub_failed until one is set.
            scrub: obleth_config::TraceProfile {
                name: String::new(),
                profile: None,
                strengthened: false,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GenAiEvent {
    pub start_unix_nanos: u64,
    pub end_unix_nanos: u64,
    pub ttft_unix_nanos: Option<u64>,
    pub enqueued_at: Option<std::time::Instant>,
    pub surface: ApiSurface,
    pub request_body: Option<bytes::Bytes>,
    pub dropped: Option<&'static str>,
    pub output: Option<CollectedOutput>,
    pub meta: EventMeta,
}

impl GenAiEvent {
    pub fn approx_bytes(&self) -> usize {
        let body = self.request_body.as_ref().map_or(0, |b| b.len());
        let output: usize = self.output.as_ref().map_or(0, |o| {
            o.choices
                .iter()
                .map(|c| {
                    c.text.len()
                        + c.reasoning.len()
                        + c.tool_calls
                            .iter()
                            .map(|t| t.arguments.len())
                            .sum::<usize>()
                })
                .sum()
        });
        body + output + 1024
    }
}

#[test]
fn surface_operation_name() {
    assert_eq!(ApiSurface::Completions.operation_name(), "text_completion");
    assert_eq!(ApiSurface::Messages.operation_name(), "chat");
    assert_eq!(ApiSurface::Responses.as_str(), "responses");
}
