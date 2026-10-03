#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 4 — `uring_proxy`: uring-kit 0.1.0 + breaker 2.0.1 +
//! throttle-kit 1.1.1.
//!
//! The proxy substrate under admission control: a real echo proxy runs
//! on an `io_uring` engine thread — listener registration, completion-
//! driven accept (the engine re-arms and materializes peers itself),
//! `ReadFixed` into a registered `BufferPool` slot, admission through
//! the estate's edge gates, `WriteFixed` echo, and `Close` — while the
//! handler decision is gated **breaker-first, then throttled**:
//!
//! 1. an open circuit answers `CIRCUIT_OPEN` before any capacity is
//!    spent (the pause — the echo handler is never invoked),
//! 2. an exhausted GCRA budget answers `THROTTLED retry_after_ms=<n>`,
//! 3. a scripted backend failure (`fail…` payload) records a breaker
//!    failure and closes the connection without a response — two in a
//!    row trip the circuit,
//! 4. everything else is echoed and recorded as a breaker success,
//!    which is what closes the circuit again from half-open.
//!
//! The admission gates are driven through their documented sync seams —
//! breaker's manual entry points (`state`/`record_success`/
//! `record_failure`, the machinery `call()` wraps) and throttle-kit's
//! `check_sync` — because the engine thread is a completion loop, not
//! an async task. That seam choice is itself part of the proof: the
//! gates compose with a non-async substrate.
//!
//! Hermeticity note: `io_uring` is a kernel surface. Where the running
//! kernel or its seccomp policy denies ring creation, the flagship test
//! reports the skip and returns — the pure-surface test still asserts
//! the token/probe/pool contracts. On stock Linux runners (including
//! GitHub's ubuntu images) the full proxy path executes.
//!
//! Full L7 proxying (request parsing, upstream pools, splice passthrough)
//! is documented future integration; this suite pins the substrate loop.

use breaker::{BackoffStrategy, CircuitBreaker, CircuitBreakerConfig, State};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use throttle_kit::{InMemoryBackend, Quota, RateLimiter};
use uring_kit::engine::Engine as _;
use uring_kit::{net, BufferPool, Op, Token, UringEngine};

/// The fixed registered-buffer slot the single-connection proxy uses.
const SLOT: u32 = 0;
/// Payload terminator — a complete request is one `\n`-terminated line.
const REQUEST_END: u8 = b'\n';

/// The admission gate shared between the engine thread (decider) and
/// the test thread (assertions).
struct Gate {
    breaker: CircuitBreaker,
    throttle: RateLimiter<InMemoryBackend>,
    /// The exact decision sequence, appended by the handler.
    decisions: Mutex<Vec<&'static str>>,
}

enum Decision {
    /// Write these bytes back, then close.
    Respond(String),
    /// Close without responding (scripted backend failure).
    FailBackend,
}

impl Gate {
    /// Breaker trips on two consecutive backend failures with a fixed
    /// 150 ms cooldown; the throttle admits a burst of 4 with a 250 ms
    /// GCRA emission interval (per_second(4) ⇒ 250 ms/token).
    fn new() -> Self {
        let breaker = CircuitBreaker::new(
            CircuitBreakerConfig::builder()
                .consecutive_failures(2)
                .failure_rate_threshold(1.0)
                .sliding_window_size(10)
                .backoff(BackoffStrategy::Fixed(Duration::from_millis(150)))
                .half_open_max_calls(1)
                .success_threshold(1)
                .build(),
        );
        let throttle =
            RateLimiter::new(Quota::per_second(4).allow_burst(4), InMemoryBackend::new());
        Self {
            breaker,
            throttle,
            decisions: Mutex::new(Vec::new()),
        }
    }

    /// The handler decision for one request line — breaker-first, then
    /// throttle, then the scripted backend, then the echo.
    fn decide(&self, payload: &str) -> Decision {
        // 1. Circuit: an outage pauses before any capacity is spent.
        if self.breaker.is_open() {
            self.record("circuit-open");
            return Decision::Respond("CIRCUIT_OPEN\n".to_owned());
        }
        // 2. Throttle: GCRA over the single client key.
        let admission = self.throttle.check_sync("proxy-client");
        if !admission.allowed {
            self.record("throttled");
            let retry_ms = admission.retry_after.unwrap_or_default().as_millis();
            return Decision::Respond(format!("THROTTLED retry_after_ms={retry_ms}\n"));
        }
        // 3. Scripted backend failure: the "write to the backend" fails,
        //    recorded through the breaker's manual entry point.
        if payload.starts_with("fail") {
            self.breaker.record_failure();
            self.record("injected-failure");
            return Decision::FailBackend;
        }
        // 4. Echo — a healthy backend exchange records a success.
        self.breaker.record_success();
        self.record("echo");
        Decision::Respond(format!("ECHO:{payload}\n"))
    }

    fn record(&self, decision: &'static str) {
        self.decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(decision);
    }
}

/// One blocking client round-trip: connect, send the payload line, read
/// until the proxy closes the connection (EOF).
fn roundtrip(addr: SocketAddr, payload: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("proxy is accepting");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("write timeout");
    stream
        .write_all(format!("{payload}\n").as_bytes())
        .expect("request write");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

/// The engine loop: registration → accept → read → gate → write →
/// close, driven entirely through completion events. Runs until the
/// shutdown flag is observed between polls.
fn run_proxy(
    mut engine: UringEngine,
    mut pool: BufferPool,
    listener: std::net::TcpListener,
    gate: Arc<Gate>,
    shutdown: Arc<AtomicBool>,
    addr_tx: std::sync::mpsc::Sender<SocketAddr>,
) {
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("uring proxy: no local addr: {err}");
            return;
        }
    };
    if addr_tx.send(addr).is_err() {
        return; // test thread gone
    }
    let lfd = listener.as_raw_fd();
    let accept_token = Token::accept(0);
    if let Err(err) = engine.add_listener(lfd, accept_token) {
        eprintln!("uring proxy: add_listener failed: {err}");
        return;
    }
    // Arm the single outstanding accept SQE (later re-arms happen inside
    // `poll`, per the engine contract).
    if let Err(err) = engine.accept(lfd, accept_token) {
        eprintln!("uring proxy: initial accept failed: {err}");
        return;
    }

    let mut cqes = Vec::new();
    // The single in-flight connection: fd + bytes accumulated so far.
    let mut conn: Option<(i32, Vec<u8>)> = None;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        if let Err(err) = engine.poll(Some(Duration::from_millis(25)), &mut cqes) {
            eprintln!("uring proxy: poll failed (event loop is unrecoverable): {err}");
            return;
        }
        for cqe in cqes.drain(..) {
            match cqe.token.op() {
                Op::Read => {
                    let Some((fd, request)) = conn.as_mut() else {
                        continue;
                    };
                    match cqe.result {
                        Ok(0) => {
                            // Peer EOF mid-request: nothing to gate.
                            let close_fd = *fd;
                            let _ = engine.close(Token::new(Op::Close, SLOT, 9, 0), close_fd);
                            conn = None;
                        }
                        Ok(n) => {
                            // Each ReadFixed completion lands its bytes at
                            // the slot start; accumulate them in order.
                            request.extend_from_slice(&pool.slot(SLOT)[..n as usize]);
                            if request.contains(&REQUEST_END) {
                                let line = String::from_utf8_lossy(
                                    request.split(|b| *b == REQUEST_END).next().unwrap_or(&[]),
                                )
                                .into_owned();
                                let close_fd = *fd;
                                match gate.decide(&line) {
                                    Decision::Respond(text) => {
                                        let out = pool.slot_mut(SLOT);
                                        let len = text.len().min(out.len());
                                        out[..len].copy_from_slice(&text.as_bytes()[..len]);
                                        if let Err(err) = engine.write(
                                            Token::new(Op::Write, SLOT, 2, 0),
                                            close_fd,
                                            SLOT,
                                            len,
                                            0,
                                        ) {
                                            eprintln!("uring proxy: write submit: {err}");
                                            conn = None;
                                        }
                                    }
                                    Decision::FailBackend => {
                                        let _ = engine
                                            .close(Token::new(Op::Close, SLOT, 8, 0), close_fd);
                                        conn = None;
                                    }
                                }
                            } else if request.len() < pool.buf_size() {
                                // Partial line: re-submit the read.
                                let close_fd = *fd;
                                if engine
                                    .read(Token::new(Op::Read, SLOT, 1, 0), close_fd, SLOT)
                                    .is_err()
                                {
                                    conn = None;
                                }
                            } else {
                                let close_fd = *fd;
                                let _ = engine.close(Token::new(Op::Close, SLOT, 7, 0), close_fd);
                                conn = None;
                            }
                        }
                        Err(_) => {
                            let close_fd = *fd;
                            let _ = engine.close(Token::new(Op::Close, SLOT, 6, 0), close_fd);
                            conn = None;
                        }
                    }
                }
                Op::Write => {
                    // Response fully written: close (drives the client's EOF).
                    if let Some((close_fd, _)) = conn {
                        let _ = engine.close(Token::new(Op::Close, SLOT, 5, 0), close_fd);
                    }
                    conn = None;
                }
                Op::Close => {
                    // Terminal state for the connection slot.
                    conn = None;
                }
                // Accept completions are materialized by `poll` into the
                // engine's accepted map; retrieved below.
                _ => {}
            }
        }
        // Retrieve materialized connections (the engine re-armed the
        // listener inside `poll`). The suite drives exactly one client
        // at a time, so a single connection slot is the honest shape.
        match engine.accept(lfd, accept_token) {
            Ok(Some((fd, _peer))) => {
                conn = Some((fd, Vec::with_capacity(64)));
                if engine
                    .read(Token::new(Op::Read, SLOT, 1, 0), fd, SLOT)
                    .is_err()
                {
                    conn = None;
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("uring proxy: accept retrieval failed: {err}");
                return;
            }
        }
    }
}

/// The full substrate flow under admission control: echoes, a throttle
/// rejection, a breaker trip on two injected backend failures, the
/// open-circuit pause, and the half-open recovery echo — with the exact
/// decision sequence and the breaker's own counters asserted.
#[test]
fn echo_proxy_runs_through_ring_throttle_and_breaker() {
    // -- Substrate availability: io_uring is a kernel surface; where it
    //    is denied, the suite says so and skips (see module docs).
    if let Err(err) = uring_kit::probe::Probe::detect() {
        eprintln!("skipping uring_proxy flagship: ring probe unavailable: {err}");
        return;
    }
    let Some(pool) = BufferPool::new(4, 1024) else {
        eprintln!("skipping uring_proxy flagship: buffer pool unavailable");
        return;
    };
    let engine = match UringEngine::new(64, Some(&pool), false) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("skipping uring_proxy flagship: ring creation denied: {err}");
            return;
        }
    };
    let listener = match net::tcp_listener("127.0.0.1:0".parse().expect("addr"), false, 16) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("skipping uring_proxy flagship: no loopback listener: {err}");
            return;
        }
    };

    let gate = Arc::new(Gate::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let gate_for_thread = Arc::clone(&gate);
    let shutdown_for_thread = Arc::clone(&shutdown);
    let proxy = std::thread::spawn(move || {
        run_proxy(
            engine,
            pool,
            listener,
            gate_for_thread,
            shutdown_for_thread,
            addr_tx,
        )
    });
    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("proxy binds and reports its address");

    // -- Phase 1: four echoes drain the throttle burst (4 @ burst 4).
    for payload in ["one", "two", "three", "four"] {
        let response = roundtrip(addr, payload);
        assert_eq!(response, format!("ECHO:{payload}\n"), "healthy echo");
    }
    // -- Phase 2: the fifth immediate request is throttled by GCRA.
    let throttled = roundtrip(addr, "five");
    assert!(
        throttled.starts_with("THROTTLED retry_after_ms="),
        "the burst-exceeding request must be throttled: {throttled:?}"
    );
    let retry_ms: u64 = throttled
        .strip_prefix("THROTTLED retry_after_ms=")
        .and_then(|rest| rest.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        (1..=500).contains(&retry_ms),
        "retry_after must name the GCRA emission window: {retry_ms} ms"
    );

    // -- Phase 3: after the 250 ms/token refill, a scripted backend
    //    failure records breaker failure 1 (no response, clean EOF).
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        roundtrip(addr, "fail-1"),
        "",
        "backend failure: no response"
    );
    assert!(!gate.breaker.is_open(), "one failure must not trip");

    // -- Phase 4: the second failure trips the circuit.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        roundtrip(addr, "fail-2"),
        "",
        "backend failure: no response"
    );
    assert_eq!(gate.breaker.state(), State::Open, "2 consecutive failures");

    // -- Phase 5: the pause — requests during the outage are answered
    //    CIRCUIT_OPEN without invoking the echo path.
    assert_eq!(
        roundtrip(addr, "during-outage"),
        "CIRCUIT_OPEN\n",
        "an open circuit must pause before any capacity is spent"
    );

    // -- Phase 6: cooldown (150 ms) elapses → half-open; the next echo
    //    is admitted as the probe and its success closes the circuit.
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        roundtrip(addr, "recovery"),
        "ECHO:recovery\n",
        "the half-open probe must be admitted and echoed"
    );
    assert_eq!(
        gate.breaker.state(),
        State::Closed,
        "the successful probe must close the circuit"
    );

    // -- Teardown + exact decision ledger.
    shutdown.store(true, Ordering::Relaxed);
    proxy
        .join()
        .expect("the engine loop exits on the shutdown flag");
    let decisions: Vec<&'static str> = gate
        .decisions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        decisions,
        vec![
            "echo",
            "echo",
            "echo",
            "echo",      // the healthy burst
            "throttled", // the GCRA rejection
            "injected-failure",
            "injected-failure", // the trip
            "circuit-open",     // the pause
            "echo",             // the half-open probe
        ],
        "the handler ledger must be the exact scripted sequence"
    );
    let metrics = gate.breaker.metrics();
    assert_eq!(metrics.total_failures, 2, "only the scripted failures");
    assert_eq!(metrics.total_successes, 5, "4 echoes + 1 recovery probe");
    assert!(
        metrics.transitions >= 3,
        "Closed→Open→HalfOpen→Closed must appear in the transitions"
    );
}

/// The pure substrate contracts, runnable on any host: token packing
/// round-trips, kernel-version parsing, and the registered-buffer pool
/// lifecycle. This is the part of the suite that stays meaningful even
/// where ring creation is denied.
#[test]
fn token_probe_and_pool_contracts_hold() {
    // -- Token packing: [ op: 8 | gen: 16 | slot: 24 | aux: 16 ].
    let token = Token::new(Op::Read, 17, 513, 9);
    assert_eq!(token.op(), Op::Read);
    assert_eq!(token.slot(), 17);
    assert_eq!(token.generation(), 513);
    assert_eq!(token.aux(), 9);
    assert_eq!(Token::from_bits(token.bits()), token, "bits round-trip");
    let listener_token = Token::accept(3);
    assert_eq!(listener_token.op(), Op::Accept);
    assert_eq!(listener_token.aux(), 3, "accept aux names the listener");

    // -- Kernel-version parsing (pure, no probe needed).
    assert_eq!(
        uring_kit::probe::parse_kernel_version("6.8.0-42-generic"),
        (6, 8)
    );
    assert_eq!(
        uring_kit::probe::parse_kernel_version("5.15.0-1047-aws"),
        (5, 15)
    );
    assert!(
        !uring_kit::probe::KNOWN_OPCODES.is_empty(),
        "the opcode table is populated"
    );

    // -- BufferPool lifecycle: slots exhaust and release exactly.
    let mut pool = BufferPool::new(2, 64).expect("small pool");
    assert_eq!(pool.capacity(), 2);
    assert_eq!(pool.buf_size(), 64);
    assert_eq!(pool.free_slots(), 2);
    let first = pool.take().expect("first slot");
    let second = pool.take().expect("second slot");
    assert!(pool.take().is_none(), "exhausted pool yields None");
    pool.release(first);
    assert_eq!(pool.free_slots(), 1);
    assert_eq!(pool.take(), Some(first), "the released slot re-arms");
    pool.release(second);
    pool.slot_mut(second)[0] = 42;
    assert_eq!(pool.slot(second)[0], 42, "slots are addressable in place");

    // -- The live probe, when the kernel offers one, reports honestly.
    match uring_kit::probe::Probe::detect() {
        Ok(probe) => {
            assert!(!probe.kernel_release().is_empty());
            assert!(!probe.opcode_report().is_empty());
        }
        Err(err) => eprintln!("ring probe unavailable on this host: {err}"),
    }
}
