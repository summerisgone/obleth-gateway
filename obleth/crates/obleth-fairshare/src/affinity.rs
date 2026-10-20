//! Session affinity inside a model pool.
//!
//! An agent session sends its steps one after another, each a longer prompt
//! than the last over the same prefix. The upstream keeps that prefix cached
//! only while other sessions do not push it out, so a step that waits in the
//! queue behind other sessions' steps usually finds it gone and is prefilled
//! from scratch. Affinity keeps a warm session's next step ahead of that:
//!
//! - **Warm**: a session with a step in flight, or whose last step finished
//!   within `warm_ttl`.
//! - **Order within a key**: the oldest waiter of a warm session goes before
//!   older waiters of the same key. Which tenant and which key is served next
//!   is still the fairshare decision; affinity only reorders one key's queue.
//! - **Hold**: when a step finishes whose own arrival came within `hold` of
//!   the session's previous step finishing (an agent loop, not a person
//!   typing or a one-shot call), its slot is held for `hold` so the next
//!   step takes it instead of another session's waiter.
//! - **Jumps**: a session that went ahead of an older waiter `max_jumps`
//!   times in a row loses both privileges until it is served in plain order
//!   again, so one busy session cannot keep the others waiting.
//! - **Aging**: once the pool's oldest waiter has waited `max_bypass`, holds
//!   no longer keep it out and its key's queue is served in arrival order.
//!
//! Sessions are scoped to the scheduler key ([`scoped`]), so two keys sending
//! the same client id never share warmth or holds. State is per pool (a
//! session's cache lives on that model's upstream) and per replica: in
//! shared mode a hold only stops this replica.

use std::collections::HashMap;
use std::time::Instant;

use obleth_config::SessionAffinityConfig;
use uuid::Uuid;

/// The affinity id of a key's session.
pub(crate) fn scoped(key: Uuid, session: &str) -> String {
    format!("{key}:{session}")
}

#[derive(Default)]
struct Session {
    in_flight: usize,
    last_done: Option<Instant>,
    /// Consecutive grants that went ahead of an older waiter.
    jumps: u32,
    /// The latest step arrived within `hold` of the previous one finishing.
    chained: bool,
}

/// A slot held for the next step of the session that freed it.
struct Hold {
    session: String,
    until: Instant,
}

#[derive(Default)]
pub(crate) struct Affinity {
    config: Option<SessionAffinityConfig>,
    sessions: HashMap<String, Session>,
    holds: Vec<Hold>,
}

impl Affinity {
    pub(crate) fn set_config(&mut self, config: Option<SessionAffinityConfig>) {
        self.config = config;
        if config.is_none() {
            self.sessions.clear();
            self.holds.clear();
        }
    }

    fn warm(&self, cfg: &SessionAffinityConfig, session: &str, now: Instant) -> bool {
        self.sessions.get(session).is_some_and(|s| {
            s.in_flight > 0
                || s.last_done
                    .is_some_and(|t| now.saturating_duration_since(t) < cfg.warm_ttl)
        })
    }

    /// Warm and not out of jumps: may go ahead and may hold a slot.
    fn favoured(&self, session: &str, now: Instant) -> bool {
        let Some(cfg) = &self.config else {
            return false;
        };
        self.warm(cfg, session, now)
            && self
                .sessions
                .get(session)
                .is_none_or(|s| s.jumps < cfg.max_jumps)
    }

    /// Whether a waiter queued at `enqueued` has waited long enough to be
    /// served in plain order.
    pub(crate) fn aged(&self, enqueued: Instant, now: Instant) -> bool {
        self.config
            .is_none_or(|cfg| now.saturating_duration_since(enqueued) >= cfg.max_bypass)
    }

    /// Index of the waiter to serve next from one key's queue, given each
    /// waiter's session and arrival time in queue order.
    pub(crate) fn pick<'a>(
        &self,
        waiters: impl Iterator<Item = (Option<&'a str>, Instant)>,
        now: Instant,
    ) -> usize {
        if self.config.is_none() {
            return 0;
        }
        let mut first = true;
        for (i, (session, enqueued)) in waiters.enumerate() {
            if first {
                if self.aged(enqueued, now) {
                    return 0;
                }
                first = false;
            }
            if session.is_some_and(|s| self.favoured(s, now)) {
                return i;
            }
        }
        0
    }

    /// Drop holds that ran out. Returns whether any did.
    pub(crate) fn expire_holds(&mut self, now: Instant) -> bool {
        let before = self.holds.len();
        self.holds.retain(|h| h.until > now);
        self.holds.len() != before
    }

    /// Live slots held for sessions other than `session`.
    pub(crate) fn held_for_others(&self, session: Option<&str>, now: Instant) -> usize {
        self.holds
            .iter()
            .filter(|h| h.until > now && Some(h.session.as_str()) != session)
            .count()
    }

    /// Whether a live slot is held for `session`.
    pub(crate) fn holds_for(&self, session: Option<&str>, now: Instant) -> bool {
        session.is_some_and(|s| self.holds.iter().any(|h| h.until > now && h.session == s))
    }

    /// Give up the slot held for `session`, if any: its step was taken.
    pub(crate) fn take_hold(&mut self, session: Option<&str>) {
        let Some(session) = session else { return };
        if let Some(at) = self.holds.iter().position(|h| h.session == session) {
            self.holds.swap_remove(at);
        }
    }

    /// A step of `session` that arrived at `arrived` got its slot. `jumped`:
    /// affinity put it ahead of an older waiter.
    pub(crate) fn granted(&mut self, session: Option<&str>, jumped: bool, arrived: Instant) {
        let Some(session) = session else { return };
        let Some(cfg) = self.config else { return };
        self.take_hold(Some(session));
        let s = self.sessions.entry(session.to_string()).or_default();
        s.chained = s
            .last_done
            .is_some_and(|t| arrived.saturating_duration_since(t) <= cfg.hold);
        s.in_flight += 1;
        s.jumps = if jumped { s.jumps.saturating_add(1) } else { 0 };
    }

    /// A step of `session` finished. Returns when the slot it held for the
    /// session's next step runs out, if it holds one.
    pub(crate) fn released(&mut self, session: Option<&str>, now: Instant) -> Option<Instant> {
        let session = session?;
        let cfg = self.config?;
        let s = self.sessions.get_mut(session)?;
        s.in_flight = s.in_flight.saturating_sub(1);
        s.last_done = Some(now);
        let chained = s.chained;
        if cfg.hold.is_zero() || !chained || s.in_flight > 0 || !self.favoured(session, now) {
            return None;
        }
        let until = now + cfg.hold;
        self.holds.push(Hold {
            session: session.to_string(),
            until,
        });
        Some(until)
    }

    /// Forget sessions that went cold.
    pub(crate) fn prune(&mut self, now: Instant) {
        let Some(cfg) = self.config else { return };
        self.sessions.retain(|_, s| {
            s.in_flight > 0
                || s.last_done
                    .is_some_and(|t| now.saturating_duration_since(t) < cfg.warm_ttl)
        });
        self.expire_holds(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> SessionAffinityConfig {
        SessionAffinityConfig {
            warm_ttl: Duration::from_secs(120),
            hold: Duration::from_secs(3),
            max_jumps: 2,
            max_bypass: Duration::from_secs(30),
        }
    }

    fn on() -> Affinity {
        let mut a = Affinity::default();
        a.set_config(Some(cfg()));
        a
    }

    /// One whole step of `s` arriving and finishing at `at`.
    fn step(a: &mut Affinity, s: &str, at: Instant) -> Option<Instant> {
        a.granted(Some(s), false, at);
        a.released(Some(s), at)
    }

    #[test]
    fn off_serves_in_order_and_records_nothing() {
        let mut a = Affinity::default();
        let now = Instant::now();
        assert_eq!(step(&mut a, "s", now), None);
        assert_eq!(step(&mut a, "s", now), None);
        assert_eq!(a.pick([(Some("s"), now)].into_iter(), now), 0);
        assert!(a.sessions.is_empty());
    }

    #[test]
    fn warm_session_goes_ahead_within_its_key() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "warm", now);
        let queue = [(Some("cold"), now), (None, now), (Some("warm"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 2);
    }

    #[test]
    fn session_goes_cold_after_the_ttl() {
        let mut a = on();
        let t0 = Instant::now();
        step(&mut a, "s", t0);
        let later = t0 + Duration::from_secs(121);
        let queue = [(Some("other"), later), (Some("s"), later)];
        assert_eq!(a.pick(queue.into_iter(), later), 0);
        a.prune(later);
        assert!(a.sessions.is_empty());
    }

    #[test]
    fn aged_head_is_served_in_order() {
        let mut a = on();
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(30);
        step(&mut a, "warm", now);
        let queue = [(Some("cold"), t0), (Some("warm"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 0);
    }

    #[test]
    fn one_shot_and_slow_sessions_hold_nothing() {
        let mut a = on();
        let t0 = Instant::now();
        // First step: no earlier step to chain to.
        assert_eq!(step(&mut a, "s", t0), None);
        // Next step arrives after more than `hold`: a person, not a loop.
        assert_eq!(step(&mut a, "s", t0 + Duration::from_secs(4)), None);
    }

    #[test]
    fn chained_step_holds_the_slot_for_the_next_one() {
        let mut a = on();
        let now = Instant::now();
        assert_eq!(step(&mut a, "s", now), None);
        let until = step(&mut a, "s", now).expect("held");
        assert_eq!(until, now + Duration::from_secs(3));
        assert!(a.holds_for(Some("s"), now));
        assert_eq!(a.held_for_others(Some("s"), now), 0);
        assert_eq!(a.held_for_others(Some("x"), now), 1);
        assert_eq!(a.held_for_others(None, now), 1);
        assert_eq!(a.held_for_others(None, until), 0);
        a.take_hold(Some("s"));
        assert!(!a.holds_for(Some("s"), now));
    }

    #[test]
    fn hold_runs_out() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        step(&mut a, "s", now).expect("held");
        assert!(!a.expire_holds(now + Duration::from_secs(2)));
        assert!(a.expire_holds(now + Duration::from_secs(3)));
        assert_eq!(a.held_for_others(None, now), 0);
    }

    #[test]
    fn no_hold_while_another_step_is_in_flight() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        a.granted(Some("s"), false, now);
        a.granted(Some("s"), false, now);
        assert_eq!(a.released(Some("s"), now), None);
        assert!(a.released(Some("s"), now).is_some());
    }

    #[test]
    fn out_of_jumps_loses_priority_and_hold_until_served_in_order() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        a.granted(Some("s"), true, now);
        assert!(a.released(Some("s"), now).is_some());
        a.granted(Some("s"), true, now);
        assert_eq!(a.released(Some("s"), now), None);
        let queue = [(Some("cold"), now), (Some("s"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 0);
        a.granted(Some("s"), false, now);
        assert!(a.released(Some("s"), now).is_some());
        assert_eq!(a.pick(queue.into_iter(), now), 1);
    }

    #[test]
    fn sessions_are_scoped_to_their_key() {
        let (k1, k2) = (Uuid::new_v4(), Uuid::new_v4());
        assert_ne!(scoped(k1, "s"), scoped(k2, "s"));
        let mut a = on();
        let now = Instant::now();
        let mine = scoped(k1, "s");
        step(&mut a, &mine, now);
        let queue = [(Some(scoped(k2, "x")), now), (Some(scoped(k2, "s")), now)];
        assert_eq!(
            a.pick(queue.iter().map(|(s, t)| (s.as_deref(), *t)), now),
            0
        );
    }

    #[test]
    fn disabling_forgets_state() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        step(&mut a, "s", now);
        a.set_config(None);
        assert_eq!(a.held_for_others(None, now), 0);
        assert!(a.sessions.is_empty());
    }
}
