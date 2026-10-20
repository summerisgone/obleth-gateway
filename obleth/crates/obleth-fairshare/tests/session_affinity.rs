//! Session affinity: a warm session's next step goes first within its key,
//! and a slot its finished step frees is held for it briefly.

use std::sync::Arc;
use std::time::Duration;

use obleth_config::{Admission, FairshareAlgorithm, SessionAffinityConfig};
use obleth_fairshare::{AdmitRequest, Admitted, FairShare, StaticCapacity};
use tokio::task::JoinHandle;
use uuid::Uuid;

const MODEL: &str = "m";

fn affinity(hold_ms: u64, max_jumps: u32, max_bypass_ms: u64) -> SessionAffinityConfig {
    SessionAffinityConfig {
        warm_ttl: Duration::from_secs(120),
        hold: Duration::from_millis(hold_ms),
        max_jumps,
        max_bypass: Duration::from_millis(max_bypass_ms),
    }
}

/// A one-slot pool, so every grant after the first waits for a release.
fn start(config: Option<SessionAffinityConfig>) -> FairShare {
    let fs = FairShare::start(
        Arc::new(StaticCapacity::new(16)),
        FairshareAlgorithm::Weighted,
        1,
    );
    fs.set_session_affinity(config);
    fs
}

fn step(tenant: Uuid, key: Uuid, session: &str) -> AdmitRequest {
    AdmitRequest::new(tenant, MODEL, 1)
        .key(key, 100)
        .model_cap(1)
        .session(session)
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

/// Queue `req` behind whatever is queued now.
async fn queue(fs: &FairShare, req: AdmitRequest) -> JoinHandle<Option<Admitted>> {
    let before = fs
        .snapshot()
        .await
        .expect("snapshot")
        .model_queued
        .get(MODEL)
        .copied()
        .unwrap_or(0);
    let f = fs.clone();
    let handle = tokio::spawn(async move { f.admit(req).await });
    wait_for_queued(fs, before + 1).await;
    handle
}

async fn granted_within(handle: &mut JoinHandle<Option<Admitted>>, ms: u64) -> Option<Admitted> {
    match tokio::time::timeout(Duration::from_millis(ms), handle).await {
        Ok(joined) => Some(joined.expect("join").expect("admitted")),
        Err(_) => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_keeps_arrival_order() {
    let fs = start(None);
    let (t, k) = (Uuid::new_v4(), Uuid::new_v4());
    let first = fs.admit(step(t, k, "a")).await.expect("admit");
    let mut cold = queue(&fs, step(t, k, "b")).await;
    let mut next = queue(&fs, step(t, k, "a")).await;
    drop(first.permit);
    let cold = granted_within(&mut cold, 1000).await.expect("first come");
    assert!(granted_within(&mut next, 50).await.is_none());
    drop(cold.permit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_session_goes_ahead_within_its_key() {
    let fs = start(Some(affinity(0, 8, 30_000)));
    let (t, k) = (Uuid::new_v4(), Uuid::new_v4());
    let first = fs.admit(step(t, k, "a")).await.expect("admit");
    let mut cold = queue(&fs, step(t, k, "b")).await;
    let mut next = queue(&fs, step(t, k, "a")).await;
    drop(first.permit);
    let got = granted_within(&mut next, 1000)
        .await
        .expect("warm step first");
    assert_eq!(got.admission, Admission::Queued);
    assert!(granted_within(&mut cold, 50).await.is_none());
    drop(got.permit);
    assert!(granted_within(&mut cold, 1000).await.is_some());
}

/// Run one step of `session` on an idle pool and return the permit of the
/// next one, which arrives right after it: a chained step, whose finish
/// holds the slot.
async fn chained(fs: &FairShare, t: Uuid, k: Uuid, session: &str) -> Admitted {
    let first = fs.admit(step(t, k, session)).await.expect("admit");
    drop(first.permit);
    fs.admit(step(t, k, session)).await.expect("admit")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_step_holds_nothing() {
    let fs = start(Some(affinity(5_000, 8, 30_000)));
    let t = Uuid::new_v4();
    let first = fs.admit(step(t, Uuid::new_v4(), "a")).await.expect("admit");
    let mut other = queue(&fs, step(t, Uuid::new_v4(), "b")).await;
    drop(first.permit);
    assert!(granted_within(&mut other, 1000).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freed_slot_is_held_for_the_next_step() {
    let fs = start(Some(affinity(5_000, 8, 30_000)));
    let (t, k) = (Uuid::new_v4(), Uuid::new_v4());
    let second = chained(&fs, t, k, "a").await;
    // Another key's waiter: the hold reaches across keys.
    let mut other = queue(&fs, step(t, Uuid::new_v4(), "b")).await;
    drop(second.permit);
    assert!(granted_within(&mut other, 200).await.is_none());
    let next = tokio::time::timeout(Duration::from_secs(1), fs.admit(step(t, k, "a")))
        .await
        .expect("held slot taken at once")
        .expect("admit");
    assert!(granted_within(&mut other, 50).await.is_none());
    drop(next.permit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_key_cannot_take_a_hold_with_the_same_session_id() {
    let fs = start(Some(affinity(500, 8, 30_000)));
    let t = Uuid::new_v4();
    let second = chained(&fs, t, Uuid::new_v4(), "a").await;
    let mut other = queue(&fs, step(t, Uuid::new_v4(), "a")).await;
    drop(second.permit);
    assert!(granted_within(&mut other, 200).await.is_none());
    assert!(granted_within(&mut other, 2000).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_runs_out_and_the_slot_moves_on() {
    let fs = start(Some(affinity(300, 8, 30_000)));
    let (t, k) = (Uuid::new_v4(), Uuid::new_v4());
    let second = chained(&fs, t, k, "a").await;
    let mut other = queue(&fs, step(t, k, "b")).await;
    drop(second.permit);
    assert!(granted_within(&mut other, 100).await.is_none());
    assert!(granted_within(&mut other, 2000).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aged_waiter_is_not_kept_out_by_a_hold() {
    let fs = start(Some(affinity(10_000, 8, 300)));
    let (t, k) = (Uuid::new_v4(), Uuid::new_v4());
    let second = chained(&fs, t, k, "a").await;
    let mut other = queue(&fs, step(t, k, "b")).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    drop(second.permit);
    assert!(granted_within(&mut other, 200).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_out_of_jumps_queues_in_order() {
    let fs = start(Some(affinity(0, 1, 30_000)));
    let (t, k) = (Uuid::new_v4(), Uuid::new_v4());
    let first = fs.admit(step(t, k, "a")).await.expect("admit");
    let mut cold = queue(&fs, step(t, k, "b")).await;
    let mut second = queue(&fs, step(t, k, "a")).await;
    drop(first.permit);
    // One jump allowed: the warm step goes ahead of the older cold one.
    let second = granted_within(&mut second, 1000).await.expect("jumped");
    let mut third = queue(&fs, step(t, k, "a")).await;
    drop(second.permit);
    // Out of jumps: the cold step goes first now.
    let cold = granted_within(&mut cold, 1000).await.expect("cold served");
    assert!(granted_within(&mut third, 50).await.is_none());
    drop(cold.permit);
    assert!(granted_within(&mut third, 1000).await.is_some());
}

#[test]
fn empty_session_id_is_no_session() {
    assert_eq!(
        AdmitRequest::new(Uuid::nil(), MODEL, 1).session("").session,
        None
    );
    assert_eq!(
        AdmitRequest::new(Uuid::nil(), MODEL, 1)
            .session("s")
            .session
            .as_deref(),
        Some("s")
    );
}
