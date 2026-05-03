//! Telos store — Week 9 persistence layer.
//!
//! Append-only sqlite log of every intent's lifecycle. Each row is one
//! `(intent_id, stage, payload_json)` tuple, written as the listener
//! progresses through observed → quoted → simulated → decided → settled.
//!
//! Why an event log rather than per-stage tables: the schema stays stable
//! as the domain types evolve, and the lifecycle is naturally append-only
//! (no `UPDATE`s to reason about). The trade-off is no SQL-level filtering
//! on payload contents — but that is what the application layer is for.
//!
//! Why JSON for the payload: serde already serialises the domain types,
//! so we get persistence "for free." A more ambitious schema would shred
//! into typed columns; defer until query patterns demand it.

use alloy::primitives::B256;
use eyre::{Result, WrapErr};
use serde::Serialize;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Lifecycle stages, in observation order. Stored as text so a `sqlite3`
/// session is human-readable; the cost vs storing an integer is trivial.
#[derive(Debug, Clone, Copy)]
pub enum Stage {
    Observed,
    Quoted,
    Simulated,
    Decided,
    Settled,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Quoted => "quoted",
            Self::Simulated => "simulated",
            Self::Decided => "decided",
            Self::Settled => "settled",
        }
    }
}

/// Cloneable handle around a `SqlitePool`. Pool is internally
/// `Arc`-shared, so cloning here is cheap and safe across async tasks.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open a sqlite database at the given URL (e.g. `sqlite://./telos.db`).
    /// Creates the file if missing and runs any pending migrations.
    pub async fn open(url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)
            .wrap_err("invalid TELOS_DB_URL")?
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(opts)
            .await
            .wrap_err("failed to open sqlite pool")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .wrap_err("migration failed")?;

        Ok(Self { pool })
    }

    /// Append one event to the log. `payload` is anything `Serialize`;
    /// it lands as a JSON string in the `payload_json` column.
    pub async fn record_event<P: Serialize>(
        &self,
        intent_id: B256,
        stage: Stage,
        payload: &P,
    ) -> Result<()> {
        let payload_json =
            serde_json::to_string(payload).wrap_err("payload serialisation failed")?;
        let id_hex = format!("{:#x}", intent_id);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let stage_str = stage.as_str();

        sqlx::query(
            "INSERT INTO intent_events (intent_id, stage, payload_json, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&id_hex)
        .bind(stage_str)
        .bind(&payload_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .wrap_err("insert failed")?;

        Ok(())
    }

    /// Distinct intent_ids that have an `observed` row but no `settled` row.
    /// This is the reconciliation set on startup — what was in flight when
    /// the last process died.
    pub async fn count_pending(&self) -> Result<u64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(DISTINCT intent_id) FROM intent_events \
             WHERE stage = 'observed' \
               AND intent_id NOT IN ( \
                   SELECT DISTINCT intent_id FROM intent_events WHERE stage = 'settled' \
               )",
        )
        .fetch_one(&self.pool)
        .await
        .wrap_err("pending query failed")?;
        Ok(row.0 as u64)
    }
}

// Compile-time guard: Store must be Send + Sync so it can be cloned
// across listener tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
};
