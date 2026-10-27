//! Prompt construction for verdicts questions.
//!
//! Every question in a request shares a byte-identical system message carrying
//! the state, and differs only in the user message carrying the question. With
//! all questions pinned to one upstream target and identical sampling params,
//! the shared prefix makes each additional question roughly one prefix-cache
//! hit plus a single decoded token on vLLM/SGLang. Images, when a request has
//! them, open every question's user message, ahead of the question text, so
//! they stay inside that shared prefix.
//!
//! Answer labels are chosen to be single tokens by construction — `yes`/`no`
//! for boolean, single letters for choice and score — so the first generated
//! token's `top_logprobs` is the entire answer distribution. Digits are
//! deliberately not used for score levels: `"10"` is two tokens on
//! SentencePiece vocabularies and `1` is an ambiguous prefix of it.
//!
//! A choice with more options than letters gets two-letter codes (`AA`, `AB`,
//! …) instead. Large vocabularies hold every such pair as one token, so the
//! first token is still the whole answer; where a vocabulary splits a code,
//! the first token is its first letter and a continuation call reads the
//! second (see [`build_body_continued`]).

use super::types::{BooleanCriteria, Question, MAX_LETTER_OPTIONS};

/// Letters used for choice options and score levels, in assignment order.
const LETTERS: [char; 26] = [
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

/// What the answer labels of one question mean.
pub(crate) enum LabelSemantics {
    /// Labels are `["yes", "no"]`.
    Boolean,
    /// Labels are letters; index i maps to this option name.
    Choice(Vec<String>),
    /// Labels are letters; index i maps to level i+1.
    Score,
}

/// The answer labels for one question plus what they map back to.
pub(crate) struct LabelSet {
    pub labels: Vec<String>,
    pub semantics: LabelSemantics,
}

/// Render a state or instructions value into prompt text: strings verbatim,
/// structured values as pretty-printed JSON.
pub(crate) fn render_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// The shared system message. Byte-identical across every question of one
/// request — this is the prefix the backend's cache amortizes. A request that
/// sends only images has no state text; the images stand in for it.
pub(crate) fn render_system(state: &serde_json::Value) -> String {
    let state = match state {
        serde_json::Value::Null => "The attached images.".to_string(),
        other => render_value(other),
    };
    format!(
        "You answer classification questions about the STATE below. Reply with exactly one \
         answer label and nothing else — no explanation, no punctuation, no preamble.\n\n\
         # State\n{state}"
    )
}

/// Assign answer labels to a question. Choice options take letters in the
/// map's (sorted) iteration order; the order is part of the documented API.
/// Past 26 options every option takes a two-letter code in the same order:
/// `AA`..`AZ`, `BA`..`BZ`, and so on.
pub(crate) fn labels_for(q: &Question) -> LabelSet {
    match q {
        Question::Boolean { .. } => LabelSet {
            labels: vec!["yes".to_string(), "no".to_string()],
            semantics: LabelSemantics::Boolean,
        },
        Question::Choice { criteria, .. } => {
            let options: Vec<String> = criteria.keys().cloned().collect();
            let two_letter = options.len() > MAX_LETTER_OPTIONS;
            LabelSet {
                labels: options
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        if two_letter {
                            format!("{}{}", LETTERS[i / 26], LETTERS[i % 26])
                        } else {
                            LETTERS[i].to_string()
                        }
                    })
                    .collect(),
                semantics: LabelSemantics::Choice(options),
            }
        }
        Question::Score { criteria, .. } => LabelSet {
            labels: (0..criteria.len())
                .map(|i| LETTERS[i].to_string())
                .collect(),
            semantics: LabelSemantics::Score,
        },
    }
}

/// The per-question user message: instructions, criteria, and the answer
/// instruction naming the label format.
pub(crate) fn render_user(q: &Question, labels: &LabelSet) -> String {
    match q {
        Question::Boolean {
            instructions,
            criteria,
        } => {
            let mut out = format!("# Question\n{}\n", render_value(instructions));
            let (yes, no) = match criteria {
                Some(BooleanCriteria { yes, no }) => (yes.as_ref(), no.as_ref()),
                None => (None, None),
            };
            if yes.is_some() || no.is_some() {
                out.push_str("\n# Answer meanings\n");
                if let Some(y) = yes {
                    out.push_str(&format!("yes — {}\n", render_value(y)));
                }
                if let Some(n) = no {
                    out.push_str(&format!("no — {}\n", render_value(n)));
                }
            }
            out.push_str("\nAnswer \"yes\" or \"no\".");
            out
        }
        Question::Choice {
            instructions,
            criteria,
        } => {
            let mut out = format!("# Question\n{}\n\n# Options\n", render_value(instructions));
            for (label, (option, description)) in labels.labels.iter().zip(criteria.iter()) {
                match description {
                    serde_json::Value::Null => out.push_str(&format!("{label}) {option}\n")),
                    d => out.push_str(&format!("{label}) {option} — {}\n", render_value(d))),
                }
            }
            if is_two_letter(&labels.labels) {
                out.push_str("\nAnswer with the two-letter code of the best option.");
            } else {
                out.push_str("\nAnswer with the letter of the best option.");
            }
            out
        }
        Question::Score {
            instructions,
            criteria,
        } => {
            let mut out = format!("# Question\n{}\n\n# Levels\n", render_value(instructions));
            for (i, (label, description)) in labels.labels.iter().zip(criteria.iter()).enumerate() {
                out.push_str(&format!(
                    "{label}) level {} — {}\n",
                    i + 1,
                    render_value(description)
                ));
            }
            out.push_str("\nAnswer with the letter of the level that best matches.");
            out
        }
    }
}

/// Whether a label set is two-letter codes (a choice past 26 options).
pub(crate) fn is_two_letter(labels: &[String]) -> bool {
    labels
        .first()
        .is_some_and(|l| l.len() == 2 && l.bytes().all(|b| b.is_ascii_uppercase()))
}

/// The user message's content: the question text alone, or the images
/// followed by the question text. The images come first so the part of the
/// prompt every question shares runs through them.
pub(crate) fn user_content(user: &str, images: &[String]) -> serde_json::Value {
    if images.is_empty() {
        return serde_json::json!(user);
    }
    let mut parts: Vec<serde_json::Value> = images
        .iter()
        .map(|url| serde_json::json!({"type": "image_url", "image_url": {"url": url}}))
        .collect();
    parts.push(serde_json::json!({"type": "text", "text": user}));
    serde_json::Value::Array(parts)
}

/// The upstream chat-completions body for one question: greedy, one token,
/// full first-token distribution. `stream:false` — the answer is one token,
/// there is nothing to stream.
pub(crate) fn build_body(
    upstream_model: &str,
    system: &str,
    user: &str,
    images: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "model": upstream_model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_content(user, images)},
        ],
        "max_tokens": 1,
        "temperature": 0,
        "logprobs": true,
        "top_logprobs": 20,
        "stream": false,
    })
}

/// The same call with the assistant turn pre-opened by an empty think block.
///
/// Reasoning models (GLM, Qwen thinking variants, DeepSeek-R1 templates)
/// spend their first token inside a `<think>` scratchpad, so the plain call's
/// first-token distribution is thinking prose, not an answer label. Seeding
/// the assistant message with a closed, empty `<think></think>` and asking
/// the backend to continue it (`continue_final_message`, supported by vLLM
/// and SGLang) moves the first generated token past the scratchpad. Verified
/// against GLM: the top tokens become the answer labels. Only ever sent after
/// the plain call produced zero label mass, so backends that reject the
/// fields (llama.cpp, SaaS APIs) never see them on a working model.
pub(crate) fn build_body_prefilled(
    upstream_model: &str,
    system: &str,
    user: &str,
    images: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "model": upstream_model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_content(user, images)},
            {"role": "assistant", "content": "<think></think>"},
        ],
        "add_generation_prompt": false,
        "continue_final_message": true,
        "max_tokens": 1,
        "temperature": 0,
        "logprobs": true,
        "top_logprobs": 20,
        "stream": false,
    })
}

/// The call that reads a two-letter code's second letter: the assistant turn
/// is pre-opened with the code's first letter (after the empty think block
/// when the model needs it) and continued, so the first generated token is
/// the second letter. Sent only when the first call's mass landed on a bare
/// first letter, i.e. on a vocabulary that splits the code.
pub(crate) fn build_body_continued(
    upstream_model: &str,
    system: &str,
    user: &str,
    images: &[String],
    think: bool,
    first: char,
) -> serde_json::Value {
    let opened = if think {
        format!("<think></think>{first}")
    } else {
        first.to_string()
    };
    serde_json::json!({
        "model": upstream_model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user_content(user, images)},
            {"role": "assistant", "content": opened},
        ],
        "add_generation_prompt": false,
        "continue_final_message": true,
        "max_tokens": 1,
        "temperature": 0,
        "logprobs": true,
        "top_logprobs": 20,
        "stream": false,
    })
}

/// [`build_body_continued`] for the harmony splice: the open final channel
/// already holds the code's first letter.
pub(crate) fn build_body_spliced_continued(
    upstream_model: &str,
    system: &str,
    user: &str,
    first: char,
) -> serde_json::Value {
    let mut body = build_body_spliced(upstream_model, system, user);
    if let Some(prompt) = body.get_mut("prompt") {
        let opened = format!("{}{first}", prompt.as_str().unwrap_or_default());
        *prompt = serde_json::json!(opened);
    }
    body
}

/// The same call as a raw `/v1/completions` body with a hand-rendered
/// harmony prompt whose assistant turn is already open in the **final**
/// channel.
///
/// gpt-oss-family models keep their scratchpad in harmony channels, not a
/// `<think>` block: the plain chat call's first token is the `<|channel|>`
/// control token (deterministically — all the mass), and vLLM's harmony
/// renderer ignores `continue_final_message`, so the chat-side prefill can't
/// help. Splicing the prompt ourselves and ending it with
/// `<|start|>assistant<|channel|>final<|message|>` puts the first sampled
/// token directly on the answer labels — verified against gpt-oss-120b on
/// vLLM. The system/user texts are identical to the chat variant, so the
/// per-request prefix-cache sharing survives the format change. Only ever
/// sent after the plain call's top token was a `<|…|>` control token, so
/// non-harmony backends never see it. Text only: a request with images never
/// takes this path.
pub(crate) fn build_body_spliced(
    upstream_model: &str,
    system: &str,
    user: &str,
) -> serde_json::Value {
    let prompt = format!(
        "<|start|>system<|message|>{system}<|end|>\
         <|start|>user<|message|>{user}<|end|>\
         <|start|>assistant<|channel|>final<|message|>"
    );
    serde_json::json!({
        "model": upstream_model,
        "prompt": prompt,
        "max_tokens": 1,
        "temperature": 0,
        "logprobs": 20,
        "stream": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn choice(options: &[(&str, serde_json::Value)]) -> Question {
        Question::Choice {
            instructions: serde_json::json!("pick one"),
            criteria: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn the_system_message_is_byte_identical_across_questions() {
        // The prefix-cache contract: the state renders once, identically,
        // regardless of which question follows.
        let state = serde_json::json!({"ticket": "duplicate charge", "order": "A-104"});
        assert_eq!(render_system(&state), render_system(&state));
        // Strings render verbatim; structured state renders as pretty JSON.
        let text = render_system(&serde_json::json!("plain text state"));
        assert!(text.ends_with("# State\nplain text state"));
        let structured = render_system(&state);
        assert!(structured.contains("\"order\": \"A-104\""));
        // An images-only request names the images as the state.
        assert!(render_system(&serde_json::Value::Null).ends_with("# State\nThe attached images."));
    }

    #[test]
    fn choice_labels_follow_sorted_option_order() {
        // BTreeMap iteration is sorted; label assignment is therefore
        // deterministic and independent of client key order.
        let q = choice(&[
            ("zebra", serde_json::json!("z")),
            ("alpha", serde_json::json!("a")),
            ("mid", serde_json::json!(null)),
        ]);
        let labels = labels_for(&q);
        let LabelSemantics::Choice(options) = &labels.semantics else {
            panic!("expected choice semantics");
        };
        assert_eq!(options, &["alpha", "mid", "zebra"]);
        assert_eq!(labels.labels, ["A", "B", "C"]);

        let user = render_user(&q, &labels);
        assert!(user.contains("A) alpha — a"));
        // A null description renders the bare option, no dangling dash.
        assert!(user.contains("B) mid\n"));
        assert!(user.contains("C) zebra — z"));
        assert!(user.contains("letter of the best option"));
    }

    #[test]
    fn boolean_renders_criteria_and_the_yes_no_instruction() {
        let q = Question::Boolean {
            instructions: serde_json::json!("Is it urgent?"),
            criteria: Some(BooleanCriteria {
                yes: Some(serde_json::json!("time-sensitive")),
                no: Some(serde_json::json!("can wait")),
            }),
        };
        let labels = labels_for(&q);
        assert_eq!(labels.labels, ["yes", "no"]);
        let user = render_user(&q, &labels);
        assert!(user.contains("yes — time-sensitive"));
        assert!(user.contains("no — can wait"));
        assert!(user.contains("Answer \"yes\" or \"no\"."));
    }

    #[test]
    fn score_levels_use_letters_not_digits() {
        // Digits are multi-token / prefix-ambiguous ("1" vs "10") on
        // SentencePiece vocabs; letters are single tokens everywhere.
        let q = Question::Score {
            instructions: serde_json::json!("frustration?"),
            criteria: (0..10)
                .map(|i| serde_json::json!(format!("level-{i}")))
                .collect(),
        };
        let labels = labels_for(&q);
        assert_eq!(labels.labels.len(), 10);
        assert_eq!(labels.labels[9], "J");
        let user = render_user(&q, &labels);
        assert!(user.contains("J) level 10 — level-9"));
    }

    #[test]
    fn the_upstream_body_is_greedy_single_token_with_full_logprobs() {
        let body = build_body("qwen", "sys", "user", &[]);
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["logprobs"], true);
        assert_eq!(body["top_logprobs"], 20);
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        // Without images the user content stays a plain string.
        assert_eq!(body["messages"][1]["content"], "user");
    }

    #[test]
    fn images_open_the_user_message_ahead_of_the_question() {
        let images = vec![
            "data:image/png;base64,AAAA".to_string(),
            "https://example.com/b.jpg".to_string(),
        ];
        for body in [
            build_body("m", "sys", "the question", &images),
            build_body_prefilled("m", "sys", "the question", &images),
        ] {
            let parts = body["messages"][1]["content"].as_array().expect("parts");
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[0]["type"], "image_url");
            assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AAAA");
            assert_eq!(parts[1]["image_url"]["url"], "https://example.com/b.jpg");
            assert_eq!(
                parts[2],
                serde_json::json!({"type": "text", "text": "the question"})
            );
            // The state stays a plain system string.
            assert_eq!(body["messages"][0]["content"], "sys");
        }
        // Two questions share everything up to their text part.
        let a = build_body("m", "sys", "first", &images);
        let b = build_body("m", "sys", "second", &images);
        assert_eq!(a["messages"][0], b["messages"][0]);
        assert_eq!(
            a["messages"][1]["content"].as_array().unwrap()[..2],
            b["messages"][1]["content"].as_array().unwrap()[..2]
        );
    }

    #[test]
    fn the_spliced_body_opens_the_final_channel_and_reuses_the_chat_texts() {
        let body = build_body_spliced("gpt-oss-120b", "sys text", "user text");
        let prompt = body["prompt"].as_str().unwrap();
        assert!(prompt.starts_with("<|start|>system<|message|>sys text<|end|>"));
        assert!(prompt.contains("<|start|>user<|message|>user text<|end|>"));
        // The assistant turn is open in the final channel: the next sampled
        // token is the answer, not a channel opener.
        assert!(prompt.ends_with("<|start|>assistant<|channel|>final<|message|>"));
        // Completions-endpoint spelling: `logprobs` is the top-N int.
        assert_eq!(body["logprobs"], 20);
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["temperature"], 0);
        assert!(body.get("messages").is_none());
    }

    #[test]
    fn structured_instructions_render_as_pretty_json() {
        let q = Question::Boolean {
            instructions: serde_json::json!({
                "question": "Same person as `candidate`?",
                "candidate": {"name": "John Smith"}
            }),
            criteria: None,
        };
        let user = render_user(&q, &labels_for(&q));
        assert!(user.contains("\"name\": \"John Smith\""));
    }

    fn options(n: usize) -> Question {
        let names: Vec<(String, serde_json::Value)> = (0..n)
            .map(|i| (format!("opt{i:03}"), serde_json::Value::Null))
            .collect();
        Question::Choice {
            instructions: serde_json::json!("pick one"),
            criteria: names.into_iter().collect(),
        }
    }

    #[test]
    fn up_to_26_options_keep_single_letters_and_the_letter_instruction() {
        let q = options(26);
        let labels = labels_for(&q);
        assert_eq!(labels.labels.first().unwrap(), "A");
        assert_eq!(labels.labels.last().unwrap(), "Z");
        assert!(!is_two_letter(&labels.labels));
        let user = render_user(&q, &labels);
        assert!(user.contains("Z) opt025\n"));
        assert!(user.ends_with("\nAnswer with the letter of the best option."));
    }

    #[test]
    fn past_26_options_every_option_takes_a_two_letter_code() {
        let q = options(151);
        let labels = labels_for(&q);
        assert!(is_two_letter(&labels.labels));
        assert_eq!(labels.labels[0], "AA");
        assert_eq!(labels.labels[25], "AZ");
        assert_eq!(labels.labels[26], "BA");
        assert_eq!(labels.labels[150], "FU");
        let user = render_user(&q, &labels);
        assert!(user.contains("AA) opt000\n"));
        assert!(user.contains("FU) opt150\n"));
        assert!(user.ends_with("\nAnswer with the two-letter code of the best option."));
        let max = labels_for(&options(255));
        assert_eq!(max.labels.last().unwrap(), "JU");
    }

    #[test]
    fn boolean_and_score_labels_are_never_two_letter_codes() {
        let boolean = Question::Boolean {
            instructions: serde_json::json!("?"),
            criteria: None,
        };
        assert!(!is_two_letter(&labels_for(&boolean).labels));
        let score = Question::Score {
            instructions: serde_json::json!("?"),
            criteria: vec![serde_json::json!("low"), serde_json::json!("high")],
        };
        assert!(!is_two_letter(&labels_for(&score).labels));
    }

    #[test]
    fn the_continued_body_opens_the_answer_with_the_first_letter() {
        let body = build_body_continued("m", "sys", "user", &[], false, 'B');
        assert_eq!(
            body["messages"][2],
            serde_json::json!({"role": "assistant", "content": "B"})
        );
        assert_eq!(body["continue_final_message"], true);
        assert_eq!(body["add_generation_prompt"], false);
        assert_eq!(body["max_tokens"], 1);
        let think = build_body_continued("m", "sys", "user", &[], true, 'C');
        assert_eq!(think["messages"][2]["content"], "<think></think>C");
        let spliced = build_body_spliced_continued("m", "sys", "user", 'D');
        assert!(spliced["prompt"]
            .as_str()
            .unwrap()
            .ends_with("<|start|>assistant<|channel|>final<|message|>D"));
    }
}
