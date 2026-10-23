//! Session affinity: a warm session's next step goes first within its key,
//! and a looping session's slot is held for its next step.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use obleth_config::{Admission, FairshareAlgorithm, SessionAffinityConfig};
use obleth_fairshare::{AdmitRequest, Admitted, FairShare, StaticCapacity};
use tokio::task::JoinHandle;
use uuid::Uuid;

const MODEL: &str = "m";

fn config(hold_ms: u64) -> SessionAffinityConfig {
    SessionAffinityConfig {
        warm_ttl: Duration::from_secs(120),
        hold: Duration::from_millis(hold_ms),
        max_jumps: 8,
        max_bypass: Duration::from_secs(30),
        max_held_percent: 100,
    }
}

fn start(affinity: Option<SessionAffinityConfig>) -> FairShare {
    let fs = FairShare::start(
        Arc::new(StaticCapacity::new(64)),
        FairshareAlgorithm::Weighted,
        8,
    );
    fs.set_session_affinity(affinity);
    fs
}

/// A request of `key` (its own tenant) in the test pool of `cap` slots.
fn req(key: Uuid, cap: usize, session: &str) -> AdmitRequest {
    AdmitRequest::new(key, MODEL, 10)
        .model_cap(cap)
        .session(session)
}

async fn admit(fs: &FairShare, req: AdmitRequest) -> Admitted {
    tokio::time::timeout(Duration::from_secs(5), fs.admit(req))
        .await
        .expect("admitted in time")
        .expect("admitted")
}

fn spawn_admit(fs: &FairShare, req: AdmitRequest) -> JoinHandle<Admitted> {
    let fs = fs.clone();
    tokio::spawn(async move { fs.admit(req).await.expect("admitted") })
}

async fn wait_for_queued(fs: &FairShare, n: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snap = fs.snapshot().await.expect("snapshot");
            if snap.model_queued.get(MODEL).copied().unwrap_or(0) == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {n} queued"));
}

async fn held(fs: &FairShare) -> usize {
    let snap = fs.snapshot().await.expect("snapshot");
    snap.pools
        .iter()
        .find(|p| p.model == MODEL)
        .map_or(0, |p| p.held)
}

/// Run one whole step of `session` and let its slot go.
async fn step(fs: &FairShare, key: Uuid, cap: usize, session: &str) -> Admission {
    let admitted = admit(fs, req(key, cap, session)).await;
    let admission = admitted.admission;
    drop(admitted.permit);
    admission
}

async fn granted_within(handle: &mut JoinHandle<Admitted>, wait: Duration) -> Option<Admitted> {
    match tokio::time::timeout(wait, &mut *handle).await {
        Ok(joined) => Some(joined.expect("admit task")),
        Err(_) => None,
    }
}

/// Fill a one-slot pool, queue a cold request and then a warm session's step
/// from the same key, and free the slot: whichever is granted first.
async fn first_after_release(affinity: Option<SessionAffinityConfig>) -> &'static str {
    let fs = start(affinity);
    let key = Uuid::new_v4();
    step(&fs, key, 1, "warm").await;
    let busy = admit(&fs, req(key, 1, "busy")).await;
    let mut cold = spawn_admit(&fs, req(key, 1, "cold"));
    wait_for_queued(&fs, 1).await;
    let mut warm = spawn_admit(&fs, req(key, 1, "warm"));
    wait_for_queued(&fs, 2).await;
    drop(busy.permit);
    let wait = Duration::from_secs(2);
    tokio::select! {
        _ = &mut cold => "cold",
        _ = &mut warm => "warm",
        _ = tokio::time::sleep(wait) => "neither",
    }
}

#[tokio::test]
async fn off_serves_a_key_in_arrival_order() {
    assert_eq!(first_after_release(None).await, "cold");
}

#[tokio::test]
async fn a_warm_session_goes_ahead_within_its_key() {
    assert_eq!(first_after_release(Some(config(0))).await, "warm");
}

#[tokio::test]
async fn a_looping_session_gets_its_slot_back() {
    let fs = start(Some(config(2_000)));
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&fs, agent, 1, "loop").await;
    // The second step comes straight after the first: a loop.
    let second = admit(&fs, req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&fs, req(other, 1, "other"));
    wait_for_queued(&fs, 1).await;
    drop(second.permit);
    assert!(
        granted_within(&mut waiting, Duration::from_millis(200))
            .await
            .is_none(),
        "the freed slot is held for the loop's next step"
    );
    assert_eq!(held(&fs).await, 1);
    let third = admit(&fs, req(agent, 1, "loop")).await;
    assert_eq!(
        third.admission,
        Admission::Fast,
        "the held slot is the step's"
    );
    assert_eq!(held(&fs).await, 0);
    let stats = fs.stats();
    assert_eq!(stats.affinity.holds.load(Ordering::Relaxed), 1);
    assert_eq!(stats.affinity.redeemed.load(Ordering::Relaxed), 1);
    drop(third.permit);
}

#[tokio::test]
async fn a_hold_nobody_takes_runs_out() {
    let fs = start(Some(config(150)));
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&fs, agent, 1, "loop").await;
    let second = admit(&fs, req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&fs, req(other, 1, "other"));
    wait_for_queued(&fs, 1).await;
    drop(second.permit);
    let granted = granted_within(&mut waiting, Duration::from_secs(2))
        .await
        .expect("granted once the hold runs out");
    assert_eq!(granted.admission, Admission::Queued);
    assert_eq!(held(&fs).await, 0);
    assert_eq!(fs.stats().affinity.expired.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_step_that_comes_late_holds_nothing() {
    let fs = start(Some(config(100)));
    let agent = Uuid::new_v4();
    step(&fs, agent, 1, "slow").await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    step(&fs, agent, 1, "slow").await;
    assert_eq!(held(&fs).await, 0, "a person typing, not a loop");
}

#[tokio::test]
async fn a_held_slot_counts_against_the_key_cap() {
    let fs = start(Some(config(150)));
    let agent = Uuid::new_v4();
    let capped = |session| req(agent, 4, session).key_cap(1);
    step(&fs, agent, 4, "loop").await;
    let second = admit(&fs, capped("loop")).await;
    drop(second.permit);
    assert_eq!(held(&fs).await, 1);
    // The key is at its cap while its slot is held, though the pool is not.
    let mut sibling = spawn_admit(&fs, capped("sibling"));
    assert!(
        granted_within(&mut sibling, Duration::from_millis(60))
            .await
            .is_none(),
        "the key's held slot still counts against its cap"
    );
    granted_within(&mut sibling, Duration::from_secs(2))
        .await
        .expect("granted once the hold runs out");
}

#[tokio::test]
async fn holds_are_capped_per_pool() {
    let mut cfg = config(2_000);
    cfg.max_held_percent = 25;
    let fs = start(Some(cfg));
    let agents: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    for a in &agents {
        step(&fs, *a, 4, "loop").await;
    }
    let mut seconds = Vec::new();
    for a in &agents {
        seconds.push(admit(&fs, req(*a, 4, "loop")).await);
    }
    drop(seconds);
    assert_eq!(held(&fs).await, 1, "25% of four slots is one");
}

#[tokio::test]
async fn holds_give_way_to_a_request_that_waited_too_long() {
    let mut cfg = config(1_000);
    cfg.max_bypass = Duration::from_millis(300);
    cfg.hold = Duration::from_millis(300);
    let fs = start(Some(cfg));
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&fs, agent, 1, "loop").await;
    let second = admit(&fs, req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&fs, req(other, 1, "other"));
    wait_for_queued(&fs, 1).await;
    // The waiter is already most of the way to aging when the hold starts.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(second.permit);
    let granted = granted_within(&mut waiting, Duration::from_millis(250))
        .await
        .expect("the hold gives way once the waiter has aged");
    assert_eq!(granted.admission, Admission::Queued);
}

#[tokio::test]
async fn turning_affinity_off_releases_held_slots() {
    let fs = start(Some(config(30_000)));
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&fs, agent, 1, "loop").await;
    let second = admit(&fs, req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&fs, req(other, 1, "other"));
    wait_for_queued(&fs, 1).await;
    drop(second.permit);
    assert_eq!(held(&fs).await, 1);
    fs.set_session_affinity(None);
    granted_within(&mut waiting, Duration::from_secs(2))
        .await
        .expect("granted once affinity is off");
}

#[tokio::test]
async fn requests_without_a_session_are_unaffected() {
    let fs = start(Some(config(2_000)));
    let key = Uuid::new_v4();
    for _ in 0..3 {
        let a = admit(&fs, AdmitRequest::new(key, MODEL, 10).model_cap(1)).await;
        assert_eq!(a.admission, Admission::Fast);
        drop(a.permit);
    }
    assert_eq!(held(&fs).await, 0);
}
