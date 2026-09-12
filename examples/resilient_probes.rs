#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Living example: health probes + circuit breaker composition.
//!
//! ```sh
//! cargo run --example resilient_probes
//! ```

use breaker::{CircuitBreaker, CircuitBreakerConfig};
use healthkit::{HealthRegistry, HealthStatus};

#[tokio::main]
async fn main() {
    // Probes: liveness is unconditional, readiness aggregates checks.
    let registry = HealthRegistry::new();
    registry.add_check("database", || async { Ok(HealthStatus::Healthy) });
    let results = registry.check_all().await;
    for r in &results {
        println!("check {}: {:?}", r.name, r.status);
    }

    // Breaker: trip it with failures, watch it open.
    let breaker = CircuitBreaker::new(CircuitBreakerConfig::standard());
    for _ in 0..10 {
        let _ = breaker
            .call(|| async { Err::<(), _>("upstream 500") })
            .await;
    }
    println!("breaker state after outage: {:?}", breaker.state());
    println!("metrics: {:?}", breaker.metrics());
}
