//! Rust Storage cold/warm cursor pagination and mutation benchmark.
//! The database is isolated under a temporary directory; raw samples are JSON.
use anyhow::Result;
use protium_core::{provider::Role, storage::Storage};
use rusqlite::{Connection, params};
use serde_json::json;
use std::{
    hint::black_box,
    path::{Path, PathBuf},
    time::Instant,
};

const MESSAGE_COUNT: usize = 100_000;
const PAIRS: usize = MESSAGE_COUNT / 2;
const ROUNDS: usize = 3;
// Three exclusive rounds of 34 give 102 samples for every cold and hot class.
const SAMPLES_PER_ROUND: usize = 34;
const PAGE_SIZE: usize = 100;

fn seed_database(path: &Path) -> Result<(String, f64)> {
    let started = Instant::now();
    let storage = Storage::open(path)?;
    let session_id = storage.create_session(Path::new("visible-history-benchmark"))?;
    drop(storage);

    let mut connection = Connection::open(path)?;
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
    let payload = "Benchmark payload: fixed text for a realistic indexed row lookup.";
    let mut parent = root_turn;
    for pair in 0..PAIRS {
        let turn = format!("benchmark-turn-{pair:05}");
        turn_insert.execute(params![turn, session_id, parent])?;
        message_insert.execute(params![session_id, "user", payload, turn])?;
        message_insert.execute(params![session_id, "assistant", payload, turn])?;
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
    Ok((session_id, started.elapsed().as_secs_f64() * 1000.0))
}

fn page_before(class: &str) -> Option<i64> {
    match class {
        "latest" => None,
        "middle" => Some((MESSAGE_COUNT as i64 / 2) + 1),
        "oldest" => Some(101),
        _ => unreachable!(),
    }
}

fn sample_pages(path: &Path, session_id: &str) -> Result<serde_json::Value> {
    let mut cold = serde_json::Map::new();
    let mut warm = serde_json::Map::new();
    let mut open_ms = Vec::with_capacity(ROUNDS * 3 * SAMPLES_PER_ROUND);
    for class in ["latest", "middle", "oldest"] {
        let mut class_cold = Vec::with_capacity(ROUNDS * SAMPLES_PER_ROUND);
        let mut class_warm = Vec::with_capacity(ROUNDS * SAMPLES_PER_ROUND);
        for round in 0..ROUNDS {
            eprintln!("sampling {class}, round {}/{}", round + 1, ROUNDS);
            for _ in 0..SAMPLES_PER_ROUND {
                let open_started = Instant::now();
                let storage = Storage::open(path)?;
                open_ms.push(open_started.elapsed().as_secs_f64() * 1000.0);
                let query_started = Instant::now();
                let page = storage.load_message_page(session_id, page_before(class), PAGE_SIZE)?;
                class_cold.push(query_started.elapsed().as_secs_f64() * 1000.0);
                anyhow::ensure!(page.len() == PAGE_SIZE, "short {class} page");
                drop(storage);
            }

            // Keep this class's cursor fixed and collect independent hot-page
            // reads only after the corresponding cold samples are complete.
            let storage = Storage::open(path)?;
            for _ in 0..SAMPLES_PER_ROUND {
                let query_started = Instant::now();
                black_box(storage.load_message_page(session_id, page_before(class), PAGE_SIZE)?);
                class_warm.push(query_started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        cold.insert(class.to_owned(), json!(class_cold));
        warm.insert(class.to_owned(), json!(class_warm));
    }
    Ok(json!({"cold_query_ms": cold, "warm_query_ms": warm, "storage_open_ms": open_ms}))
}

fn sample_legacy_recursive_pages(path: &Path, session_id: &str) -> Result<serde_json::Value> {
    let mut legacy = serde_json::Map::new();
    for class in ["latest", "middle", "oldest"] {
        let mut samples = Vec::with_capacity(ROUNDS * SAMPLES_PER_ROUND);
        for round in 0..ROUNDS {
            for _ in 0..SAMPLES_PER_ROUND {
                let connection = Connection::open(path)?;
                let mut statement = connection.prepare(
                    "WITH RECURSIVE chain(id) AS (
                         SELECT head_turn_id FROM sessions WHERE id = ?1
                         UNION ALL
                         SELECT turns.parent_id FROM turns JOIN chain ON turns.id = chain.id
                         WHERE turns.parent_id IS NOT NULL
                     )
                     SELECT id, role, content, kind, metadata, created_at FROM messages
                     WHERE session_id = ?1 AND hidden = 0 AND partial = 0
                       AND (turn_id IN (SELECT id FROM chain) OR turn_id IS NULL)
                       AND (?2 IS NULL OR id < ?2)
                     ORDER BY id DESC LIMIT ?3",
                )?;
                let started = Instant::now();
                let mut rows =
                    statement.query(params![session_id, page_before(class), PAGE_SIZE as i64])?;
                let mut count = 0;
                while let Some(row) = rows.next()? {
                    black_box((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                    ));
                    count += 1;
                }
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                anyhow::ensure!(count == PAGE_SIZE, "short legacy {class} page");
                drop(rows);
                drop(statement);
                drop(connection);
            }
            eprintln!("legacy recursive {class}, round {}/{}", round + 1, ROUNDS);
        }
        legacy.insert(class.to_owned(), json!(samples));
    }
    Ok(json!(legacy))
}

fn migration_samples(source: &Path, root: &Path) -> Result<Vec<f64>> {
    let target = root.join("migration-replay.db");
    std::fs::copy(source, &target)?;
    {
        let connection = Connection::open(&target)?;
        // Construct the pre-v9 state without measuring the work needed to
        // disable the current version's triggers as part of migration setup.
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             DROP TRIGGER IF EXISTS messages_visible_history_insert;
             DROP TRIGGER IF EXISTS messages_visible_history_update;
             DROP TRIGGER IF EXISTS messages_visible_history_delete;
             DROP TRIGGER IF EXISTS turns_visible_history_path;
             DELETE FROM schema_migrations WHERE version = 9;
             UPDATE turns SET on_active_path = 0;
             DELETE FROM visible_history;
             COMMIT;",
        )?;
    }
    let started = Instant::now();
    let storage = Storage::open(&target)?;
    let migration_open_ms = started.elapsed().as_secs_f64() * 1000.0;
    drop(storage);
    Ok(vec![migration_open_ms])
}

fn mutation_samples(path: &Path, session_id: &str) -> Result<serde_json::Value> {
    let storage = Storage::open(path)?;
    let (mut append, mut undo, mut redo) = (Vec::new(), Vec::new(), Vec::new());
    for index in 0..SAMPLES_PER_ROUND * ROUNDS {
        let started = Instant::now();
        storage.append_message(session_id, Role::User, &format!("write-cost-{index}"))?;
        append.push(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        anyhow::ensure!(storage.undo(session_id)?, "undo failed");
        undo.push(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        anyhow::ensure!(storage.redo(session_id)?, "redo failed");
        redo.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(json!({
        "append_call_ms": append,
        "undo_call_ms": undo,
        "redo_call_ms": redo,
        "measurement_note": "Uncontended public call wall time; upper bound for connection mutex hold duration."
    }))
}

fn database_sizes(path: &Path) -> Result<serde_json::Value> {
    let connection = Connection::open(path)?;
    let checkpoint: (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    drop(connection);
    let wal_path = PathBuf::from(format!("{}-wal", path.display()));
    Ok(json!({
        "database_bytes": std::fs::metadata(path)?.len(),
        "wal_bytes_after_passive_checkpoint": std::fs::metadata(wal_path).map(|m| m.len()).unwrap_or(0),
        "passive_checkpoint_result": checkpoint
    }))
}

fn current_working_set_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        #[repr(C)]
        struct ProcessMemoryCounters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }
        #[link(name = "psapi")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut c_void;
            fn GetProcessMemoryInfo(
                process: *mut c_void,
                counters: *mut ProcessMemoryCounters,
                size: u32,
            ) -> i32;
        }
        let mut counters = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        // SAFETY: both calls use the current-process pseudo handle and a valid
        // PROCESS_MEMORY_COUNTERS buffer with its exact size.
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<ProcessMemoryCounters>() as u32,
            )
        };
        (ok != 0).then_some(counters.working_set_size as u64)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn main() -> Result<()> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "design/performance/m2-visible-history-rust.json".into());
    let root = tempfile::tempdir()?;
    let database = root.path().join("visible-history.db");
    let (session_id, seed_ms) = seed_database(&database)?;
    let connection = Connection::open(&database)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(connection);

    let paging = sample_pages(&database, &session_id)?;
    let legacy_recursive_pages = sample_legacy_recursive_pages(&database, &session_id)?;
    let migration_open_ms = migration_samples(&database, root.path())?;
    let mutations = mutation_samples(&database, &session_id)?;
    let database_size = database_sizes(&database)?;
    let result = json!({
        "benchmark": "protium-core Storage visible_history pagination",
        "messages": MESSAGE_COUNT,
        "turns_including_root": PAIRS + 1,
        "page_size": PAGE_SIZE,
        "rounds": ROUNDS,
        "samples_per_round_per_page_class": SAMPLES_PER_ROUND,
        "samples_total_per_page_class": ROUNDS * SAMPLES_PER_ROUND,
        "classes": {"latest": "before=None", "middle": "before=50001", "oldest": "before=101"},
        "seed_build_ms": seed_ms,
        "cold_and_hot_pages": paging,
        "legacy_recursive_query_ms": legacy_recursive_pages,
        "migration_storage_open_ms": migration_open_ms,
        "migration_sample_count": migration_open_ms.len(),
        "mutations": mutations,
        "database_size": database_size,
        "process_working_set_bytes_after_run": current_working_set_bytes(),
        "cache_conditions": {
            "application_cache": "cold page samples use a newly opened Storage for each query; hot samples repeat the same class on one Storage",
            "os_file_cache": "not flushed; repeated runs use normal OS file caching",
            "synchronous": "FULL",
            "isolation": "temporary database, no concurrent benchmark process"
        }
    });
    if let Some(parent) = Path::new(&output).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    Ok(())
}
