//! Session affinity inside a model pool.
//!
//! An agent sends the steps of a session one after another, each over the
//! same and growing prompt. The upstream keeps that prefix cached only while
//! other work does not push it out, so a step that waits behind other
//! sessions' steps often finds it gone and is prefilled again from scratch.
//! With [`SessionAffinityConfig`] set, a pool favours sessions that are warm:
//!
//! - **Warm**: a session with a step in flight in the pool, or whose last
//!   step there finished within `warm_ttl`.
//! - **Order within a key**: the first queued request of a warm session goes
//!   before older requests of the same key. Which tenant and which key is
//!   served next is still the fairshare decision.
//! - **Holds**: when a step finishes that arrived within `hold` of the
//!   session's previous step finishing (an agent loop, not a person typing),
//!   its slot stays taken for `hold` and goes to the session's next step. A
//!   held slot still counts against the pool, the tenant and the key, so it
//!   never lets anyone past a cap or a share, and at most
//!   [`SessionAffinityConfig::max_held`] of a pool's slots are held at once.
//! - **Jumps**: a session that went ahead of an older request `max_jumps`
//!   times in a row, by either route, loses both until it is served in order.
//! - **Aging**: once a request has waited `max_bypass`, its key is served in
//!   arrival order again and its pool's held slots are given up.
//!
//! Sessions are scoped to the scheduler key ([`scoped`]), so two keys sending
//! the same client id never share warmth or a hold.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use obleth_config::SessionAffinityConfig;
use uuid::Uuid;

/// A session's id in the scheduler: the client's or derived id, under the
/// scheduler key that sent it.
pub(crate) fn scoped(key: Uuid, session: &str) -> String {
    format!("{key}/{session}")
}

/// Running totals of what affinity did, for metrics.
#[derive(Debug, Default)]
pub struct AffinityStats {
    /// Requests that went ahead of an older request, by either route.
    pub jumps: AtomicU64,
    /// Slots held for a session's next step.
    pub holds: AtomicU64,
    /// Held slots the session's next step took.
    pub redeemed: AtomicU64,
    /// Held slots given up unused: ran out, or gave way to a request that
    /// had waited too long.
    pub expired: AtomicU64,
}

#[derive(Debug, Default)]
struct Session {
    in_flight: usize,
    last_done: Option<Instant>,
    /// Grants in a row that went ahead of an older request.
    jumps: u32,
    /// The latest step arrived within `hold` of the previous one finishing.
    chained: bool,
}

/// A slot kept taken for a session's next step.
#[derive(Debug)]
pub(crate) struct Hold {
    pub(crate) session: String,
    pub(crate) tenant: Uuid,
    pub(crate) key: Uuid,
    pub(crate) until: Instant,
}

#[derive(Default)]
pub(crate) struct Affinity {
    config: Option<SessionAffinityConfig>,
    sessions: HashMap<String, Session>,
    holds: Vec<Hold>,
    stats: Arc<AffinityStats>,
}

impl Affinity {
    /// Turn affinity on or off. Turning it off forgets every session and
    /// hands back the slots it held, which the caller must release.
    pub(crate) fn configure(
        &mut self,
        config: Option<SessionAffinityConfig>,
        stats: Arc<AffinityStats>,
    ) -> Vec<Hold> {
        self.config = config;
        self.stats = stats;
        if config.is_some() {
            return Vec::new();
        }
        self.sessions.clear();
        std::mem::take(&mut self.holds)
    }

    fn warm(&self, cfg: &SessionAffinityConfig, session: &str, now: Instant) -> bool {
        self.sessions.get(session).is_some_and(|s| {
            s.in_flight > 0
                || s.last_done
                    .is_some_and(|t| now.saturating_duration_since(t) < cfg.warm_ttl)
        })
    }

    /// Warm and not out of jumps: may go ahead, and may have a slot held.
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

    /// Whether a request queued at `enqueued` has waited long enough that
    /// nothing may pass it any more. Always false while affinity is off.
    pub(crate) fn aged(&self, enqueued: Instant, now: Instant) -> bool {
        self.config
            .is_some_and(|cfg| now.saturating_duration_since(enqueued) >= cfg.max_bypass)
    }

    /// Which of one key's queued requests goes next, given each one's
    /// session and arrival in queue order: the first of a favoured session,
    /// unless the head has aged. 0 (the head) while affinity is off.
    pub(crate) fn pick<'a>(
        &self,
        mut waiters: impl Iterator<Item = (Option<&'a str>, Instant)>,
        now: Instant,
    ) -> usize {
        if self.config.is_none() {
            return 0;
        }
        let Some((session, enqueued)) = waiters.next() else {
            return 0;
        };
        if self.aged(enqueued, now) || session.is_some_and(|s| self.favoured(s, now)) {
            return 0;
        }
        waiters
            .position(|(s, _)| s.is_some_and(|s| self.favoured(s, now)))
            .map_or(0, |i| i + 1)
    }

    /// A step of `session` that arrived at `arrived` was granted. `jumped`:
    /// it went ahead of an older request.
    pub(crate) fn granted(&mut self, session: Option<&str>, jumped: bool, arrived: Instant) {
        let (Some(cfg), Some(session)) = (self.config, session) else {
            return;
        };
        if jumped {
            self.stats.jumps.fetch_add(1, Ordering::Relaxed);
        }
        let s = self.sessions.entry(session.to_string()).or_default();
        s.chained = s
            .last_done
            .is_some_and(|t| arrived.saturating_duration_since(t) <= cfg.hold);
        s.in_flight += 1;
        s.jumps = if jumped { s.jumps.saturating_add(1) } else { 0 };
    }

    /// A step of `session` finished. Returns whether the session would like
    /// its slot held: its step chained onto the previous one, nothing else of
    /// it is running, and it is still favoured. Whether the pool can spare
    /// the slot is the caller's call ([`Affinity::can_hold`]).
    pub(crate) fn finished(&mut self, session: Option<&str>, now: Instant) -> bool {
        let (Some(cfg), Some(session)) = (self.config, session) else {
            return false;
        };
        let Some(s) = self.sessions.get_mut(session) else {
            return false;
        };
        s.in_flight = s.in_flight.saturating_sub(1);
        s.last_done = Some(now);
        let wants = s.chained && s.in_flight == 0;
        wants && !cfg.hold.is_zero() && self.favoured(session, now)
    }

    /// Whether a pool of `cap` slots may hold one more.
    pub(crate) fn can_hold(&self, cap: usize) -> bool {
        self.config
            .is_some_and(|cfg| self.holds.len() < cfg.max_held(cap))
    }

    /// Keep a slot for `session`'s next step, until `now + hold`.
    pub(crate) fn hold(&mut self, session: &str, tenant: Uuid, key: Uuid, now: Instant) {
        let Some(cfg) = self.config else {
            return;
        };
        self.stats.holds.fetch_add(1, Ordering::Relaxed);
        self.holds.push(Hold {
            session: session.to_string(),
            tenant,
            key,
            until: now + cfg.hold,
        });
    }

    /// Slots held now.
    pub(crate) fn held(&self) -> usize {
        self.holds.len()
    }

    pub(crate) fn holds(&self) -> impl Iterator<Item = &Hold> {
        self.holds.iter()
    }

    /// Take the slot held for `session`, if there is one: its next step has
    /// it now.
    pub(crate) fn redeem(&mut self, session: Option<&str>) -> Option<Hold> {
        let session = session?;
        let at = self.holds.iter().position(|h| h.session == session)?;
        self.stats.redeemed.fetch_add(1, Ordering::Relaxed);
        Some(self.holds.swap_remove(at))
    }

    /// Give up the holds that ran out by `now`; the caller releases them.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<Hold> {
        if !self.holds.iter().any(|h| h.until <= now) {
            return Vec::new();
        }
        let (gone, kept) = std::mem::take(&mut self.holds)
            .into_iter()
            .partition(|h| h.until <= now);
        self.holds = kept;
        self.count_expired(&gone);
        gone
    }

    /// Give up every hold, e.g. to a request that has waited too long; the
    /// caller releases them.
    pub(crate) fn give_up(&mut self) -> Vec<Hold> {
        let gone = std::mem::take(&mut self.holds);
        self.count_expired(&gone);
        gone
    }

    fn count_expired(&self, gone: &[Hold]) {
        self.stats
            .expired
            .fetch_add(gone.len() as u64, Ordering::Relaxed);
    }

    /// When the next hold runs out.
    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        self.holds.iter().map(|h| h.until).min()
    }

    /// Forget sessions that have gone cold.
    pub(crate) fn prune(&mut self, now: Instant) {
        let Some(cfg) = self.config else {
            return;
        };
        self.sessions.retain(|_, s| {
            s.in_flight > 0
                || s.last_done
                    .is_some_and(|t| now.saturating_duration_since(t) < cfg.warm_ttl)
        });
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
            max_held_percent: 25,
        }
    }

    fn on() -> Affinity {
        let mut a = Affinity::default();
        a.configure(Some(cfg()), Arc::default());
        a
    }

    /// One whole step of `s` arriving and finishing at `at`; whether it
    /// would like its slot held.
    fn step(a: &mut Affinity, s: &str, at: Instant) -> bool {
        a.granted(Some(s), false, at);
        a.finished(Some(s), at)
    }

    #[test]
    fn off_keeps_order_and_state() {
        let mut a = Affinity::default();
        let now = Instant::now();
        assert!(!step(&mut a, "s", now));
        assert!(!step(&mut a, "s", now));
        assert_eq!(a.pick([(None, now), (Some("s"), now)].into_iter(), now), 0);
        assert!(a.sessions.is_empty());
        assert!(!a.aged(now - Duration::from_secs(3600), now));
    }

    #[test]
    fn a_warm_session_goes_first_within_its_key() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "warm", now);
        let queue = [(Some("cold"), now), (None, now), (Some("warm"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 2);
        // Already at the head: nothing to jump.
        let queue = [(Some("warm"), now), (Some("cold"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 0);
    }

    #[test]
    fn a_session_goes_cold_after_the_warm_window() {
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
    fn an_aged_head_is_served_first() {
        let mut a = on();
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(30);
        step(&mut a, "warm", now);
        let queue = [(Some("cold"), t0), (Some("warm"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 0);
    }

    #[test]
    fn only_a_chained_step_asks_for_a_hold() {
        let mut a = on();
        let t0 = Instant::now();
        assert!(
            !step(&mut a, "s", t0),
            "a first step has nothing to chain to"
        );
        assert!(
            !step(&mut a, "s", t0 + Duration::from_secs(4)),
            "a step that came later than the hold is a person, not a loop"
        );
        assert!(step(&mut a, "s", t0 + Duration::from_secs(5)));
    }

    #[test]
    fn no_hold_while_another_step_of_the_session_runs() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        a.granted(Some("s"), false, now);
        a.granted(Some("s"), false, now);
        assert!(!a.finished(Some("s"), now));
        assert!(a.finished(Some("s"), now));
    }

    #[test]
    fn holds_are_bounded_redeemed_and_expire() {
        let mut a = on();
        let now = Instant::now();
        let (t, k) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert!(a.can_hold(4), "25% of 4 is one slot");
        a.hold("s", t, k, now);
        assert!(!a.can_hold(4));
        assert!(a.can_hold(8));
        assert_eq!(a.next_expiry(), Some(now + Duration::from_secs(3)));
        assert!(a.redeem(Some("other")).is_none());
        let h = a.redeem(Some("s")).expect("held for s");
        assert_eq!((h.tenant, h.key), (t, k));
        a.hold("s", t, k, now);
        assert!(a.expire(now + Duration::from_secs(2)).is_empty());
        assert_eq!(a.expire(now + Duration::from_secs(3)).len(), 1);
        assert_eq!(a.held(), 0);
        assert_eq!(a.stats.holds.load(Ordering::Relaxed), 2);
        assert_eq!(a.stats.redeemed.load(Ordering::Relaxed), 1);
        assert_eq!(a.stats.expired.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn out_of_jumps_waits_its_turn_until_served_in_order() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        a.granted(Some("s"), true, now);
        assert!(a.finished(Some("s"), now));
        a.granted(Some("s"), true, now);
        assert!(!a.finished(Some("s"), now), "out of jumps: no hold");
        let queue = [(Some("cold"), now), (Some("s"), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 0, "out of jumps: no jump");
        a.granted(Some("s"), false, now);
        assert!(a.finished(Some("s"), now));
        assert_eq!(a.pick(queue.into_iter(), now), 1);
        assert_eq!(a.stats.jumps.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn sessions_are_scoped_to_their_key() {
        let (k1, k2) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert_ne!(scoped(k1, "s"), scoped(k2, "s"));
        let mut a = on();
        let now = Instant::now();
        step(&mut a, &scoped(k1, "s"), now);
        let (x, s) = (scoped(k2, "x"), scoped(k2, "s"));
        let queue = [(Some(x.as_str()), now), (Some(s.as_str()), now)];
        assert_eq!(a.pick(queue.into_iter(), now), 0);
    }

    #[test]
    fn turning_off_hands_back_the_holds() {
        let mut a = on();
        let now = Instant::now();
        step(&mut a, "s", now);
        a.hold("s", Uuid::nil(), Uuid::nil(), now);
        assert_eq!(a.configure(None, Arc::default()).len(), 1);
        assert_eq!(a.held(), 0);
        assert!(a.sessions.is_empty());
    }
}
