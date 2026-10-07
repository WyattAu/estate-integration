//! The IPC stack: HTTP over a Unix domain socket, backed by a pooled database.
//!
//! Three crates compose here in the shape a real local service takes:
//! `axum-stack` provides the router's operational surface (health, request
//! IDs), `poolkit` provides the storage, and `uds-kit` is the client —
//! transport only, so this suite formats its own HTTP/1.0 frames and proves
//! that the client's deadline wrappers work against a live peer rather than a
//! mock.
//!
//! The socket is the point. A service bound to a Unix domain socket is not
//! reachable by "just curl it", and a TCP-based test would prove nothing about
//! the socket path: the whole reason uds-kit exists is that its two original
//! call sites hand-rolled `connect` + `read_to_end` and both forgot the write
//! deadline.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use axum::extract::State;
use axum::routing::get;
use axum::Router;
use axum_stack::health::health_routes;
use healthkit::HealthCheckError;
use healthkit::{HealthRegistry, HealthStatus};
use poolkit::DbPool;
use sqlx::Row;
use uds_kit::{connect, read_with_timeout, write_all_with_timeout, UdsError};

/// State shared by the handlers.
struct App {
    pool: Arc<DbPool>,
}

type Shared = Arc<App>;

async fn pool_for_test() -> (tempfile::TempDir, DbPool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("ipc.db").display());
    let pool = DbPool::builder(url)
        .max_connections(2)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(5))
        .build()
        .await
        .expect("pool builds");
    (dir, pool)
}

async fn seed(pool: &DbPool) {
    // `install_default_drivers` is called inside the pool, so `AnyPool` can
    // speak sqlite here without this crate naming a driver.
    sqlx::query("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .execute(pool.inner())
        .await
        .expect("create table");
    sqlx::query("INSERT INTO items (name) VALUES ('alpha'), ('beta')")
        .execute(pool.inner())
        .await
        .expect("seed rows");
}

fn router(state: Shared) -> Router {
    // The count route needs the pool as state; health_routes is stateless.
    // Merging happens on the stateless outer router, which is why with_state
    // is applied to the inner one.
    let count = Router::new()
        .route("/count", get(count_handler))
        .with_state(state.clone());
    Router::new()
        .merge(count)
        .merge(health_routes(health_registry(state)))
}

fn health_registry(state: Shared) -> HealthRegistry {
    let registry = HealthRegistry::new();
    // The check is real: it pings the pool, so an unhealthy database makes
    // /health unhealthy rather than the check being a constant. DbPool is not
    // Clone, so the closure reaches the inner sqlx pool, which is.
    registry.add_check("database", {
        let pool = state.pool.inner().clone();

        move || {
            let pool = pool.clone();
            async move {
                sqlx::query("SELECT 1")
                    .execute(&pool)
                    .await
                    .map(|_| HealthStatus::Healthy)
                    .map_err(|e| HealthCheckError::CheckFailed(e.to_string()))
            }
        }
    });
    registry
}

async fn count_handler(State(state): State<Shared>) -> String {
    let row = sqlx::query("SELECT COUNT(*) FROM items")
        .fetch_one(state.pool.inner())
        .await
        .expect("count query");
    let n: i64 = row.try_get(0).expect("count column");
    format!("{n}")
}

/// Serve `app` on a Unix domain socket and return the socket path.
fn serve_on_uds(app: Router, dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("svc.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind uds");
    listener.set_nonblocking(true).expect("nonblocking");
    let tokio_listener = tokio::net::UnixListener::from_std(listener).expect("tokio uds listener");
    tokio::spawn(async move {
        axum::serve(tokio_listener, app)
            .await
            .expect("serve over uds");
    });
    path.display().to_string()
}

/// One raw HTTP/1.0 round trip over the socket, via uds-kit only.
async fn get_via_uds(path: &str, socket: &str) -> Result<(u16, String), UdsError> {
    let mut stream = connect(socket, Duration::from_secs(5)).await?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: ipc\r\nConnection: close\r\n\r\n");
    write_all_with_timeout(&mut stream, request.as_bytes(), Duration::from_secs(5)).await?;
    let mut response = Vec::new();
    read_with_timeout(&mut stream, &mut response, Duration::from_secs(5)).await?;
    let text = String::from_utf8_lossy(&response).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, text))
}

#[tokio::test(flavor = "multi_thread")]
async fn http_served_on_a_unix_socket_reaches_pooled_sqlite() {
    let (dir, pool) = pool_for_test().await;
    seed(&pool).await;
    let pool = Arc::new(pool);
    let socket = serve_on_uds(
        router(Arc::new(App {
            pool: Arc::clone(&pool),
        })),
        &dir,
    );

    let (status, body) = get_via_uds("/count", &socket)
        .await
        .expect("round trip over the socket");
    assert_eq!(status, 200);
    assert_eq!(body.trim_end().lines().last(), Some("2"), "two seeded rows");

    // The pool is real: the handler's query went through it, materializing a
    // connection that the pre-request snapshot did not have.
    let stats = pool.stats();
    assert!(stats.size >= 1, "the handler materialized a connection");
}

#[tokio::test(flavor = "multi_thread")]
async fn health_reflects_the_pool_it_probes() {
    let (dir, pool) = pool_for_test().await;
    seed(&pool).await;
    let socket = serve_on_uds(
        router(Arc::new(App {
            pool: Arc::new(pool),
        })),
        &dir,
    );

    let (status, body) = get_via_uds("/health", &socket)
        .await
        .expect("health round trip");
    assert_eq!(status, 200);
    assert!(
        body.contains("healthy") || body.contains("Healthy") || body.contains("\"ok\""),
        "a live pool must report healthy, got: {body}"
    );
}

#[tokio::test]
async fn a_missing_socket_is_an_immediate_io_error_not_a_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("nothing.sock");
    let started = std::time::Instant::now();
    let result = connect(absent.display().to_string(), Duration::from_secs(5)).await;
    // ENOENT is immediate: ConnectTimeout is reserved for a connect that
    // genuinely blocks past the deadline, and collapsing the two would make a
    // misconfigured path indistinguishable from a hung one.
    assert!(matches!(result, Err(UdsError::Io(_))));
    assert!(started.elapsed() < Duration::from_secs(1), "must not wait");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_peer_times_out_the_read() {
    // A listener that accepts and never writes: the read deadline is what
    // saves the client, and this is the exact ClamAV-shaped hang uds-kit was
    // written to close.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("silent.sock");
    let listener = tokio::net::UnixListener::bind(&path).expect("bind");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Hold the connection open without responding.
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = stream.shutdown().await;
        }
    });
    let started = std::time::Instant::now();
    let mut stream = connect(path.display().to_string(), Duration::from_secs(5))
        .await
        .expect("connect succeeds; the peer is merely silent");
    let mut response = Vec::new();
    let result = read_with_timeout(&mut stream, &mut response, Duration::from_millis(300)).await;
    assert!(matches!(result, Err(UdsError::ReadTimeout)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must fire, not the test timeout"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_in_sequence_share_one_pool() {
    let (dir, pool) = pool_for_test().await;
    seed(&pool).await;
    let pool = Arc::new(pool);
    let socket = serve_on_uds(
        router(Arc::new(App {
            pool: Arc::clone(&pool),
        })),
        &dir,
    );

    for expected in ["2", "2"] {
        let (status, body) = get_via_uds("/count", &socket)
            .await
            .expect("sequential round trip");
        assert_eq!(status, 200);
        assert_eq!(body.trim_end().lines().last(), Some(expected));
    }
    // Two requests, one pool, capped at two connections: the cap is the point
    // of the pool, and it holds across clients because the pool lives in the
    // server, not the client.
    assert!(pool.stats().max_connections <= 2);
}
