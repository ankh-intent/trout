pub mod sweeper;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::RelayError;

const SCHEMA: &str = include_str!("schema.sql");

pub type Settings = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    InProgress,
    Completed,
    Incomplete,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "completed" => Self::Completed,
            "incomplete" => Self::Incomplete,
            _ => Self::InProgress,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot open the conversation store at {path}: {source}")]
    Open { path: String, source: rusqlite::Error },
}

/// What a turn is about to do, as the client asked for it.
#[derive(Debug, Clone, Default)]
pub struct NewTurn {
    pub target: String,
    pub previous_response_id: Option<String>,
    /// Settings fields present in the request. `null` clears an inherited value.
    pub overrides: Settings,
    pub input_items: Vec<Value>,
    pub metadata: Option<Value>,
    pub persist: bool,
}

/// A turn that passed validation, with everything needed to build the upstream request.
#[derive(Debug, Clone)]
pub struct Begun {
    pub response_id: String,
    /// `None` when the turn is not stored.
    pub conversation_id: Option<String>,
    pub previous_response_id: Option<String>,
    pub settings: Settings,
    /// Every item that precedes this turn's input, oldest first.
    pub history: Vec<Value>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct StoredResponse {
    pub id: String,
    pub conversation_id: String,
    pub parent_id: Option<String>,
    pub status: Status,
    pub settings: Settings,
    pub input_items: Vec<Value>,
    pub output_items: Vec<Value>,
    pub usage: Option<Value>,
    pub incomplete_reason: Option<String>,
    pub metadata: Option<Value>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub status: Option<Status>,
    pub output_items: Vec<Value>,
    pub usage: Option<Value>,
    pub incomplete_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub expired: usize,
    pub evicted: usize,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn internal<E>(_: E) -> RelayError {
    RelayError::Internal
}

fn parse_array(raw: &str) -> Vec<Value> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn parse_optional(raw: Option<String>) -> Option<Value> {
    raw.and_then(|raw| serde_json::from_str(&raw).ok())
}

/// Conversations kept on local disk. One connection, used from blocking threads.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let open = || -> rusqlite::Result<Connection> {
            let conn = Connection::open(path)?;

            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.execute_batch(SCHEMA)?;
            // Turns that were running when the previous process stopped can never finish.
            conn.execute("DELETE FROM responses WHERE status = 'in_progress'", [])?;
            conn.execute(
                "DELETE FROM conversations WHERE id NOT IN (SELECT conversation_id FROM responses)",
                [],
            )?;

            Ok(conn)
        };

        open()
            .map(|conn| Self {
                conn: Arc::new(Mutex::new(conn)),
            })
            .map_err(|source| StoreError::Open {
                path: path.display().to_string(),
                source,
            })
    }

    async fn run<T, F>(&self, work: F) -> Result<T, RelayError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, RelayError> + Send + 'static,
    {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(internal)?;

            work(&mut guard)
        })
        .await
        .map_err(internal)?
    }

    /// Validates a turn against the stored conversation and, when it is to be stored, records it
    /// as in progress.
    pub async fn begin(&self, turn: NewTurn) -> Result<Begun, RelayError> {
        self.run(move |conn| begin(conn, turn, now_ms())).await
    }

    /// Records the outcome of a turn in one transaction. A no-op for turns that are not stored.
    pub async fn finalise(&self, response_id: String, completion: Completion) -> Result<(), RelayError> {
        self.run(move |conn| finalise(conn, &response_id, &completion, now_ms()))
            .await
    }

    /// Forgets a turn that did not finish.
    pub async fn abort(&self, response_id: String) -> Result<(), RelayError> {
        self.run(move |conn| abort(conn, &response_id)).await
    }

    pub async fn get(&self, response_id: String) -> Result<StoredResponse, RelayError> {
        self.run(move |conn| {
            let response = load(conn, &response_id)?.ok_or(RelayError::ResponseNotFound)?;

            touch(conn, &response.conversation_id, now_ms())?;

            Ok(response)
        })
        .await
    }

    /// Effective settings and full history up to and including a response: the complete request
    /// prefix for whatever follows it.
    pub async fn request_prefix(&self, response_id: String) -> Result<(Settings, Vec<Value>), RelayError> {
        self.run(move |conn| {
            let response = load(conn, &response_id)?.ok_or(RelayError::ResponseNotFound)?;

            Ok((response.settings, history(conn, &response_id)?))
        })
        .await
    }

    pub async fn delete_response(&self, response_id: String) -> Result<(), RelayError> {
        self.run(move |conn| delete_response(conn, &response_id)).await
    }

    pub async fn delete_conversation(&self, conversation_id: String) -> Result<(), RelayError> {
        self.run(move |conn| delete_conversation(conn, &conversation_id))
            .await
    }

    pub async fn sweep(&self, ttl_ms: i64, max_bytes: u64) -> Result<SweepStats, RelayError> {
        self.run(move |conn| sweep(conn, now_ms(), ttl_ms, max_bytes))
            .await
    }
}

fn settings_hash(settings: &Settings) -> (String, String) {
    // serde_json maps are ordered by key, so this serialisation is canonical.
    let json = Value::Object(settings.clone()).to_string();
    let hash = Sha256::digest(json.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    (hash, json)
}

fn merge_settings(mut base: Settings, overrides: &Settings) -> Settings {
    for (key, value) in overrides {
        if value.is_null() {
            base.remove(key);
        } else {
            base.insert(key.clone(), value.clone());
        }
    }

    base
}

/// Every `function_call_output` must answer a `function_call` that has no output yet.
fn check_tool_pairing(history: &[Value], input: &[Value]) -> Result<(), RelayError> {
    let mut open: HashSet<&str> = HashSet::new();

    for item in history.iter().chain(input) {
        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or_default();

        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                open.insert(call_id);
            }
            Some("function_call_output") if !open.remove(call_id) => {
                return Err(RelayError::InvalidRequest(format!(
                    "function_call_output `{call_id}` does not answer an open function_call"
                )));
            }
            _ => {}
        }
    }

    Ok(())
}

fn begin(conn: &mut Connection, turn: NewTurn, now: i64) -> Result<Begun, RelayError> {
    let parent = match &turn.previous_response_id {
        None => None,
        Some(id) => {
            let parent = load(conn, id)?.ok_or(RelayError::ResponseNotFound)?;

            match parent.status {
                Status::InProgress => return Err(RelayError::ResponseInProgress),
                Status::Incomplete => return Err(RelayError::ResponseNotContinuable),
                Status::Completed => {}
            }

            let target: String = conn
                .query_row(
                    "SELECT target FROM conversations WHERE id = ?1",
                    [&parent.conversation_id],
                    |r| r.get(0),
                )
                .map_err(internal)?;

            if target != turn.target {
                return Err(RelayError::TargetMismatch);
            }

            Some(parent)
        }
    };
    let history = match &parent {
        Some(parent) => history(conn, &parent.id)?,
        None => Vec::new(),
    };
    let settings = merge_settings(
        parent.as_ref().map(|p| p.settings.clone()).unwrap_or_default(),
        &turn.overrides,
    );

    if !settings
        .get("model")
        .is_some_and(|m| m.as_str().is_some_and(|m| !m.is_empty()))
    {
        return Err(RelayError::InvalidRequest(
            "`model` is required on the first turn".to_string(),
        ));
    }

    check_tool_pairing(&history, &turn.input_items)?;

    let response_id = new_id("resp");
    let mut begun = Begun {
        response_id: response_id.clone(),
        conversation_id: None,
        previous_response_id: turn.previous_response_id.clone(),
        settings,
        history,
        created_at_ms: now,
    };

    if let Some(parent) = &parent {
        touch(conn, &parent.conversation_id, now)?;
    }

    if !turn.persist {
        return Ok(begun);
    }

    let tx = conn.transaction().map_err(internal)?;
    let conversation_id = match &parent {
        Some(parent) => parent.conversation_id.clone(),
        None => {
            let id = new_id("conv");

            tx.execute(
                "INSERT INTO conversations (id, target, created_at, last_used_at) VALUES (?1, ?2, ?3, ?3)",
                params![id, turn.target, now],
            )
            .map_err(internal)?;

            id
        }
    };
    let (hash, json) = settings_hash(&begun.settings);

    tx.execute(
        "INSERT OR IGNORE INTO settings (hash, json, bytes) VALUES (?1, ?2, ?3)",
        params![hash, json, json.len() as i64],
    )
    .map_err(internal)?;
    tx.execute(
        "INSERT INTO responses (id, conversation_id, parent_id, status, settings_hash, input_items, metadata, created_at)
         VALUES (?1, ?2, ?3, 'in_progress', ?4, ?5, ?6, ?7)",
        params![
            response_id,
            conversation_id,
            turn.previous_response_id,
            hash,
            Value::Array(turn.input_items).to_string(),
            turn.metadata.map(|m| m.to_string()),
            now,
        ],
    )
    .map_err(internal)?;
    tx.commit().map_err(internal)?;

    begun.conversation_id = Some(conversation_id);

    Ok(begun)
}

fn finalise(
    conn: &mut Connection,
    response_id: &str,
    completion: &Completion,
    now: i64,
) -> Result<(), RelayError> {
    let tx = conn.transaction().map_err(internal)?;
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT conversation_id, input_items FROM responses WHERE id = ?1 AND status = 'in_progress'",
            [response_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((conversation_id, input_items)) = row else {
        return Ok(());
    };
    let output = Value::Array(completion.output_items.clone()).to_string();
    let bytes = (input_items.len() + output.len()) as i64;

    tx.execute(
        "UPDATE responses
         SET status = ?2, output_items = ?3, usage = ?4, incomplete_reason = ?5, completed_at = ?6, bytes = ?7
         WHERE id = ?1",
        params![
            response_id,
            completion.status.unwrap_or(Status::Completed).as_str(),
            output,
            completion.usage.as_ref().map(Value::to_string),
            completion.incomplete_reason,
            now,
            bytes,
        ],
    )
    .map_err(internal)?;
    tx.execute(
        "UPDATE conversations SET bytes = bytes + ?2, last_used_at = ?3 WHERE id = ?1",
        params![conversation_id, bytes, now],
    )
    .map_err(internal)?;
    tx.commit().map_err(internal)
}

fn drop_empty_conversations(conn: &Connection) -> Result<(), RelayError> {
    conn.execute(
        "DELETE FROM conversations WHERE id NOT IN (SELECT conversation_id FROM responses)",
        [],
    )
    .map(|_| ())
    .map_err(internal)
}

fn abort(conn: &mut Connection, response_id: &str) -> Result<(), RelayError> {
    conn.execute(
        "DELETE FROM responses WHERE id = ?1 AND status = 'in_progress'",
        [response_id],
    )
    .map_err(internal)?;

    drop_empty_conversations(conn)
}

fn touch(conn: &Connection, conversation_id: &str, now: i64) -> Result<(), RelayError> {
    conn.execute(
        "UPDATE conversations SET last_used_at = ?2 WHERE id = ?1",
        params![conversation_id, now],
    )
    .map(|_| ())
    .map_err(internal)
}

fn load(conn: &Connection, response_id: &str) -> Result<Option<StoredResponse>, RelayError> {
    conn.query_row(
        "SELECT r.id, r.conversation_id, r.parent_id, r.status, s.json, r.input_items, r.output_items, r.usage,
                r.incomplete_reason, r.metadata, r.created_at
         FROM responses r JOIN settings s ON s.hash = r.settings_hash
         WHERE r.id = ?1",
        [response_id],
        |r| {
            Ok(StoredResponse {
                id: r.get(0)?,
                conversation_id: r.get(1)?,
                parent_id: r.get(2)?,
                status: Status::parse(&r.get::<_, String>(3)?),
                settings: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                input_items: parse_array(&r.get::<_, String>(5)?),
                output_items: parse_array(&r.get::<_, String>(6)?),
                usage: parse_optional(r.get(7)?),
                incomplete_reason: r.get(8)?,
                metadata: parse_optional(r.get(9)?),
                created_at_ms: r.get(10)?,
            })
        },
    )
    .optional()
    .map_err(internal)
}

/// Items from the root of the conversation down to and including `response_id`.
fn history(conn: &Connection, response_id: &str) -> Result<Vec<Value>, RelayError> {
    let mut statement = conn
        .prepare(
            "WITH RECURSIVE chain (id, parent_id, input_items, output_items, depth) AS (
                 SELECT id, parent_id, input_items, output_items, 0 FROM responses WHERE id = ?1
                 UNION ALL
                 SELECT r.id, r.parent_id, r.input_items, r.output_items, chain.depth + 1
                 FROM responses r JOIN chain ON r.id = chain.parent_id
             )
             SELECT input_items, output_items FROM chain ORDER BY depth DESC",
        )
        .map_err(internal)?;
    let rows = statement
        .query_map([response_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(internal)?;
    let mut items = Vec::new();

    for row in rows {
        let (input, output) = row.map_err(internal)?;

        items.extend(parse_array(&input));
        items.extend(parse_array(&output));
    }

    Ok(items)
}

const SUBTREE: &str = "WITH RECURSIVE subtree (id) AS (
         SELECT ?1
         UNION ALL
         SELECT r.id FROM responses r JOIN subtree ON r.parent_id = subtree.id
     )";

fn delete_response(conn: &mut Connection, response_id: &str) -> Result<(), RelayError> {
    let tx = conn.transaction().map_err(internal)?;
    let conversation_id: String = tx
        .query_row(
            "SELECT conversation_id FROM responses WHERE id = ?1",
            [response_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?
        .ok_or(RelayError::ResponseNotFound)?;
    let running: i64 = tx
        .query_row(
            &format!(
                "{SUBTREE} SELECT COUNT(*) FROM responses
                 WHERE status = 'in_progress' AND id IN (SELECT id FROM subtree)"
            ),
            [response_id],
            |r| r.get(0),
        )
        .map_err(internal)?;

    if running > 0 {
        return Err(RelayError::ResponseInProgress);
    }

    tx.execute(
        &format!("{SUBTREE} DELETE FROM responses WHERE id IN (SELECT id FROM subtree)"),
        [response_id],
    )
    .map_err(internal)?;
    tx.execute(
        "UPDATE conversations
         SET bytes = (SELECT COALESCE(SUM(bytes), 0) FROM responses WHERE conversation_id = ?1)
         WHERE id = ?1",
        [&conversation_id],
    )
    .map_err(internal)?;
    drop_empty_conversations(&tx)?;
    tx.commit().map_err(internal)
}

fn delete_conversation(conn: &mut Connection, conversation_id: &str) -> Result<(), RelayError> {
    let tx = conn.transaction().map_err(internal)?;
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM conversations WHERE id = ?1",
            [conversation_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)?;

    if exists.is_none() {
        return Err(RelayError::ResponseNotFound);
    }

    let running: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM responses WHERE conversation_id = ?1 AND status = 'in_progress'",
            [conversation_id],
            |r| r.get(0),
        )
        .map_err(internal)?;

    if running > 0 {
        return Err(RelayError::ResponseInProgress);
    }

    tx.execute("DELETE FROM conversations WHERE id = ?1", [conversation_id])
        .map_err(internal)?;
    tx.commit().map_err(internal)
}

const IDLE: &str = "id NOT IN (SELECT conversation_id FROM responses WHERE status = 'in_progress')";

fn sweep(conn: &mut Connection, now: i64, ttl_ms: i64, max_bytes: u64) -> Result<SweepStats, RelayError> {
    let tx = conn.transaction().map_err(internal)?;
    let expired = tx
        .execute(
            &format!("DELETE FROM conversations WHERE last_used_at < ?1 AND {IDLE}"),
            [now - ttl_ms],
        )
        .map_err(internal)?;
    let mut evicted = 0;

    loop {
        let total: i64 = tx
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM conversations", [], |r| {
                r.get(0)
            })
            .map_err(internal)?;

        if total as u64 <= max_bytes {
            break;
        }

        let removed = tx
            .execute(
                &format!(
                    "DELETE FROM conversations WHERE id =
                     (SELECT id FROM conversations WHERE {IDLE} ORDER BY last_used_at ASC, created_at ASC LIMIT 1)"
                ),
                [],
            )
            .map_err(internal)?;

        if removed == 0 {
            break;
        }

        evicted += removed;
    }

    tx.execute(
        "DELETE FROM settings WHERE hash NOT IN (SELECT settings_hash FROM responses)",
        [],
    )
    .map_err(internal)?;
    tx.commit().map_err(internal)?;

    Ok(SweepStats { expired, evicted })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const TARGET: &str = "https://api.example.com/v1";

    fn temp_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("trout-store-{}.db", Uuid::new_v4().simple()))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn settings(model: &str) -> Settings {
        json!({ "model": model, "tools": [{ "type": "function", "name": "FileRead" }] })
            .as_object()
            .unwrap()
            .clone()
    }

    fn user(text: &str) -> Value {
        json!({ "type": "message", "role": "user", "content": text })
    }

    fn assistant(text: &str) -> Value {
        json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
    }

    fn root(text: &str) -> NewTurn {
        NewTurn {
            target: TARGET.to_string(),
            overrides: settings("m1"),
            input_items: vec![user(text)],
            persist: true,
            ..NewTurn::default()
        }
    }

    fn follow(previous: &str, text: &str) -> NewTurn {
        NewTurn {
            target: TARGET.to_string(),
            previous_response_id: Some(previous.to_string()),
            input_items: vec![user(text)],
            persist: true,
            ..NewTurn::default()
        }
    }

    async fn complete(store: &Store, turn: NewTurn, answer: &str) -> Begun {
        let begun = store.begin(turn).await.unwrap();

        store
            .finalise(
                begun.response_id.clone(),
                Completion {
                    output_items: vec![assistant(answer)],
                    ..Completion::default()
                },
            )
            .await
            .unwrap();

        begun
    }

    fn texts(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .map(|item| match &item["content"] {
                Value::String(text) => text.clone(),
                parts => parts[0]["text"].as_str().unwrap().to_string(),
            })
            .collect()
    }

    #[tokio::test]
    async fn root_turn_requires_a_model() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let mut turn = root("hi");

        turn.overrides.remove("model");

        assert!(matches!(
            store.begin(turn).await,
            Err(RelayError::InvalidRequest(_))
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn history_is_rebuilt_in_order_and_settings_are_inherited() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let second = complete(&store, follow(&first.response_id, "q2"), "a2").await;
        let third = store.begin(follow(&second.response_id, "q3")).await.unwrap();

        assert!(first.history.is_empty());
        assert_eq!(texts(&second.history), ["q1", "a1"]);
        assert_eq!(texts(&third.history), ["q1", "a1", "q2", "a2"]);
        assert_eq!(third.settings, settings("m1"));
        assert_eq!(third.conversation_id, first.conversation_id);
        assert_eq!(
            third.previous_response_id.as_deref(),
            Some(second.response_id.as_str())
        );

        let (prefix_settings, prefix) = store.request_prefix(second.response_id.clone()).await.unwrap();

        assert_eq!(prefix_settings, settings("m1"));
        assert_eq!(texts(&prefix), ["q1", "a1", "q2", "a2"]);

        cleanup(&path);
    }

    #[tokio::test]
    async fn overrides_replace_persist_and_null_clears() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let mut change = follow(&first.response_id, "q2");

        change.overrides = json!({ "model": "m2", "tools": null, "temperature": 0.2 })
            .as_object()
            .unwrap()
            .clone();

        let second = complete(&store, change, "a2").await;
        let third = store.begin(follow(&second.response_id, "q3")).await.unwrap();
        let expected = json!({ "model": "m2", "temperature": 0.2 })
            .as_object()
            .unwrap()
            .clone();

        assert_eq!(second.settings, expected);
        assert_eq!(third.settings, expected);

        cleanup(&path);
    }

    #[tokio::test]
    async fn identical_settings_are_stored_once() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;

        complete(&store, follow(&first.response_id, "q2"), "a2").await;
        complete(&store, root("other"), "a").await;

        let count: i64 = store
            .run(|conn| {
                conn.query_row("SELECT COUNT(*) FROM settings", [], |r| r.get(0))
                    .map_err(internal)
            })
            .await
            .unwrap();

        assert_eq!(count, 1);

        cleanup(&path);
    }

    #[tokio::test]
    async fn siblings_form_separate_branches() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let left = complete(&store, follow(&first.response_id, "left"), "al").await;
        let right = complete(&store, follow(&first.response_id, "right"), "ar").await;
        let after_left = store.begin(follow(&left.response_id, "next")).await.unwrap();
        let after_right = store.begin(follow(&right.response_id, "next")).await.unwrap();

        assert_eq!(texts(&after_left.history), ["q1", "a1", "left", "al"]);
        assert_eq!(texts(&after_right.history), ["q1", "a1", "right", "ar"]);

        cleanup(&path);
    }

    #[tokio::test]
    async fn only_completed_responses_can_be_continued() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let running = store.begin(root("q1")).await.unwrap();

        assert!(matches!(
            store.begin(follow(&running.response_id, "q2")).await,
            Err(RelayError::ResponseInProgress)
        ));

        store
            .finalise(
                running.response_id.clone(),
                Completion {
                    status: Some(Status::Incomplete),
                    incomplete_reason: Some("max_output_tokens".to_string()),
                    ..Completion::default()
                },
            )
            .await
            .unwrap();

        assert!(matches!(
            store.begin(follow(&running.response_id, "q2")).await,
            Err(RelayError::ResponseNotContinuable)
        ));
        assert!(matches!(
            store.begin(follow("resp_missing", "q")).await,
            Err(RelayError::ResponseNotFound)
        ));

        let stored = store.get(running.response_id).await.unwrap();

        assert_eq!(stored.status, Status::Incomplete);
        assert_eq!(stored.incomplete_reason.as_deref(), Some("max_output_tokens"));

        cleanup(&path);
    }

    #[tokio::test]
    async fn other_target_cannot_continue() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let mut turn = follow(&first.response_id, "q2");

        turn.target = "https://other.example.com/v1".to_string();

        assert!(matches!(store.begin(turn).await, Err(RelayError::TargetMismatch)));

        cleanup(&path);
    }

    #[tokio::test]
    async fn tool_outputs_must_answer_open_calls() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let begun = store.begin(root("read it")).await.unwrap();
        let call =
            json!({ "type": "function_call", "call_id": "call_1", "name": "FileRead", "arguments": "{}" });

        store
            .finalise(
                begun.response_id.clone(),
                Completion {
                    output_items: vec![call],
                    ..Completion::default()
                },
            )
            .await
            .unwrap();

        let output = |id: &str| json!({ "type": "function_call_output", "call_id": id, "output": "data" });
        let mut orphan = follow(&begun.response_id, "");
        let mut paired = follow(&begun.response_id, "");

        orphan.input_items = vec![output("call_9")];
        paired.input_items = vec![output("call_1")];

        assert!(matches!(
            store.begin(orphan).await,
            Err(RelayError::InvalidRequest(_))
        ));

        let answered = complete(&store, paired, "done").await;
        let mut twice = follow(&answered.response_id, "");

        twice.input_items = vec![output("call_1")];

        assert!(matches!(
            store.begin(twice).await,
            Err(RelayError::InvalidRequest(_))
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn unstored_turn_writes_nothing_but_can_read_a_parent() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let mut turn = follow(&first.response_id, "q2");

        turn.persist = false;

        let begun = store.begin(turn).await.unwrap();

        assert_eq!(begun.conversation_id, None);
        assert_eq!(texts(&begun.history), ["q1", "a1"]);

        store
            .finalise(begun.response_id.clone(), Completion::default())
            .await
            .unwrap();

        assert!(matches!(
            store.get(begun.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn abort_removes_the_turn_and_an_empty_conversation() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let begun = store.begin(root("q1")).await.unwrap();

        store.abort(begun.response_id.clone()).await.unwrap();

        assert!(matches!(
            store.get(begun.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(matches!(
            store.delete_conversation(begun.conversation_id.unwrap()).await,
            Err(RelayError::ResponseNotFound)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn deleting_a_response_takes_its_descendants() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let second = complete(&store, follow(&first.response_id, "q2"), "a2").await;
        let third = complete(&store, follow(&second.response_id, "q3"), "a3").await;
        let sibling = complete(&store, follow(&first.response_id, "side"), "as").await;

        store.delete_response(second.response_id.clone()).await.unwrap();

        assert!(matches!(
            store.get(second.response_id.clone()).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(matches!(
            store.get(third.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(store.get(first.response_id.clone()).await.is_ok());
        assert!(store.get(sibling.response_id).await.is_ok());
        assert!(matches!(
            store.delete_response(second.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));

        store.delete_response(first.response_id).await.unwrap();

        assert!(matches!(
            store.delete_conversation(first.conversation_id.unwrap()).await,
            Err(RelayError::ResponseNotFound)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn running_turns_block_deletion() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let first = complete(&store, root("q1"), "a1").await;
        let running = store.begin(follow(&first.response_id, "q2")).await.unwrap();
        let conversation = first.conversation_id.clone().unwrap();

        assert!(matches!(
            store.delete_response(first.response_id.clone()).await,
            Err(RelayError::ResponseInProgress)
        ));
        assert!(matches!(
            store.delete_conversation(conversation.clone()).await,
            Err(RelayError::ResponseInProgress)
        ));

        store.abort(running.response_id).await.unwrap();
        store.delete_conversation(conversation).await.unwrap();

        assert!(matches!(
            store.get(first.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));

        cleanup(&path);
    }

    #[tokio::test]
    async fn sweep_expires_unused_and_evicts_least_recently_used() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let old = complete(&store, root("old"), "a").await;
        let mid = complete(&store, root("mid"), "a").await;
        let new = complete(&store, root("new"), "a").await;
        let running = store.begin(root("running")).await.unwrap();
        let stamp = |id: &Begun, at: i64| {
            let conversation = id.conversation_id.clone().unwrap();

            store.run(move |conn| touch(conn, &conversation, at))
        };

        stamp(&old, 1_000).await.unwrap();
        stamp(&mid, 5_000).await.unwrap();
        stamp(&new, 9_000).await.unwrap();
        stamp(&running, 500).await.unwrap();

        let stats = store
            .run(|conn| sweep(conn, 10_000, 8_000, u64::MAX))
            .await
            .unwrap();

        assert_eq!(
            stats,
            SweepStats {
                expired: 1,
                evicted: 0
            }
        );
        assert!(matches!(
            store.get(old.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));

        stamp(&mid, 5_000).await.unwrap();
        stamp(&new, 9_000).await.unwrap();

        let stats = store.run(|conn| sweep(conn, 10_000, 100_000, 1)).await.unwrap();

        assert_eq!(stats.evicted, 2);
        assert!(matches!(
            store.get(mid.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(matches!(
            store.get(new.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(store.get(running.response_id).await.is_ok());

        cleanup(&path);
    }

    #[tokio::test]
    async fn sweep_evicts_oldest_first_until_under_the_cap() {
        let path = temp_path();
        let store = Store::open(&path).unwrap();
        let old = complete(&store, root("old"), &"x".repeat(2_000)).await;
        let new = complete(&store, root("new"), &"x".repeat(2_000)).await;
        let old_conversation = old.conversation_id.clone().unwrap();

        store
            .run(move |conn| touch(conn, &old_conversation, 1))
            .await
            .unwrap();

        let stats = store
            .run(|conn| sweep(conn, now_ms(), i64::MAX / 2, 3_000))
            .await
            .unwrap();

        assert_eq!(
            stats,
            SweepStats {
                expired: 0,
                evicted: 1
            }
        );
        assert!(matches!(
            store.get(old.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(store.get(new.response_id).await.is_ok());

        let orphans: i64 = store
            .run(|conn| {
                conn.query_row("SELECT COUNT(*) FROM settings", [], |r| r.get(0))
                    .map_err(internal)
            })
            .await
            .unwrap();

        assert_eq!(orphans, 1);

        cleanup(&path);
    }

    #[tokio::test]
    async fn reopening_keeps_completed_turns_and_drops_running_ones() {
        let path = temp_path();
        let (first, running, lonely) = {
            let store = Store::open(&path).unwrap();
            let first = complete(&store, root("q1"), "a1").await;
            let running = store.begin(follow(&first.response_id, "q2")).await.unwrap();
            let lonely = store.begin(root("never finished")).await.unwrap();

            (first, running, lonely)
        };
        let store = Store::open(&path).unwrap();
        let next = store
            .begin(follow(&first.response_id, "after restart"))
            .await
            .unwrap();

        assert_eq!(texts(&next.history), ["q1", "a1"]);
        assert!(matches!(
            store.get(running.response_id).await,
            Err(RelayError::ResponseNotFound)
        ));
        assert!(matches!(
            store.delete_conversation(lonely.conversation_id.unwrap()).await,
            Err(RelayError::ResponseNotFound)
        ));

        cleanup(&path);
    }
}
