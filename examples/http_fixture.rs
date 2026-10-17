//! Generate an isolated 100,000-message WebUI HTTP/SSE fixture.
use anyhow::Result;
use protium_core::storage::Storage;
use rusqlite::{Connection, params};
use serde_json::json;
use std::{env, fs, path::Path};

const MESSAGES: usize = 100_000;

fn main() -> Result<()> {
    let root = env::args()
        .nth(1)
        .unwrap_or_else(|| "design/performance/http-100k".into());
    let root = Path::new(&root);
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let data_dir = root.join("data");
    let workspace = root.join("workspace");
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(&workspace)?;
    let data_dir = data_dir.canonicalize()?;
    let workspace = workspace.canonicalize()?;
    let database = data_dir.join("agent.db");

    let storage = Storage::open(&database)?;
    let session_id = storage.create_session(&workspace)?;
    drop(storage);

    let mut connection = Connection::open(&database)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let root_turn: String = tx.query_row(
        "SELECT head_turn_id FROM sessions WHERE id = ?1",
        [&session_id],
        |row| row.get(0),
    )?;
    let mut turn_insert = tx.prepare(
        "INSERT INTO turns(id, session_id, parent_id, created_at, on_active_path)
         VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', 1)",
    )?;
    let mut message_insert = tx.prepare(
        "INSERT INTO messages(session_id, role, content, created_at, turn_id, kind, hidden, partial)
         VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', ?4, 'message', 0, 0)",
    )?;
    let mut parent = root_turn;
    for pair in 0..(MESSAGES / 2) {
        let turn = format!("http-bench-turn-{pair:05}");
        turn_insert.execute(params![turn, session_id, parent])?;
        message_insert.execute(params![
            session_id,
            "user",
            format!("HTTP benchmark prompt {pair}"),
            turn
        ])?;
        message_insert.execute(params![
            session_id,
            "assistant",
            format!("HTTP benchmark response {pair}"),
            turn
        ])?;
        parent = turn;
    }
    tx.execute(
        "UPDATE sessions SET head_turn_id = ?2 WHERE id = ?1",
        params![session_id, parent],
    )?;
    drop(message_insert);
    drop(turn_insert);
    tx.commit()?;
    drop(connection);

    let fixture = json!({
        "data_dir": data_dir.canonicalize()?.to_string_lossy(),
        "workspace": workspace.canonicalize()?.to_string_lossy(),
        "session_id": session_id,
        "sessions": 1,
        "messages": MESSAGES,
        "shape": "50,000 alternating user/assistant turns; synchronous=FULL"
    });
    fs::write(
        root.join("fixture.json"),
        serde_json::to_vec_pretty(&fixture)?,
    )?;
    println!("{}", serde_json::to_string(&fixture)?);
    Ok(())
}
