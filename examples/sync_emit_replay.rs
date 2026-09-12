#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Living example: sync event → persistent bus → replay after restart.
//!
//! ```sh
//! cargo run --example sync_emit_replay
//! ```

use std::sync::Arc;

use typed_eventbus::{EventBus, InMemoryStore, PersistentBus};

#[tokio::main]
async fn main() {
    let store = Arc::new(InMemoryStore::<String>::new());
    let bus = PersistentBus::new(EventBus::new(), store.clone());
    bus.subscribe("sync.*", |envelope| {
        println!("live: [{}] {}", envelope.topic, envelope.payload);
    })
    .await;
    bus.publish("sync.mail_arrived", r#"{"new":3}"#.to_string())
        .await
        .expect("publish");

    // Simulated restart: same store, fresh bus, replay history.
    let bus2 = PersistentBus::new(EventBus::new(), store);
    bus2.subscribe("sync.*", |envelope| {
        println!("replayed: [{}] {}", envelope.topic, envelope.payload);
    })
    .await;
    let count = bus2.replay("sync.*").await.expect("replay");
    println!("replayed {count} event(s) after restart");
}
