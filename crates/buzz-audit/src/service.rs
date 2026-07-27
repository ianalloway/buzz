use chrono::{DateTime, Utc};
use futures_util::FutureExt as _;
use sqlx::{Acquire, PgPool, Row};
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use buzz_core::CommunityId;

use crate::{
    action::AuditAction,
    entry::{AuditEntry, NewAuditEntry},
    error::AuditError,
    hash::compute_hash,
};

/// Per-community advisory lock key. Derived in Postgres from the community UUID
/// so two communities never serialize each other's audit writes (which would be
/// both a throughput bottleneck and a cross-tenant timing oracle). The lock is
/// taken with `pg_advisory_lock(hashtextextended(...))` — see [`AuditService::log`].
const AUDIT_LOCK_NAMESPACE: &str = "buzz_audit:";

/// Sequence number of the first entry in every community's chain.
///
/// [`AuditService::log`] assigns `prev_seq + 1` starting from an absent head, so
/// a community's first entry is always `seq = 1` with a `NULL` `prev_hash`.
/// [`AuditService::verify_chain`] relies on that to detect a chain whose
/// earliest entries have been deleted.
const GENESIS_SEQ: i64 = 1;

/// Append-only, per-community hash-chain audit log backed by Postgres.
///
/// Each community has an independent chain keyed `(community_id, seq)`. Writes
/// for one community are serialized by a per-community advisory lock so the chain
/// stays consistent across relay processes; different communities proceed in
/// parallel.
pub struct AuditService {
    pool: PgPool,
}

impl AuditService {
    /// Creates a new `AuditService` using the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Append a new entry to the calling community's chain.
    ///
    /// Serialized per-community via `pg_advisory_lock`. Postgres advisory locks
    /// are session-scoped, so we acquire before the transaction and release
    /// after commit (or on any error path).
    #[instrument(skip(self, entry), fields(action = %entry.action))]
    pub async fn log(&self, entry: NewAuditEntry) -> Result<AuditEntry, AuditError> {
        let mut conn = self.pool.acquire().await?;

        // Per-community advisory lock: hash the namespaced community id to an
        // i64 lock key inside Postgres. Communities lock independently.
        let lock_key = format!("{AUDIT_LOCK_NAMESPACE}{}", entry.community_id);
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(&lock_key)
            .execute(&mut *conn)
            .await?;

        // Run the chain append and release the lock regardless of outcome.
        // catch_unwind so a panic still releases the lock before the connection
        // returns to the pool.
        let result = std::panic::AssertUnwindSafe(self.log_inner(&mut conn, entry))
            .catch_unwind()
            .await;

        let _ = sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(&lock_key)
            .execute(&mut *conn)
            .await;

        match result {
            Ok(inner_result) => inner_result,
            Err(panic_payload) => std::panic::resume_unwind(panic_payload),
        }
    }

    async fn log_inner(
        &self,
        conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
        entry: NewAuditEntry,
    ) -> Result<AuditEntry, AuditError> {
        let mut tx = conn.begin().await?;

        // The stored row keys on the raw UUID; the typed `CommunityId` on the
        // input is the provenance fence, dereferenced here at the DB boundary.
        let community_id = *entry.community_id.as_uuid();

        // Head of THIS community's chain — scoped by community_id.
        let head = sqlx::query(
            "SELECT seq, hash FROM audit_log
             WHERE community_id = $1
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(community_id)
        .fetch_optional(&mut *tx)
        .await?;

        let (prev_seq, prev_hash): (i64, Option<Vec<u8>>) = match head {
            Some(row) => (
                row.get::<i64, _>("seq"),
                Some(row.get::<Vec<u8>, _>("hash")),
            ),
            None => (0, None), // community's first entry
        };
        let seq = prev_seq + 1;

        let created_at: DateTime<Utc> = db_precision_now();

        let mut audit_entry = AuditEntry {
            community_id,
            seq,
            hash: Vec::new(),
            prev_hash,
            action: entry.action,
            actor_pubkey: entry.actor_pubkey,
            object_id: entry.object_id,
            detail: entry.detail,
            created_at,
        };

        audit_entry.hash = compute_hash(&audit_entry)?.to_vec();

        debug!(seq, "writing audit entry");

        sqlx::query(
            r#"
            INSERT INTO audit_log
                (community_id, seq, hash, prev_hash, action, actor_pubkey, object_id, detail, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(audit_entry.community_id)
        .bind(audit_entry.seq)
        .bind(&audit_entry.hash)
        .bind(audit_entry.prev_hash.as_deref())
        .bind(audit_entry.action.as_str())
        .bind(audit_entry.actor_pubkey.as_deref())
        .bind(audit_entry.object_id.as_deref())
        .bind(&audit_entry.detail)
        .bind(audit_entry.created_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(audit_entry)
    }

    /// Verify the hash chain for one community over `[from_seq, to_seq]`.
    ///
    /// Reads exactly that community's chain — it can never observe another
    /// community's entries or head. Returns `Ok(false)` if the range is empty,
    /// `Ok(true)` if the segment is internally consistent.
    ///
    /// ## What a passing result does and does not mean
    ///
    /// Each entry's `prev_hash` is checked against its predecessor's hash and
    /// its digest is recomputed, so *interior* edits and deletions are caught:
    /// a survivor's `prev_hash` no longer matches the row that now precedes it.
    ///
    /// The first row in a range has no predecessor inside the range to link
    /// against. When `from_seq <= 1` the caller is asking about the chain from
    /// its start, so the segment must actually begin at the genesis row
    /// ([`GENESIS_SEQ`] with a `NULL` `prev_hash`, which is what
    /// [`AuditService::log`] always writes first). Without that check, deleting
    /// entries `1..k` left a remainder that verified cleanly — the earliest
    /// entries, community creation and initial owner grants, being exactly what
    /// an attacker would drop.
    ///
    /// **Still not detected: truncation of the tail.** Removing the newest
    /// entries leaves a prefix that is genuinely self-consistent, and nothing in
    /// the table distinguishes "chain ends here" from "chain was cut here".
    /// Catching that needs an anchor outside the log — a signed or externally
    /// replicated head — which this type does not have.
    #[instrument(skip(self))]
    pub async fn verify_chain(
        &self,
        community: CommunityId,
        from_seq: i64,
        to_seq: i64,
    ) -> Result<bool, AuditError> {
        let rows = sqlx::query(
            r#"
            SELECT community_id, seq, hash, prev_hash, action, actor_pubkey,
                   object_id, detail, created_at
            FROM audit_log
            WHERE community_id = $1 AND seq BETWEEN $2 AND $3
            ORDER BY seq ASC
            "#,
        )
        .bind(community.as_uuid())
        .bind(from_seq)
        .bind(to_seq)
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(false);
        }

        let mut expected_prev: Option<Vec<u8>> = None;

        for (idx, row) in rows.iter().enumerate() {
            let entry = row_to_audit_entry(row)?;

            // A range that starts at (or before) the chain's beginning must
            // actually contain the genesis row. Nothing inside the segment can
            // reveal that earlier entries were removed, because the survivors
            // still link to each other correctly.
            if idx == 0
                && from_seq <= GENESIS_SEQ
                && (entry.seq != GENESIS_SEQ || entry.prev_hash.is_some())
            {
                return Err(AuditError::ChainViolation { seq: entry.seq });
            }

            if let Some(ref expected) = expected_prev {
                // The previous entry's hash must equal this entry's prev_hash.
                if entry.prev_hash.as_deref() != Some(expected.as_slice()) {
                    return Err(AuditError::ChainViolation { seq: entry.seq });
                }
            }

            let computed = compute_hash(&entry)?;
            if computed.as_slice() != entry.hash.as_slice() {
                return Err(AuditError::HashMismatch { seq: entry.seq });
            }

            expected_prev = Some(entry.hash);
        }

        Ok(true)
    }

    /// Returns up to `limit` entries from one community's chain starting at
    /// `from_seq`, ordered by sequence number. Scoped to `community` — never
    /// returns another community's rows.
    #[instrument(skip(self))]
    pub async fn get_entries(
        &self,
        community: CommunityId,
        from_seq: i64,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        let rows = sqlx::query(
            r#"
            SELECT community_id, seq, hash, prev_hash, action, actor_pubkey,
                   object_id, detail, created_at
            FROM audit_log
            WHERE community_id = $1 AND seq >= $2
            ORDER BY seq ASC
            LIMIT $3
            "#,
        )
        .bind(community.as_uuid())
        .bind(from_seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_audit_entry).collect()
    }
}

/// `Utc::now()` truncated to the microsecond precision a Postgres `timestamptz`
/// actually stores.
///
/// The audit hash commits to `created_at`, so the value that gets hashed must be
/// one the column can hold *exactly*. `Utc::now()` carries nanoseconds on Linux
/// and Postgres rounds them away on write, so hashing the un-truncated instant
/// binds the entry to a timestamp that no longer exists once the row is read
/// back: [`AuditService::verify_chain`] then recomputes a different digest and
/// reports [`AuditError::HashMismatch`] on a chain nobody tampered with.
///
/// Truncating here — before both the hash and the INSERT bind — keeps the hashed
/// and stored timestamps byte-identical, so the chain round-trips.
fn db_precision_now() -> DateTime<Utc> {
    let now = Utc::now();
    // Only fails outside the representable range, which `Utc::now()` never is.
    DateTime::from_timestamp_micros(now.timestamp_micros()).unwrap_or(now)
}

fn row_to_audit_entry(row: &sqlx::postgres::PgRow) -> Result<AuditEntry, AuditError> {
    let action_str: String = row.get("action");
    let action: AuditAction = action_str.parse().map_err(|_| {
        warn!("unknown action in audit log");
        AuditError::UnknownAction
    })?;

    Ok(AuditEntry {
        community_id: row.get::<Uuid, _>("community_id"),
        seq: row.get("seq"),
        hash: row.get("hash"),
        prev_hash: row.get("prev_hash"),
        action,
        actor_pubkey: row.get("actor_pubkey"),
        object_id: row.get("object_id"),
        detail: row.get("detail"),
        created_at: row.get("created_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::AuditAction;
    use crate::entry::NewAuditEntry;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    // The per-community advisory lock means different communities don't contend,
    // but tests share one table; serialize them so seq assertions are stable.
    static DB_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn db_lock() -> &'static Mutex<()> {
        DB_LOCK.get_or_init(|| Mutex::new(()))
    }

    async fn test_pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://buzz:buzz_dev@localhost:5432/buzz".into());
        PgPool::connect(&url).await.ok()
    }

    /// A `community_id` known to exist in `communities` (FK target). Inserts a
    /// throwaway community row with a unique host and returns its id.
    async fn make_community(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        let host = format!("test-{id}.example");
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(id)
            .bind(host)
            .execute(pool)
            .await
            .expect("insert test community");
        id
    }

    fn new_entry(community_id: Uuid, action: AuditAction) -> NewAuditEntry {
        NewAuditEntry {
            community_id: CommunityId::from_uuid(community_id),
            action,
            actor_pubkey: Some(vec![0xab; 32]),
            object_id: Some(format!("obj_{}", Uuid::new_v4())),
            detail: serde_json::json!({"test": true}),
        }
    }

    /// The audit hash commits to `created_at`, and Postgres `timestamptz` holds
    /// only microseconds. If the hashed instant carries sub-microsecond nanos —
    /// which `Utc::now()` does on Linux — the stored row rounds them away and
    /// every later `verify_chain` recomputes a different digest, reporting
    /// `HashMismatch` on an untampered chain.
    ///
    /// This runs without infra deliberately: the Postgres-backed tests below
    /// are `#[ignore]`d, so they are not what catches a regression here.
    #[test]
    fn hashed_timestamp_is_already_at_postgres_precision() {
        for _ in 0..2_000 {
            let ts = db_precision_now();
            assert_eq!(
                ts.timestamp_subsec_nanos() % 1_000,
                0,
                "created_at must carry no sub-microsecond component before it is \
                 hashed, or the chain cannot survive a Postgres round-trip: {ts:?}"
            );
        }
    }

    /// A timestamp and its microsecond-truncated form hash differently, which is
    /// exactly why the truncation has to happen before `compute_hash`, not after.
    #[test]
    fn sub_microsecond_nanos_change_the_entry_hash() {
        let with_nanos = DateTime::from_timestamp_nanos(1_767_225_600_123_456_789);
        let truncated = DateTime::from_timestamp_micros(with_nanos.timestamp_micros())
            .expect("in-range timestamp");
        assert_ne!(with_nanos, truncated, "the two instants must differ");

        let mut a = AuditEntry {
            community_id: Uuid::from_u128(1),
            seq: 1,
            hash: Vec::new(),
            prev_hash: None,
            action: AuditAction::EventCreated,
            actor_pubkey: Some(vec![0xab; 32]),
            object_id: Some("abc123".into()),
            detail: serde_json::Value::Null,
            created_at: with_nanos,
        };
        let hash_with_nanos = compute_hash(&a).expect("hash");
        a.created_at = truncated;
        let hash_truncated = compute_hash(&a).expect("hash");

        assert_ne!(
            hash_with_nanos, hash_truncated,
            "sub-microsecond nanos must change the digest — this is the round-trip hazard"
        );
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn community_chain_starts_at_seq_1_with_null_prev() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let c = make_community(&pool).await;

        let e = svc
            .log(new_entry(c, AuditAction::EventCreated))
            .await
            .unwrap();
        assert_eq!(e.seq, 1, "first entry in a community starts at seq 1");
        assert!(e.prev_hash.is_none(), "genesis entry has NULL prev_hash");
        assert_eq!(e.hash.len(), 32);
        assert_eq!(e.community_id, c);
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn chain_links_within_one_community() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let c = make_community(&pool).await;

        let e1 = svc
            .log(new_entry(c, AuditAction::EventCreated))
            .await
            .unwrap();
        let e2 = svc
            .log(new_entry(c, AuditAction::ChannelCreated))
            .await
            .unwrap();
        let e3 = svc
            .log(new_entry(c, AuditAction::MemberAdded))
            .await
            .unwrap();

        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(e3.seq, 3);
        assert!(e1.prev_hash.is_none());
        assert_eq!(e2.prev_hash.as_deref(), Some(e1.hash.as_slice()));
        assert_eq!(e3.prev_hash.as_deref(), Some(e2.hash.as_slice()));
        assert!(svc
            .verify_chain(CommunityId::from_uuid(c), 1, 3)
            .await
            .unwrap());
    }

    /// THE isolation property: two communities keep independent chains. Each
    /// starts at seq 1; interleaving writes does not link them; verifying one
    /// never traverses the other.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn chains_are_independent_per_community() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let a = make_community(&pool).await;
        let b = make_community(&pool).await;

        // Interleave A and B writes.
        let a1 = svc
            .log(new_entry(a, AuditAction::EventCreated))
            .await
            .unwrap();
        let b1 = svc
            .log(new_entry(b, AuditAction::EventCreated))
            .await
            .unwrap();
        let a2 = svc
            .log(new_entry(a, AuditAction::ChannelCreated))
            .await
            .unwrap();
        let b2 = svc
            .log(new_entry(b, AuditAction::ChannelCreated))
            .await
            .unwrap();

        // Each community's seq is independent and starts at 1.
        assert_eq!((a1.seq, a2.seq), (1, 2));
        assert_eq!((b1.seq, b2.seq), (1, 2));

        // A's chain links only within A; B's only within B. A2 must NOT chain to
        // B1 even though B1 was written between A1 and A2.
        assert_eq!(a2.prev_hash.as_deref(), Some(a1.hash.as_slice()));
        assert_eq!(b2.prev_hash.as_deref(), Some(b1.hash.as_slice()));
        assert_ne!(a2.prev_hash, b1.prev_hash);

        // Verifying A's chain traverses only A; same for B.
        assert!(svc
            .verify_chain(CommunityId::from_uuid(a), 1, 2)
            .await
            .unwrap());
        assert!(svc
            .verify_chain(CommunityId::from_uuid(b), 1, 2)
            .await
            .unwrap());

        // get_entries scoped to A returns only A's rows.
        let a_rows = svc
            .get_entries(CommunityId::from_uuid(a), 1, 100)
            .await
            .unwrap();
        assert!(
            a_rows.iter().all(|e| e.community_id == a),
            "A read leaked another community"
        );
        assert_eq!(a_rows.len(), 2);
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn verify_detects_tampering_within_a_community() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let c = make_community(&pool).await;

        svc.log(new_entry(c, AuditAction::EventCreated))
            .await
            .unwrap();
        let e2 = svc
            .log(new_entry(c, AuditAction::EventDeleted))
            .await
            .unwrap();
        svc.log(new_entry(c, AuditAction::ChannelDeleted))
            .await
            .unwrap();

        // Tamper with e2's stored actor_pubkey.
        let tampered: Vec<u8> = vec![0xff; 32];
        sqlx::query("UPDATE audit_log SET actor_pubkey = $1 WHERE community_id = $2 AND seq = $3")
            .bind(tampered)
            .bind(c)
            .bind(e2.seq)
            .execute(&pool)
            .await
            .unwrap();

        let r = svc.verify_chain(CommunityId::from_uuid(c), 1, 3).await;
        assert!(matches!(r, Err(AuditError::HashMismatch { seq }) if seq == e2.seq));
    }

    /// Deleting the earliest entries must not verify.
    ///
    /// Interior deletions are already caught: the survivor's `prev_hash` points
    /// at a row that is no longer its predecessor, so the link check fires. The
    /// head is the gap — the first row in a range has nothing before it to link
    /// against, so truncating `1..k` leaves a segment that is internally
    /// perfectly consistent. Since the write path always starts a community's
    /// chain at `seq = 1` with `prev_hash = NULL`, a verification that begins at
    /// the chain's start can and must insist on seeing that genesis row.
    ///
    /// This is the high-value target for anyone editing an audit log: the
    /// earliest rows are community creation and the initial owner grants.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn verify_detects_deletion_of_the_chain_head() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let c = make_community(&pool).await;

        for action in [
            AuditAction::EventCreated,
            AuditAction::EventDeleted,
            AuditAction::ChannelDeleted,
        ] {
            svc.log(new_entry(c, action)).await.unwrap();
        }
        assert!(
            svc.verify_chain(CommunityId::from_uuid(c), 1, 3)
                .await
                .unwrap(),
            "intact chain must verify"
        );

        // Excise the genesis entry, as someone hiding the chain's origin would.
        sqlx::query("DELETE FROM audit_log WHERE community_id = $1 AND seq = 1")
            .bind(c)
            .execute(&pool)
            .await
            .unwrap();

        // Entries 2..3 still link to each other, and entry 2's stored prev_hash
        // still matches its own digest, so nothing internal to the segment is
        // wrong — only the missing origin gives it away.
        let r = svc.verify_chain(CommunityId::from_uuid(c), 1, 3).await;
        assert!(
            matches!(r, Err(AuditError::ChainViolation { seq }) if seq == 2),
            "a chain missing its genesis row must not verify, got {r:?}"
        );
    }

    /// Verifying a strict sub-range stays legal: a caller asking about
    /// `[2, 3]` is not claiming to have seen the head, so the genesis
    /// requirement must not fire there.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn verify_of_a_sub_range_does_not_require_genesis() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let c = make_community(&pool).await;

        for action in [
            AuditAction::EventCreated,
            AuditAction::EventDeleted,
            AuditAction::ChannelDeleted,
        ] {
            svc.log(new_entry(c, action)).await.unwrap();
        }

        assert!(
            svc.verify_chain(CommunityId::from_uuid(c), 2, 3)
                .await
                .unwrap(),
            "a sub-range that legitimately starts past seq 1 must still verify"
        );
    }

    /// A row forged with another community's id cannot pass verification against
    /// the chain it was stamped for, because community_id is hashed in. (Models
    /// "a row can't be replayed across chains and still verify".)
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn cross_community_row_does_not_verify() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let a = make_community(&pool).await;
        let b = make_community(&pool).await;

        let a1 = svc
            .log(new_entry(a, AuditAction::EventCreated))
            .await
            .unwrap();

        // Forge: copy A's seq-1 row's hash into B's chain at seq 1.
        sqlx::query(
            "INSERT INTO audit_log (community_id, seq, hash, prev_hash, action, actor_pubkey, object_id, detail, created_at)
             VALUES ($1, 1, $2, NULL, $3, $4, $5, $6, NOW())",
        )
        .bind(b)
        .bind(&a1.hash) // A's hash, which was computed over community_id = A
        .bind(a1.action.as_str())
        .bind(a1.actor_pubkey.as_deref())
        .bind(a1.object_id.as_deref())
        .bind(&a1.detail)
        .execute(&pool)
        .await
        .unwrap();

        // Verifying B's chain recomputes the hash with community_id = B, which
        // won't match A's stored hash → HashMismatch. The forge is rejected.
        let r = svc.verify_chain(CommunityId::from_uuid(b), 1, 1).await;
        assert!(matches!(r, Err(AuditError::HashMismatch { seq: 1 })));
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn verify_empty_range_is_false() {
        let _g = db_lock().lock().await;
        let Some(pool) = test_pool().await else {
            return;
        };
        let svc = AuditService::new(pool.clone());
        let c = make_community(&pool).await;
        // No entries for this fresh community.
        assert!(!svc
            .verify_chain(CommunityId::from_uuid(c), 1, 100)
            .await
            .unwrap());
    }
}
