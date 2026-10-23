//! Data-plane side of `auto` conversation stickiness: the store lookups, the
//! complaint question, and recording the turn once it has been answered. The
//! decisions themselves are pure and live in [`obleth_config::routing::sticky`].

use axum::body::Body;
use axum::http::{HeaderValue, Response};
use obleth_config::routing::sticky::{self, Decision, Observed, Reason, TurnRecord};
use obleth_config::routing::{Candidate, Intent};
use uuid::Uuid;

use crate::state::AppState;

/// Response header naming the model `auto` served.
pub(crate) const AUTO_MODEL_HEADER: &str = "x-obleth-auto-model";
/// Response header saying why, when stickiness is on (see [`Reason`]).
pub(crate) const AUTO_REASON_HEADER: &str = "x-obleth-auto-reason";

/// Value stored under a retry key: the request was answered.
const ANSWERED: &str = "1";

/// What the store and the request say about the conversation so far.
pub(crate) struct Conversation {
    pub record: Option<TurnRecord>,
    pub observed: Observed,
    retry_key: Option<String>,
    user: String,
}

/// Find the conversation's incumbent, and whether this exact request was
/// already answered. A store error reads as a new conversation.
pub(crate) async fn lookup(
    state: &AppState,
    tenant: &Uuid,
    json: &serde_json::Value,
) -> Conversation {
    let retry_key = sticky::retry_key(json);
    let (record, seen) = match sticky::incumbent_key(json) {
        Some(key) => {
            let (record, seen) = tokio::join!(state.redis.auto_turn_get(tenant, &key), async {
                match &retry_key {
                    Some(k) => state.redis.auto_turn_get(tenant, k).await,
                    None => Ok(None),
                }
            });
            let record = record
                .map_err(|e| tracing::debug!(error = %e, "auto turn lookup failed"))
                .ok()
                .flatten()
                .and_then(|raw| serde_json::from_str::<TurnRecord>(&raw).ok());
            (record, matches!(seen, Ok(Some(_))))
        }
        None => (None, false),
    };
    Conversation {
        observed: sticky::observe(json, seen && record.is_some()),
        record,
        retry_key,
        user: sticky::newest_user_text(json),
    }
}

/// Ask the classifier whether the user's newest message says the last answer
/// was wrong. Only asked when there is an incumbent to judge and the user, not
/// a tool, spoke last; `false` whenever the classifier cannot answer.
pub(crate) async fn complaint(
    state: &AppState,
    settings: &obleth_config::AutoRouterSettings,
    conversation: Option<&Conversation>,
    json: &serde_json::Value,
    skip_classifier: bool,
) -> bool {
    let Some(conversation) = conversation else {
        return false;
    };
    if conversation.record.is_none()
        || skip_classifier
        || !settings.classifier_active()
        || !sticky::user_spoke_last(json)
    {
        return false;
    }
    let Some(name) = settings.classifier_model.as_deref() else {
        return false;
    };
    if name == crate::router::AUTO_MODEL_NAME {
        return false;
    }
    let Some(brain) = crate::proxy::resolve_model(state, name).await else {
        return false;
    };
    let message = conversation.user.trim();
    if message.is_empty() {
        return false;
    }
    state
        .classifier
        .complaint(&state.http, &brain, message)
        .await
        .is_some_and(|p| p >= sticky::COMPLAINT_THRESHOLD)
}

/// The candidates a sticky turn is routed over, and why.
pub(crate) struct Choice {
    /// `None` routes over every candidate.
    pub pool: Option<Vec<Candidate>>,
    pub reason: Reason,
    pub floor: u8,
    pub domains: Vec<String>,
    pub strikes: u8,
}

/// Narrow the candidates for this turn. `serves` says whether a pool would
/// produce a pick for this request (the router's hard filters), so the floor
/// can clamp down past models that cannot take it.
pub(crate) fn choose(
    candidates: &[Candidate],
    record: Option<&TurnRecord>,
    intent: &Intent,
    observed: &Observed,
    serves: impl Fn(&[Candidate]) -> bool,
) -> Choice {
    let Some(record) = record else {
        return Choice {
            pool: None,
            reason: Reason::Fresh,
            floor: 0,
            domains: sticky::domains_of(&intent.tags),
            strikes: 0,
        };
    };
    let plan = sticky::decide(record, intent, observed);
    let incumbent = candidates
        .iter()
        .find(|c| c.model.model_name == record.model);
    let choice = |pool, reason| Choice {
        pool,
        reason,
        floor: plan.floor,
        domains: plan.domains.clone(),
        strikes: plan.strikes,
    };

    // The incumbent stays whenever the evidence allows it and it can take the
    // request: on a quiet turn, and on a harder one it is strong enough for.
    let keepable = match plan.decision {
        Decision::Keep => true,
        Decision::Harder => incumbent.is_some_and(|c| c.strength(&plan.domains) >= plan.floor),
        Decision::Escalate(_) | Decision::Topic => false,
    };
    if keepable {
        if let Some(c) = incumbent {
            let only = std::slice::from_ref(c);
            if serves(only) {
                return choice(Some(only.to_vec()), Reason::Kept);
            }
        }
    }
    let reason = match plan.decision {
        Decision::Keep => Reason::Forced,
        Decision::Escalate(signal) => Reason::Escalated(signal),
        Decision::Harder => Reason::Harder,
        Decision::Topic => Reason::Topic,
    };
    let exclude = matches!(plan.decision, Decision::Escalate(_)).then_some(record.model.as_str());
    for floor in (0..=plan.floor).rev() {
        let pool = sticky::floor_pool(candidates, &plan.domains, floor, exclude);
        if !pool.is_empty() && serves(&pool) {
            return choice(Some(pool), reason);
        }
    }
    // Nothing else can take it: route as usual, which may keep the incumbent.
    choice(None, reason)
}

/// A routed turn waiting for its answer, to be stored once it is served.
pub(crate) struct Turn {
    tenant: Uuid,
    user: String,
    retry_key: Option<String>,
    record: TurnRecord,
}

impl Turn {
    pub(crate) fn new(
        tenant: Uuid,
        conversation: Conversation,
        choice: &Choice,
        chosen: &Candidate,
    ) -> Self {
        let rung = chosen.strength(&choice.domains).max(choice.floor);
        Turn {
            tenant,
            user: conversation.user,
            retry_key: conversation.retry_key,
            record: TurnRecord {
                model: chosen.model.model_name.clone(),
                domains: choice.domains.clone(),
                rung,
                strikes: choice.strikes,
                bad_output: false,
            },
        }
    }

    /// Store the turn under the answer the client received, and mark the
    /// request answered. Runs in the background; a store error only costs the
    /// next turn its incumbent.
    pub(crate) fn record(self, state: &AppState, completion: &serde_json::Value) {
        let Some(answer) = completion.pointer("/choices/0/message") else {
            return;
        };
        let key = sticky::turn_key(&self.user, answer);
        let mut record = self.record;
        record.bad_output = sticky::bad_output(completion);
        let Ok(value) = serde_json::to_string(&record) else {
            return;
        };
        let redis = state.redis.clone();
        let tenant = self.tenant;
        let retry_key = self.retry_key;
        tokio::spawn(async move {
            let ttl = sticky::STICKY_TTL_SECS;
            if let Err(e) = redis.auto_turn_put(&tenant, &key, &value, ttl).await {
                tracing::debug!(error = %e, "auto turn store failed");
            }
            if let Some(retry) = retry_key {
                let _ = redis.auto_turn_put(&tenant, &retry, ANSWERED, ttl).await;
            }
        });
    }
}

/// How an `auto` request was routed, for the response headers.
pub(crate) struct Note {
    pub model: String,
    /// `None` when stickiness is off.
    pub reason: Option<Reason>,
}

impl Note {
    pub(crate) fn apply_headers(&self, resp: &mut Response<Body>) {
        if let Ok(v) = HeaderValue::from_str(&self.model) {
            resp.headers_mut().insert(AUTO_MODEL_HEADER, v);
        }
        if let Some(reason) = self.reason {
            resp.headers_mut().insert(
                AUTO_REASON_HEADER,
                HeaderValue::from_static(reason.as_str()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obleth_config::routing::IntentSource;

    fn candidate(name: &str, level: u8) -> Candidate {
        let mut model = crate::boons::test_support::test_route();
        model.model_name = name.to_string();
        Candidate {
            model,
            healthy: true,
            levels: vec![("coding".into(), level)],
        }
    }

    fn record(model: &str, rung: u8) -> TurnRecord {
        TurnRecord {
            model: model.into(),
            domains: vec!["coding".into()],
            rung,
            strikes: 0,
            bad_output: false,
        }
    }

    fn coding(difficulty: u8) -> Intent {
        Intent {
            tags: vec!["coding".into()],
            difficulty,
            source: IntentSource::Classifier,
        }
    }

    fn names(choice: &Choice) -> Vec<&str> {
        choice
            .pool
            .as_ref()
            .map(|p| p.iter().map(|c| c.model.model_name.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_quiet_turn_keeps_the_incumbent_even_when_others_are_free() {
        let all = vec![
            candidate("small", 1),
            candidate("mid", 2),
            candidate("big", 3),
        ];
        let choice = choose(
            &all,
            Some(&record("mid", 2)),
            &coding(1),
            &Observed::default(),
            |_| true,
        );
        assert_eq!(choice.reason, Reason::Kept);
        assert_eq!(names(&choice), ["mid"]);
    }

    #[test]
    fn a_complaint_moves_up_and_away_from_the_incumbent() {
        let all = vec![
            candidate("small", 1),
            candidate("mid", 2),
            candidate("big", 3),
        ];
        let observed = Observed {
            complaint: true,
            ..Default::default()
        };
        let choice = choose(
            &all,
            Some(&record("small", 1)),
            &coding(1),
            &observed,
            |_| true,
        );
        assert_eq!(choice.reason, Reason::Escalated(sticky::Signal::Complaint));
        assert_eq!(names(&choice), ["mid", "big"]);

        // At the top, the move is sideways to another top model.
        let tops = vec![
            candidate("big-a", 3),
            candidate("big-b", 3),
            candidate("mid", 2),
        ];
        let choice = choose(
            &tops,
            Some(&record("big-a", 3)),
            &coding(1),
            &observed,
            |_| true,
        );
        assert_eq!(names(&choice), ["big-b"]);
    }

    #[test]
    fn an_incumbent_that_cannot_serve_is_replaced_at_its_rung_or_above() {
        let all = vec![
            candidate("small", 1),
            candidate("mid", 2),
            candidate("big", 3),
        ];
        let serves = |pool: &[Candidate]| pool.iter().any(|c| c.model.model_name != "mid");
        let choice = choose(
            &all,
            Some(&record("mid", 2)),
            &coding(1),
            &Observed::default(),
            serves,
        );
        assert_eq!(choice.reason, Reason::Forced);
        assert_eq!(names(&choice), ["mid", "big"]);
    }

    #[test]
    fn the_floor_clamps_down_when_nothing_at_it_can_serve() {
        let all = vec![
            candidate("small", 1),
            candidate("mid", 2),
            candidate("big", 3),
        ];
        let observed = Observed {
            regenerate: true,
            ..Default::default()
        };
        // Only "small" can take this request (say, the context is too long
        // for the others).
        let serves = |pool: &[Candidate]| pool.iter().any(|c| c.model.model_name == "small");
        let choice = choose(&all, Some(&record("mid", 2)), &coding(1), &observed, serves);
        assert_eq!(names(&choice), ["small", "big"]);
    }

    #[test]
    fn a_harder_turn_keeps_an_incumbent_strong_enough_for_it() {
        let all = vec![candidate("mid", 2), candidate("big", 3)];
        let strong = choose(
            &all,
            Some(&record("big", 1)),
            &coding(3),
            &Observed::default(),
            |_| true,
        );
        assert_eq!(strong.reason, Reason::Kept);
        let weak = choose(
            &all,
            Some(&record("mid", 2)),
            &coding(3),
            &Observed::default(),
            |_| true,
        );
        assert_eq!(weak.reason, Reason::Harder);
        assert_eq!(names(&weak), ["big"]);
    }

    #[test]
    fn the_new_record_never_drops_below_the_floor() {
        let small = candidate("small", 1);
        let conversation = Conversation {
            record: Some(record("big", 3)),
            observed: Observed::default(),
            retry_key: None,
            user: "now write it up".into(),
        };
        let choice = Choice {
            pool: None,
            reason: Reason::Topic,
            floor: 3,
            domains: vec!["coding".into()],
            strikes: 0,
        };
        let turn = Turn::new(Uuid::nil(), conversation, &choice, &small);
        assert_eq!(turn.record.rung, 3);
        assert_eq!(turn.record.model, "small");
    }
}
