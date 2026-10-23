//! Session affinity: a warm session's next step goes first within its key,
//! and a looping session's slot is held for its next step.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use obleth_config::{Admission, FairshareAlgorithm, SessionAffinityConfig};
use obleth_fairshare::{
    AdmitRequest, Admitted, FairShare, InMemoryCluster, PoolKey, SharedSlotsConfig, SlotMode,
    StaticCapacity,
};
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
async fn a_cold_request_is_passed_at_most_max_jumps_times() {
    let mut cfg = config(0);
    cfg.max_jumps = 2;
    let fs = start(Some(cfg));
    let key = Uuid::new_v4();
    for s in ["w1", "w2", "w3"] {
        step(&fs, key, 1, s).await;
    }
    let mut running = admit(&fs, req(key, 1, "busy")).await;
    let mut cold = spawn_admit(&fs, req(key, 1, "cold"));
    wait_for_queued(&fs, 1).await;
    // Two warm sessions go ahead of it, one after the other.
    for s in ["w1", "w2"] {
        let mut warm = spawn_admit(&fs, req(key, 1, s));
        wait_for_queued(&fs, 2).await;
        drop(running.permit);
        running = granted_within(&mut warm, Duration::from_secs(2))
            .await
            .expect("the warm session goes first");
        assert!(!cold.is_finished());
    }
    // Passed twice: a third warm session waits behind it.
    let mut warm = spawn_admit(&fs, req(key, 1, "w3"));
    wait_for_queued(&fs, 2).await;
    drop(running.permit);
    let granted = granted_within(&mut cold, Duration::from_secs(2))
        .await
        .expect("its turn after being passed max_jumps times");
    assert!(!warm.is_finished());
    drop(granted.permit);
    granted_within(&mut warm, Duration::from_secs(2))
        .await
        .expect("then the warm one");
}

#[tokio::test]
async fn a_key_stops_holding_once_a_request_of_it_may_be_passed_no_more() {
    let mut cfg = config(5_000);
    cfg.max_jumps = 1;
    let fs = start(Some(cfg));
    let key = Uuid::new_v4();
    step(&fs, key, 1, "loop").await;
    let second = admit(&fs, req(key, 1, "loop")).await;
    let mut cold = spawn_admit(&fs, req(key, 1, "cold"));
    wait_for_queued(&fs, 1).await;
    drop(second.permit);
    assert_eq!(held(&fs).await, 1, "nobody passed yet: held");
    // The loop's next step takes the held slot, passing the cold request.
    let third = admit(&fs, req(key, 1, "loop")).await;
    assert_eq!(third.admission, Admission::Fast);
    drop(third.permit);
    let granted = granted_within(&mut cold, Duration::from_secs(1))
        .await
        .expect("passed once: the slot is its, not held");
    assert_eq!(held(&fs).await, 0);
    drop(granted.permit);
}

#[tokio::test]
async fn a_waiter_held_back_by_its_own_key_cap_does_not_end_holds() {
    let mut cfg = config(2_000);
    cfg.max_bypass = Duration::from_millis(100);
    let fs = start(Some(cfg));
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    let capped = |session| req(other, 2, session).key_cap(1);
    step(&fs, agent, 2, "loop").await;
    let second = admit(&fs, req(agent, 2, "loop")).await;
    let busy = admit(&fs, capped("busy")).await;
    let _backlog = spawn_admit(&fs, capped("backlog"));
    wait_for_queued(&fs, 1).await;
    // Long past max_bypass, but a free slot would not be the backlog's.
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(second.permit);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(held(&fs).await, 1);
    drop(busy.permit);
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

// ---- per-model settings ------------------------------------------------------

/// A request of `key` in the test pool, carrying the model's own setting.
fn model_req(
    key: Uuid,
    cap: usize,
    session: &str,
    own: Option<SessionAffinityConfig>,
) -> AdmitRequest {
    req(key, cap, session).session_affinity(own)
}

#[tokio::test]
async fn a_model_turns_affinity_on_for_its_pool_alone() {
    let fs = start(None);
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    let own = Some(config(2_000));
    let step_own = |key, session| {
        let fs = fs.clone();
        async move {
            let a = admit(&fs, model_req(key, 1, session, own)).await;
            drop(a.permit);
        }
    };
    step_own(agent, "loop").await;
    let second = admit(&fs, model_req(agent, 1, "loop", own)).await;
    let mut waiting = spawn_admit(&fs, model_req(other, 1, "other", own));
    wait_for_queued(&fs, 1).await;
    drop(second.permit);
    assert_eq!(
        held(&fs).await,
        1,
        "held although the scheduler's switch is off"
    );
    assert!(!waiting.is_finished());
    let third = admit(&fs, model_req(agent, 1, "loop", own)).await;
    assert_eq!(third.admission, Admission::Fast);
    drop(third.permit);
    granted_within(&mut waiting, Duration::from_secs(3))
        .await
        .expect("granted after the hold");

    // Another model with no setting of its own follows the scheduler: off.
    let plain = |session| {
        AdmitRequest::new(agent, "plain", 10)
            .model_cap(1)
            .session(session)
    };
    for _ in 0..2 {
        let a = admit(&fs, plain("loop")).await;
        drop(a.permit);
    }
    let snap = fs.snapshot().await.expect("snapshot");
    let plain_held = snap
        .pools
        .iter()
        .find(|p| p.model == "plain")
        .map_or(0, |p| p.held);
    assert_eq!(plain_held, 0);
}

#[tokio::test]
async fn a_model_turns_affinity_off_for_its_pool() {
    let fs = start(Some(config(2_000)));
    let agent = Uuid::new_v4();
    for _ in 0..3 {
        let a = admit(&fs, model_req(agent, 1, "loop", None)).await;
        drop(a.permit);
    }
    assert_eq!(
        held(&fs).await,
        0,
        "off for this model though on for the scheduler"
    );
}

#[tokio::test]
async fn turning_a_model_off_hands_back_what_its_pool_held() {
    let fs = start(None);
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    let own = Some(config(30_000));
    let a = admit(&fs, model_req(agent, 1, "loop", own)).await;
    drop(a.permit);
    let second = admit(&fs, model_req(agent, 1, "loop", own)).await;
    drop(second.permit);
    assert_eq!(held(&fs).await, 1);
    // The next request brings the model's new setting: off.
    let off = admit(&fs, model_req(other, 1, "other", None)).await;
    assert_eq!(held(&fs).await, 0);
    drop(off.permit);
}

#[tokio::test]
async fn the_scheduler_switch_leaves_a_model_with_its_own_setting_alone() {
    let fs = start(None);
    let agent = Uuid::new_v4();
    let own = Some(config(30_000));
    let a = admit(&fs, model_req(agent, 1, "loop", own)).await;
    drop(a.permit);
    let second = admit(&fs, model_req(agent, 1, "loop", own)).await;
    drop(second.permit);
    assert_eq!(held(&fs).await, 1);
    fs.set_session_affinity(Some(config(1_000)));
    fs.set_session_affinity(None);
    assert_eq!(held(&fs).await, 1, "still the model's");
}

// ---- shared slots: holds kept in the backend --------------------------------

/// `n` gateways sharing `cluster` in shared mode, with affinity on.
async fn shared_gateways(
    cluster: &InMemoryCluster,
    n: usize,
    affinity: SessionAffinityConfig,
) -> Vec<FairShare> {
    let mut gws = Vec::new();
    for i in 0..n {
        let fs = start(Some(affinity));
        fs.enable_shared_slots(
            Arc::new(cluster.gateway(&format!("gw-{i}"))),
            SharedSlotsConfig {
                reconcile_interval: Duration::from_secs(60),
                ..SharedSlotsConfig::default()
            },
        );
        fs.set_replicas(n);
        cluster.subscribe(&fs);
        gws.push(fs);
    }
    for fs in &gws {
        let stats = fs.stats();
        tokio::time::timeout(Duration::from_secs(5), async {
            while stats.mode() != SlotMode::Shared {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("shared mode");
    }
    gws
}

fn pool_id() -> String {
    PoolKey::Model(MODEL.into()).slot_id()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_slot_follows_the_session_to_another_gateway() {
    let cluster = InMemoryCluster::new();
    let gws = shared_gateways(&cluster, 2, config(2_000)).await;
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&gws[0], agent, 1, "loop").await;
    let second = admit(&gws[0], req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&gws[1], req(other, 1, "other"));
    wait_for_queued(&gws[1], 1).await;
    drop(second.permit);

    // Held in the backend: it still fills the pool, and nobody else gets it.
    tokio::time::timeout(Duration::from_secs(2), async {
        while cluster.held_slots(&pool_id()) != 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("held in the backend");
    assert_eq!(cluster.pool_in_flight(&pool_id()), 1);
    assert_eq!(cluster.held("gw-0", &pool_id()), 0);
    assert!(
        granted_within(&mut waiting, Duration::from_millis(300))
            .await
            .is_none(),
        "the held slot is not anyone else's"
    );

    // The session's next step reaches the other gateway and takes it there.
    let third = admit(&gws[1], req(agent, 1, "loop")).await;
    assert_eq!(cluster.held_slots(&pool_id()), 0);
    assert_eq!(cluster.held("gw-1", &pool_id()), 1);
    assert_eq!(gws[1].stats().affinity.redeemed.load(Ordering::Relaxed), 1);
    assert!(!waiting.is_finished(), "the other request still waits");
    drop(third.permit);
    granted_within(&mut waiting, Duration::from_secs(5))
        .await
        .expect("granted once the session is done with the slot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_hold_nobody_takes_is_freed() {
    let cluster = InMemoryCluster::new();
    let gws = shared_gateways(&cluster, 2, config(150)).await;
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&gws[0], agent, 1, "loop").await;
    let second = admit(&gws[0], req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&gws[1], req(other, 1, "other"));
    wait_for_queued(&gws[1], 1).await;
    drop(second.permit);
    let granted = granted_within(&mut waiting, Duration::from_secs(5))
        .await
        .expect("granted once the hold runs out");
    assert_eq!(granted.admission, Admission::Queued);
    assert_eq!(cluster.held_slots(&pool_id()), 0);
    assert_eq!(cluster.pool_in_flight(&pool_id()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_holds_are_capped_per_pool() {
    let cluster = InMemoryCluster::new();
    let mut cfg = config(2_000);
    cfg.max_held_percent = 25;
    let gws = shared_gateways(&cluster, 2, cfg).await;
    let agents: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    for a in &agents {
        step(&gws[0], *a, 4, "loop").await;
    }
    let mut seconds = Vec::new();
    for a in &agents {
        seconds.push(admit(&gws[0], req(*a, 4, "loop")).await);
    }
    drop(seconds);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        cluster.held_slots(&pool_id()),
        1,
        "25% of four slots is one"
    );
    assert_eq!(cluster.pool_in_flight(&pool_id()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loop_whose_steps_alternate_gateways_is_held_for() {
    let cluster = InMemoryCluster::new();
    let gws = shared_gateways(&cluster, 2, config(2_000)).await;
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    // Each step lands on the other gateway, so neither sees two in a row.
    step(&gws[0], agent, 1, "loop").await;
    let second = admit(&gws[1], req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&gws[0], req(other, 1, "other"));
    wait_for_queued(&gws[0], 1).await;
    drop(second.permit);
    tokio::time::timeout(Duration::from_secs(2), async {
        while cluster.held_slots(&pool_id()) != 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("held: the backend saw both steps");
    let third = admit(&gws[0], req(agent, 1, "loop")).await;
    assert_eq!(cluster.held("gw-0", &pool_id()), 1);
    assert!(!waiting.is_finished());
    drop(third.permit);
    granted_within(&mut waiting, Duration::from_secs(5))
        .await
        .expect("granted once the hold after the third step runs out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_step_on_a_shared_pool_holds_nothing() {
    let cluster = InMemoryCluster::new();
    let gws = shared_gateways(&cluster, 2, config(2_000)).await;
    step(&gws[0], Uuid::new_v4(), 1, "once").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(cluster.held_slots(&pool_id()), 0);
    assert_eq!(cluster.pool_in_flight(&pool_id()), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_looping_session_gives_way_to_another_gateway_after_max_jumps() {
    let cluster = InMemoryCluster::new();
    let mut cfg = config(5_000);
    cfg.max_jumps = 2;
    let gws = shared_gateways(&cluster, 2, cfg).await;
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&gws[0], agent, 1, "loop").await;
    let mut running = admit(&gws[0], req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&gws[1], req(other, 1, "other"));
    wait_for_queued(&gws[1], 1).await;
    // Two holds in a row while gw-1 waits, each taken by the next step.
    for _ in 0..2 {
        drop(running.permit);
        running = admit(&gws[0], req(agent, 1, "loop")).await;
        assert!(!waiting.is_finished(), "held for the loop");
    }
    drop(running.permit);
    let granted = granted_within(&mut waiting, Duration::from_secs(1))
        .await
        .expect("the waiter on the other gateway gets the slot");
    assert_eq!(granted.admission, Admission::Queued);
    assert_eq!(cluster.held_slots(&pool_id()), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_waited_too_long_on_another_gateway_frees_held_slots() {
    let cluster = InMemoryCluster::new();
    let mut cfg = config(10_000);
    cfg.max_jumps = 100;
    cfg.max_bypass = Duration::from_millis(400);
    let gws = shared_gateways(&cluster, 2, cfg).await;
    let (agent, other) = (Uuid::new_v4(), Uuid::new_v4());
    step(&gws[0], agent, 1, "loop").await;
    let second = admit(&gws[0], req(agent, 1, "loop")).await;
    let mut waiting = spawn_admit(&gws[1], req(other, 1, "other"));
    wait_for_queued(&gws[1], 1).await;
    drop(second.permit);
    tokio::time::timeout(Duration::from_secs(2), async {
        while cluster.held_slots(&pool_id()) != 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("held in the backend");
    let granted = granted_within(&mut waiting, Duration::from_secs(3))
        .await
        .expect("granted once it has waited max_bypass, long before the hold runs out");
    assert!(granted.waited < Duration::from_secs(2));
    assert_eq!(cluster.held_slots(&pool_id()), 0);
    drop(granted.permit);
    let third = admit(&gws[0], req(agent, 1, "loop")).await;
    drop(third.permit);
}
