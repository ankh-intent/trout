use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::RelayError;
use crate::store::{Store, internal};

/// Traffic figures, either for one request, one day or all time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Traffic {
    /// Requests passed straight through to a provider.
    pub proxy_requests: u64,
    /// Successful `/v1/responses` turns.
    pub response_turns: u64,
    /// Stored conversations begun.
    pub conversations_started: u64,
    /// Requests that ended with an error status, from the relay or the provider.
    pub failed_requests: u64,
    /// Request bytes the client actually sent.
    pub client_up_bytes: u64,
    /// Request bytes the provider received: what the client would have sent without the relay.
    pub provider_up_bytes: u64,
    /// Answer bytes the client actually received.
    pub client_down_bytes: u64,
    /// Answer bytes before compression.
    pub plain_down_bytes: u64,
}

impl Traffic {
    pub fn upload_saved_bytes(&self) -> u64 {
        self.provider_up_bytes.saturating_sub(self.client_up_bytes)
    }

    pub fn download_saved_bytes(&self) -> u64 {
        self.plain_down_bytes.saturating_sub(self.client_down_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DayTraffic {
    /// UTC date, `YYYY-MM-DD`.
    pub day: String,
    #[serde(flatten)]
    pub traffic: Traffic,
}

/// What is in the store right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StoreSnapshot {
    pub conversations: u64,
    pub responses: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub totals: Traffic,
    /// Most recent days that saw traffic, oldest first.
    pub days: Vec<DayTraffic>,
    pub store: StoreSnapshot,
}

const COLUMNS: &str = "proxy_requests, response_turns, conversations_started, failed_requests,
     client_up_bytes, provider_up_bytes, client_down_bytes, plain_down_bytes";

/// Adds `delta` to today's row.
pub(crate) fn add(conn: &Connection, delta: &Traffic) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO stats_daily (day, {COLUMNS})
             VALUES (date('now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (day) DO UPDATE SET
                 proxy_requests = proxy_requests + excluded.proxy_requests,
                 response_turns = response_turns + excluded.response_turns,
                 conversations_started = conversations_started + excluded.conversations_started,
                 failed_requests = failed_requests + excluded.failed_requests,
                 client_up_bytes = client_up_bytes + excluded.client_up_bytes,
                 provider_up_bytes = provider_up_bytes + excluded.provider_up_bytes,
                 client_down_bytes = client_down_bytes + excluded.client_down_bytes,
                 plain_down_bytes = plain_down_bytes + excluded.plain_down_bytes"
        ),
        params![
            delta.proxy_requests as i64,
            delta.response_turns as i64,
            delta.conversations_started as i64,
            delta.failed_requests as i64,
            delta.client_up_bytes as i64,
            delta.provider_up_bytes as i64,
            delta.client_down_bytes as i64,
            delta.plain_down_bytes as i64,
        ],
    )
    .map(|_| ())
}

fn traffic_from(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Traffic> {
    let get = |index: usize| row.get::<_, i64>(offset + index).map(|value| value.max(0) as u64);

    Ok(Traffic {
        proxy_requests: get(0)?,
        response_turns: get(1)?,
        conversations_started: get(2)?,
        failed_requests: get(3)?,
        client_up_bytes: get(4)?,
        provider_up_bytes: get(5)?,
        client_down_bytes: get(6)?,
        plain_down_bytes: get(7)?,
    })
}

fn read(conn: &Connection, days: u32) -> rusqlite::Result<Stats> {
    let totals = conn.query_row(
        "SELECT COALESCE(SUM(proxy_requests), 0), COALESCE(SUM(response_turns), 0),
                COALESCE(SUM(conversations_started), 0), COALESCE(SUM(failed_requests), 0),
                COALESCE(SUM(client_up_bytes), 0), COALESCE(SUM(provider_up_bytes), 0),
                COALESCE(SUM(client_down_bytes), 0), COALESCE(SUM(plain_down_bytes), 0)
         FROM stats_daily",
        [],
        |row| traffic_from(row, 0),
    )?;
    let mut statement = conn.prepare(&format!(
        "SELECT day, {COLUMNS} FROM stats_daily ORDER BY day DESC LIMIT ?1"
    ))?;
    let mut recent = statement
        .query_map([days], |row| {
            Ok(DayTraffic {
                day: row.get(0)?,
                traffic: traffic_from(row, 1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    recent.reverse();

    let store = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM conversations),
                (SELECT COUNT(*) FROM responses),
                (SELECT COALESCE(SUM(bytes), 0) FROM conversations)",
        [],
        |row| {
            Ok(StoreSnapshot {
                conversations: row.get::<_, i64>(0)?.max(0) as u64,
                responses: row.get::<_, i64>(1)?.max(0) as u64,
                bytes: row.get::<_, i64>(2)?.max(0) as u64,
            })
        },
    )?;

    Ok(Stats {
        totals,
        days: recent,
        store,
    })
}

impl Store {
    pub async fn record_traffic(&self, delta: Traffic) -> Result<(), RelayError> {
        self.run(move |conn| add(conn, &delta).map_err(internal)).await
    }

    /// All-time totals, the last `days` days that saw traffic, and what is stored now.
    pub async fn stats(&self, days: u32) -> Result<Stats, RelayError> {
        self.run(move |conn| read(conn, days).map_err(internal)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (std::path::PathBuf, Store) {
        let path = std::env::temp_dir().join(format!("trout-stats-{}.db", uuid::Uuid::new_v4().simple()));
        let store = Store::open(&path).unwrap();

        (path, store)
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn empty_store_reports_zeroes() {
        let (path, store) = temp_store();
        let stats = store.stats(30).await.unwrap();

        assert_eq!(stats.totals, Traffic::default());
        assert!(stats.days.is_empty());
        assert_eq!(stats.store, StoreSnapshot::default());

        cleanup(&path);
    }

    #[tokio::test]
    async fn traffic_accumulates_into_today_and_survives_reopen() {
        let (path, store) = temp_store();
        let request = Traffic {
            proxy_requests: 1,
            client_up_bytes: 200,
            provider_up_bytes: 2_000,
            client_down_bytes: 50,
            plain_down_bytes: 300,
            ..Traffic::default()
        };
        let turn = Traffic {
            response_turns: 1,
            failed_requests: 1,
            client_up_bytes: 100,
            provider_up_bytes: 5_000,
            client_down_bytes: 80,
            plain_down_bytes: 80,
            ..Traffic::default()
        };

        store.record_traffic(request).await.unwrap();
        store.record_traffic(turn).await.unwrap();
        drop(store);

        let stats = Store::open(&path).unwrap().stats(30).await.unwrap();

        assert_eq!(stats.days.len(), 1);
        assert_eq!(stats.days[0].traffic, stats.totals);
        assert_eq!(stats.days[0].day.len(), 10);
        assert_eq!(stats.totals.proxy_requests, 1);
        assert_eq!(stats.totals.response_turns, 1);
        assert_eq!(stats.totals.failed_requests, 1);
        assert_eq!(stats.totals.client_up_bytes, 300);
        assert_eq!(stats.totals.provider_up_bytes, 7_000);
        assert_eq!(stats.totals.upload_saved_bytes(), 6_700);
        assert_eq!(stats.totals.download_saved_bytes(), 250);

        cleanup(&path);
    }

    #[tokio::test]
    async fn days_are_limited_and_oldest_first() {
        let (path, store) = temp_store();

        store
            .run(|conn| {
                for (day, turns) in [("2026-01-01", 1), ("2026-01-02", 2), ("2026-01-03", 3)] {
                    conn.execute(
                        "INSERT INTO stats_daily (day, response_turns) VALUES (?1, ?2)",
                        params![day, turns],
                    )
                    .map_err(internal)?;
                }

                Ok(())
            })
            .await
            .unwrap();

        let stats = store.stats(2).await.unwrap();
        let days: Vec<&str> = stats.days.iter().map(|d| d.day.as_str()).collect();

        assert_eq!(days, ["2026-01-02", "2026-01-03"]);
        assert_eq!(stats.totals.response_turns, 6);

        cleanup(&path);
    }

    #[test]
    fn saving_never_goes_negative() {
        let traffic = Traffic {
            client_up_bytes: 10,
            provider_up_bytes: 4,
            ..Traffic::default()
        };

        assert_eq!(traffic.upload_saved_bytes(), 0);
        assert_eq!(traffic.download_saved_bytes(), 0);
    }

    #[test]
    fn day_serialises_flat() {
        let day = DayTraffic {
            day: "2026-11-11".into(),
            traffic: Traffic {
                response_turns: 2,
                ..Traffic::default()
            },
        };
        let json = serde_json::to_value(&day).unwrap();

        assert_eq!(json["day"], "2026-11-11");
        assert_eq!(json["response_turns"], 2);
    }
}
