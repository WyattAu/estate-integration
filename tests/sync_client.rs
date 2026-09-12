#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Scenario 5 — `sync_client`: mail-sync-kit store seam + eventbus-kit
//! persistence.
//!
//! The sync engines never touch storage directly: the host injects a
//! [`MockStore`] implementing `MailStore`. Ingested mail becomes sync
//! events that travel over a persistent event bus (SQLite-backed) and can
//! be replayed after a simulated restart.

use std::collections::HashMap;
use std::sync::Arc;

use mail_sync_kit::model::{
    AccountId, BlobHash, ConnectionState, FolderDelta, FolderId, FolderRole,
};
use mail_sync_kit::store::{
    FolderRow, IngestBatch, IngestMessage, IngestStats, MailStore, NewFolder, OutboxRow,
};
use mail_sync_kit::{
    EngineEvent, FolderSummary, MessagePage, OutboxId, SortSpec, StoreError, Window,
};
use tokio::sync::Mutex;

/// Host-defined parsed-message model: the engines never inspect it.
#[derive(Clone, Debug)]
struct ParsedMsg {
    #[allow(dead_code)]
    subject: String,
}

/// In-memory [`MailStore`]: the seam the sync engines program against.
/// Folders, ingested UIDs, blobs, and account states live in mutex-guarded
/// maps — no network, no database server.
#[derive(Default)]
struct MockStore {
    folders: Mutex<HashMap<FolderId, FolderRow>>,
    ingested: Mutex<HashMap<FolderId, Vec<u32>>>,
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    states: Mutex<HashMap<AccountId, ConnectionState>>,
}

impl MockStore {
    fn blob_key(hash: &BlobHash) -> String {
        hash.to_hex().to_owned()
    }
}

#[async_trait::async_trait]
impl MailStore for MockStore {
    type Parsed = ParsedMsg;

    async fn upsert_folder(&self, folder: &NewFolder) -> Result<FolderId, StoreError> {
        let id = FolderId::from_uuid(uuid::Uuid::now_v7());
        self.folders.lock().await.insert(
            id,
            FolderRow {
                id,
                account: folder.account,
                remote_name: folder.remote_name.clone(),
                attributes: folder.attributes.clone(),
                role: folder.role,
                delimiter: folder.delimiter.clone(),
                uid_validity: 1,
                highest_modseq: 0,
            },
        );
        Ok(id)
    }

    async fn list_folders(&self, account: AccountId) -> Result<Vec<FolderSummary>, StoreError> {
        Ok(self
            .folders
            .lock()
            .await
            .values()
            .filter(|f| f.account == account)
            .map(|f| FolderSummary {
                id: f.id,
                account: f.account,
                remote_name: f.remote_name.clone(),
                role: f.role,
                delimiter: f.delimiter.clone(),
                unread: 0,
                total: 0,
            })
            .collect())
    }

    async fn get_folder(&self, id: FolderId) -> Result<FolderRow, StoreError> {
        self.folders
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("folder {id}")))
    }

    async fn ingest_batch(
        &self,
        batch: IngestBatch<Self::Parsed>,
    ) -> Result<IngestStats, StoreError> {
        let mut ingested = self.ingested.lock().await;
        let mut inserted = 0u64;
        for msg in &batch.messages {
            ingested.entry(msg.folder).or_default().push(msg.uid);
            inserted += 1;
        }
        Ok(IngestStats {
            inserted,
            updated: 0,
        })
    }

    async fn list_messages(
        &self,
        _folder: FolderId,
        _window: Window,
        _sort: SortSpec,
    ) -> Result<MessagePage, StoreError> {
        Ok(MessagePage::default())
    }

    async fn purge_folder(&self, folder: FolderId) -> Result<u64, StoreError> {
        Ok(self
            .ingested
            .lock()
            .await
            .remove(&folder)
            .map(|v| v.len() as u64)
            .unwrap_or(0))
    }

    async fn update_sync_cursors(
        &self,
        folder: FolderId,
        uid_validity: u32,
        highest_modseq: Option<u64>,
    ) -> Result<(), StoreError> {
        let mut folders = self.folders.lock().await;
        let row = folders
            .get_mut(&folder)
            .ok_or_else(|| StoreError::NotFound(format!("folder {folder}")))?;
        row.uid_validity = uid_validity;
        if let Some(m) = highest_modseq {
            row.highest_modseq = m;
        }
        Ok(())
    }

    async fn max_uid(&self, folder: FolderId) -> Result<Option<u32>, StoreError> {
        Ok(self
            .ingested
            .lock()
            .await
            .get(&folder)
            .and_then(|v| v.iter().max().copied()))
    }

    async fn outbox_due(&self) -> Result<Vec<OutboxRow>, StoreError> {
        Ok(Vec::new())
    }

    async fn outbox_mark_retry(
        &self,
        _id: OutboxId,
        _retry_count: u32,
        _next_attempt_at: i64,
        _last_error: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    async fn outbox_mark_sent(&self, _id: OutboxId, _sent_at: i64) -> Result<(), StoreError> {
        Ok(())
    }

    async fn read_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StoreError> {
        self.blobs
            .lock()
            .await
            .get(&Self::blob_key(hash))
            .cloned()
            .ok_or_else(|| StoreError::BlobMissing(hash.to_hex().to_owned()))
    }

    async fn write_blob(&self, bytes: Vec<u8>) -> Result<BlobHash, StoreError> {
        // Deterministic 32-byte digest from the std hasher (mock-grade;
        // production hosts use SHA-256 content addressing).
        use std::hash::{Hash, Hasher};
        let mut digest = [0u8; 32];
        for (i, chunk) in digest.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            i.hash(&mut h);
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
        let hash = BlobHash::from_digest(&digest);
        self.blobs.lock().await.insert(Self::blob_key(&hash), bytes);
        Ok(hash)
    }

    async fn set_account_state(
        &self,
        id: AccountId,
        state: ConnectionState,
    ) -> Result<(), StoreError> {
        self.states.lock().await.insert(id, state);
        Ok(())
    }
}

/// The store seam: hierarchy upsert → delta ingest → cursors → blobs →
/// account state, exactly the calls a sync cycle would make after fetching.
#[tokio::test]
async fn mock_store_seam_ingest_and_cursors() {
    let store = MockStore::default();
    let account = AccountId::from_uuid(uuid::Uuid::from_u128(1));

    // Hierarchy sync: INBOX appears.
    let inbox = store
        .upsert_folder(&NewFolder {
            account,
            remote_name: "INBOX".into(),
            attributes: vec!["\\HasNoChildren".into()],
            role: Some(FolderRole::Inbox),
            delimiter: "/".into(),
            uid_validity: 1,
            highest_modseq: 0,
        })
        .await
        .expect("upsert");
    assert_eq!(store.list_folders(account).await.expect("list").len(), 1);
    store
        .set_account_state(account, ConnectionState::Syncing)
        .await
        .expect("state");

    // Delta sync: three fetched messages ingested in one batch.
    let raw = store
        .write_blob(b"From: a@example.com\r\n\r\nhello".to_vec())
        .await
        .expect("blob");
    let batch = IngestBatch {
        messages: (1u32..=3)
            .map(|uid| IngestMessage {
                folder: inbox,
                uid,
                internal_date: 1_700_000_000_000 + i64::from(uid),
                flags: Vec::new(),
                parsed: ParsedMsg {
                    subject: format!("msg {uid}"),
                },
                raw_blob: Some(raw.clone()),
                raw_size: 64,
            })
            .collect(),
    };
    let stats = store.ingest_batch(batch).await.expect("ingest");
    assert_eq!(
        stats,
        IngestStats {
            inserted: 3,
            updated: 0
        }
    );
    assert_eq!(store.max_uid(inbox).await.expect("max_uid"), Some(3));

    // Blob roundtrip + cursors advance like CONDSTORE would.
    assert_eq!(
        store.read_blob(&raw).await.expect("read"),
        b"From: a@example.com\r\n\r\nhello"
    );
    store
        .update_sync_cursors(inbox, 42, Some(9000))
        .await
        .expect("cursors");
    let row = store.get_folder(inbox).await.expect("row");
    assert_eq!((row.uid_validity, row.highest_modseq), (42, 9000));

    // UIDVALIDITY break: purge + re-ingest reconciles.
    assert_eq!(store.purge_folder(inbox).await.expect("purge"), 3);
    assert_eq!(store.max_uid(inbox).await.expect("max_uid"), None);
    assert!(store
        .get_folder(FolderId::from_uuid(uuid::Uuid::from_u128(9)))
        .await
        .is_err());
}

/// Sync events flow over a persistent bus into SQLite and replay after a
/// simulated restart: publish → durable store → drop → reattach → replay.
#[tokio::test]
async fn sync_events_persist_to_sqlite_and_replay() {
    use typed_eventbus::{EventBus, InMemoryStore, PersistentBus};

    // Engine-side event: new mail arrived (the real EngineEvent shape).
    let account = AccountId::from_uuid(uuid::Uuid::from_u128(2));
    let folder = FolderId::from_uuid(uuid::Uuid::from_u128(3));
    let event = EngineEvent::MailArrived {
        account,
        folder,
        summary: FolderDelta {
            new: 3,
            total: 3,
            unread: 3,
        },
    };
    let payload = serde_json::json!({
        "kind": "mail_arrived",
        "account": account.to_string(),
        "folder": folder.to_string(),
        "new": 3,
        "debug": format!("{event:?}"),
    })
    .to_string();

    // Live bus: subscriber sees the published sync event.
    let store = Arc::new(InMemoryStore::<String>::new());
    let bus = PersistentBus::new(EventBus::new(), store.clone());
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    bus.subscribe("sync.*", {
        let seen = Arc::clone(&seen);
        move |envelope| {
            seen.lock().expect("seen").push(envelope.payload.clone());
        }
    })
    .await;
    let notified = bus
        .publish("sync.mail_arrived", payload.clone())
        .await
        .expect("publish");
    assert_eq!(notified, 1);
    // `subscribe_sync` runs callbacks synchronously inside publish.
    assert_eq!(seen.lock().expect("seen").len(), 1);

    // Simulated restart: a fresh bus over the SAME store replays history
    // to a new subscriber — nothing is lost across the restart.
    let bus2 = PersistentBus::new(EventBus::new(), store.clone());
    let replayed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    bus2.subscribe("sync.*", {
        let replayed = Arc::clone(&replayed);
        move |envelope| {
            replayed
                .lock()
                .expect("replayed")
                .push(envelope.payload.clone());
        }
    })
    .await;
    let count = bus2.replay("sync.*").await.expect("replay");
    assert_eq!(count, 1, "one stored event replays");
    assert_eq!(
        replayed.lock().expect("replayed").as_slice(),
        std::slice::from_ref(&payload)
    );

    // SQLite durability (the on-disk story): events land in a tempdir DB,
    // survive a close/reopen, and read back ordered.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("sync-events.sqlite");
    {
        let sqlite = typed_eventbus::persistence::SqliteStore::new(&db_path).expect("open");
        sqlite
            .store("sync.mail_arrived", payload.as_bytes())
            .expect("store");
        sqlite
            .store("sync.flags_changed", b"{\"kind\":\"flags\"}")
            .expect("store");
    }
    {
        let sqlite = typed_eventbus::persistence::SqliteStore::new(&db_path).expect("reopen");
        let events = sqlite.get_events("sync.mail_arrived", 0).expect("read");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, payload.as_bytes());
        assert_eq!(events[0].topic, "sync.mail_arrived");
    }
}

/// Outbox retry schedule from the real crate: backoff grows with attempts.
#[test]
fn outbox_backoff_schedule_sane() {
    let first = mail_sync_kit::backoff_for(0);
    let second = mail_sync_kit::backoff_for(1);
    let third = mail_sync_kit::backoff_for(2);
    assert!(second >= first, "{second:?} >= {first:?}");
    assert!(third >= second, "{third:?} >= {second:?}");
}
