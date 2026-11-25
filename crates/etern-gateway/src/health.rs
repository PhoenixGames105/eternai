//! Engine health state machine.
//!
//! The two failure modes a client must distinguish are completely different:
//!
//! * the **gateway** is unreachable — the phone is off the LAN, so it must run
//!   on its own model and queue the transcript;
//! * the **engine** is not ready — the gateway is fine, but the desktop is
//!   mid-swap (a ~20 minute model load on this host) and the conversation
//!   should simply wait.
//!
//! Collapsing those two into "connection failed" is what makes a naive client
//! fall back to the edge model and fork the transcript during a routine model
//! swap. So the gateway publishes both signals separately, and an engine that
//! was ready moments ago and has since stopped answering is reported as
//! `loading`, not `down`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

/// What the engine behind the gateway can do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    /// Answers `/health` and has a free slot.
    Ready,
    /// Up, but every slot is occupied. Requests will queue, not fail.
    Busy,
    /// Reachable (or recently so) but not serving yet — a model is loading.
    Loading,
    /// Not reachable, and it has been too long for this to be a swap.
    Down,
}

impl EngineState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Loading => "loading",
            Self::Down => "down",
        }
    }

    /// True when a prompt sent now would eventually be served by the desktop.
    pub fn is_serving(self) -> bool {
        !matches!(self, Self::Down)
    }
}

/// Raw observation from one probe of the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// HTTP 200 from `/health`.
    Healthy,
    /// HTTP 503. llama.cpp uses this both for "still loading" and for "all
    /// slots busy", which is why the body is inspected.
    Unavailable { busy: bool },
    /// Answered HTTP, but not in a shape we recognise.
    Unexpected,
    /// Connection refused / timed out / no listener.
    Unreachable,
}

impl Probe {
    /// Maps a llama.cpp-compatible `/health` response.
    pub fn from_status(status: u16, body: &str) -> Self {
        match status {
            200 => Self::Healthy,
            503 => Self::Unavailable {
                busy: body.to_ascii_lowercase().contains("busy"),
            },
            _ => Self::Unexpected,
        }
    }
}

/// Pure transition function, kept separate from I/O so every edge is testable.
///
/// `last_ready` is when the engine was last observed healthy, or `None` if it
/// has never been observed healthy since the gateway started.
///
/// `gateway_started` anchors the grace window when the engine has never been
/// seen healthy. Without it, a desktop reboot where the gateway comes up before
/// llama.cpp has finished its ~20-minute load would report `down` immediately,
/// and the phone would fail over and fork the transcript during an ordinary
/// boot. A wrong engine URL still becomes `down` once the grace expires.
pub fn classify(
    probe: &Probe,
    last_ready: Option<Instant>,
    gateway_started: Instant,
    now: Instant,
    in_flight: usize,
    capacity: u64,
    load_grace: Duration,
) -> EngineState {
    match probe {
        Probe::Healthy => {
            if (in_flight as u64) >= capacity.max(1) {
                EngineState::Busy
            } else {
                EngineState::Ready
            }
        }
        Probe::Unavailable { busy: true } => EngineState::Busy,
        Probe::Unavailable { busy: false } | Probe::Unexpected => {
            // A live listener that answers 503 is up; it just has no model yet.
            EngineState::Loading
        }
        Probe::Unreachable => {
            let anchor = last_ready.unwrap_or(gateway_started);
            // It was serving recently, then the port vanished: that is exactly
            // what a model swap looks like from out here.
            if now.duration_since(anchor) <= load_grace {
                EngineState::Loading
            } else {
                EngineState::Down
            }
        }
    }
}

/// Shared health snapshot. The proxy updates `in_flight`; the prober replaces
/// `state`. Readers never block a probe.
#[derive(Debug)]
pub struct Health {
    state: std::sync::RwLock<EngineState>,
    in_flight: AtomicU64,
    capacity: AtomicU64,
    last_ready: std::sync::Mutex<Option<Instant>>,
    /// Grace anchor when the engine has never been observed healthy.
    started: Instant,
    load_grace: Duration,
    /// Monotonic counter of observed `ready -> not ready` edges, so a client
    /// can tell a fresh swap from a long-running outage.
    swaps: AtomicU64,
}

impl Health {
    pub fn new(load_grace: Duration, capacity: u64) -> Self {
        Self {
            state: std::sync::RwLock::new(EngineState::Loading),
            in_flight: AtomicU64::new(0),
            capacity: AtomicU64::new(capacity.max(1)),
            last_ready: std::sync::Mutex::new(None),
            started: Instant::now(),
            load_grace,
            swaps: AtomicU64::new(0),
        }
    }

    /// Test-only: a gateway that started `ago` in the past, so a test can drive
    /// the never-ready path past the grace window and reach `Down`.
    #[cfg(test)]
    pub fn started_ago(ago: Duration, load_grace: Duration, capacity: u64) -> Self {
        Self {
            started: Instant::now() - ago,
            ..Self::new(load_grace, capacity)
        }
    }

    /// Updated from `/props.total_slots` once the engine has been discovered.
    /// This host reports one slot, which is why the default is one.
    pub fn set_capacity(&self, capacity: u64) {
        self.capacity.store(capacity.max(1), Ordering::Relaxed);
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> EngineState {
        *self.state.read().expect("health lock")
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed) as usize
    }

    pub fn swaps(&self) -> u64 {
        self.swaps.load(Ordering::Relaxed)
    }

    /// Records an in-flight proxied request. Dropped when the request finishes.
    pub fn begin_request(self: &std::sync::Arc<Self>) -> Guard {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        Guard { health: self.clone() }
    }

    /// Applies a probe result and returns the state the engine is now in.
    pub fn record(&self, probe: Probe) -> EngineState {
        let now = Instant::now();
        let in_flight = self.in_flight();
        let capacity = self.capacity();
        let next = {
            let mut last_ready = self.last_ready.lock().expect("health lock");
            let state = classify(
                &probe,
                *last_ready,
                self.started,
                now,
                in_flight,
                capacity,
                self.load_grace,
            );
            match state {
                EngineState::Ready | EngineState::Busy => *last_ready = Some(now),
                EngineState::Loading | EngineState::Down => {}
            }
            state
        };

        let previous = self.state();
        if previous == EngineState::Ready && next != EngineState::Ready {
            self.swaps.fetch_add(1, Ordering::Relaxed);
        }
        *self.state.write().expect("health lock") = next;
        next
    }

    /// Snapshot for `GET /health`, which is deliberately unauthenticated so a
    /// phone can use it as its reachability signal without holding a token.
    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            gateway_reachable: true,
            engine_ready: self.state().as_str(),
            engine_ready_s: self.state().is_serving(),
            in_flight: self.in_flight(),
            swaps: self.swaps(),
        }
    }
}

/// Decrements the in-flight counter when the proxied request completes.
#[derive(Debug)]
pub struct Guard {
    health: std::sync::Arc<Health>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.health.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    /// Always true if this body arrived at all — the field exists so the client
    /// can write one uniform check instead of inferring from transport errors.
    pub gateway_reachable: bool,
    pub engine_ready: &'static str,
    pub engine_ready_s: bool,
    pub in_flight: usize,
    pub swaps: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Probe {
        Probe::Healthy
    }

    /// A gateway that started long enough ago to be irrelevant to the test.
    fn long_ago(now: Instant) -> Instant {
        now - Duration::from_secs(10_000)
    }

    #[test]
    fn healthy_with_a_free_slot_is_ready() {
        let now = Instant::now();
        assert_eq!(
            classify(&base(), None, long_ago(now), now, 0, 1, Duration::from_secs(10)),
            EngineState::Ready
        );
    }

    #[test]
    fn healthy_with_no_free_slot_is_busy_not_down() {
        let now = Instant::now();
        assert_eq!(
            classify(&base(), None, long_ago(now), now, 1, 1, Duration::from_secs(10)),
            EngineState::Busy
        );
    }

    #[test]
    fn zero_capacity_never_divides_down() {
        let now = Instant::now();
        assert_eq!(
            classify(&base(), None, long_ago(now), now, 0, 0, Duration::from_secs(10)),
            EngineState::Ready
        );
    }

    #[test]
    fn a_port_that_vanished_after_being_ready_is_loading() {
        // The regression this whole module exists for: a model swap must not
        // look like an outage, or the phone forks the conversation.
        let now = Instant::now();
        let just_ready = now - Duration::from_secs(5);
        assert_eq!(
            classify(
                &Probe::Unreachable,
                Some(just_ready),
                long_ago(now),
                now,
                0,
                1,
                Duration::from_secs(1_500)
            ),
            EngineState::Loading
        );
    }

    #[test]
    fn a_long_outage_is_down() {
        let now = Instant::now();
        let stale = now - Duration::from_secs(1_501);
        assert_eq!(
            classify(
                &Probe::Unreachable,
                Some(stale),
                long_ago(now),
                now,
                0,
                1,
                Duration::from_secs(1_500)
            ),
            EngineState::Down
        );
    }

    #[test]
    fn a_cold_boot_where_the_engine_is_still_loading_is_not_down() {
        // The desktop rebooted: the gateway is up, llama.cpp needs ~20 minutes
        // to load, and the phone must wait rather than fork the transcript.
        let now = Instant::now();
        let started = now - Duration::from_secs(60);
        assert_eq!(
            classify(&Probe::Unreachable, None, started, now, 0, 1, Duration::from_secs(1_500)),
            EngineState::Loading
        );
    }

    #[test]
    fn an_engine_that_never_appears_eventually_is_down() {
        // A wrong engine URL must not hang the phone in `loading` forever.
        let now = Instant::now();
        let started = now - Duration::from_secs(1_501);
        assert_eq!(
            classify(&Probe::Unreachable, None, started, now, 0, 1, Duration::from_secs(1_500)),
            EngineState::Down
        );
    }

    #[test]
    fn a_listener_answering_503_is_loading() {
        let now = Instant::now();
        assert_eq!(
            classify(
                &Probe::Unavailable { busy: false },
                None,
                long_ago(now),
                now,
                0,
                1,
                Duration::from_secs(10)
            ),
            EngineState::Loading
        );
        assert_eq!(
            classify(&Probe::Unexpected, None, long_ago(now), now, 0, 1, Duration::from_secs(10)),
            EngineState::Loading
        );
    }

    #[test]
    fn busy_is_inferred_from_the_503_body() {
        assert_eq!(
            Probe::from_status(503, r#"{"error":"all slots are busy"}"#),
            Probe::Unavailable { busy: true }
        );
        assert_eq!(
            Probe::from_status(503, "model loading"),
            Probe::Unavailable { busy: false }
        );
        assert_eq!(Probe::from_status(200, ""), Probe::Healthy);
        assert_eq!(Probe::from_status(404, ""), Probe::Unexpected);
    }

    #[test]
    fn down_is_not_serving_but_everything_else_is() {
        assert!(!EngineState::Down.is_serving());
        assert!(EngineState::Loading.is_serving());
        assert!(EngineState::Busy.is_serving());
        assert!(EngineState::Ready.is_serving());
    }

    #[test]
    fn in_flight_guard_moves_the_count_and_counts_swaps() {
        let health = std::sync::Arc::new(Health::new(Duration::from_secs(1_500), 1));
        assert_eq!(health.state(), EngineState::Loading);

        let guard = health.begin_request();
        assert_eq!(health.in_flight(), 1);

        // A full slot still counts as reachable, so it is not a swap.
        health.record(Probe::Healthy);
        assert_eq!(health.state(), EngineState::Busy);
        assert_eq!(health.swaps(), 0);

        drop(guard);
        health.record(Probe::Healthy);
        assert_eq!(health.state(), EngineState::Ready);
        assert_eq!(health.in_flight(), 0);

        health.record(Probe::Unreachable);
        assert_eq!(health.state(), EngineState::Loading, "recently ready => loading");
        assert_eq!(health.swaps(), 1, "the drop-out is a swap");

        let snapshot = health.snapshot();
        assert!(snapshot.gateway_reachable);
        assert_eq!(snapshot.engine_ready, "loading");
        assert!(snapshot.engine_ready_s, "loading must not look like an outage");
        assert_eq!(snapshot.swaps, 1);
    }
}
