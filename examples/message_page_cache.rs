//! Cold/warm message-page latency for a deep alternating-turn history.
use protium_core::{provider::Role, storage::Storage};
use serde_json::json;
use std::{hint::black_box, path::Path, time::Instant};

const MESSAGE_COUNT: usize = 100_000;
const SAMPLES: usize = 5;

fn seed() -> anyhow::Result<(Storage, String)> {
    let storage = Storage::in_memory()?;
    let session = storage.create_session(Path::new("message-page-benchmark"))?;
    for index in 0..MESSAGE_COUNT {
        let role = if index % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        storage.append_message(
            &session,
            role,
            "Benchmark **markdown** 中文 fixed 64-byte payload for read scaling.",
        )?;
    }
    Ok((storage, session))
}

fn main() -> anyhow::Result<()> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or("design/performance/message-page-cache.json".into());
    let mut cold_latest_ms = Vec::with_capacity(SAMPLES);
    let mut warm_latest_ms = Vec::new();
    let mut warm_older_ms = Vec::new();
    for _ in 0..SAMPLES {
        let (storage, session) = seed()?;
        let start = Instant::now();
        let latest = storage.load_message_page(&session, None, 100)?;
        cold_latest_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(latest.len(), 100);
        let before = Some(101);
        for _ in 0..30 {
            let start = Instant::now();
            black_box(storage.load_message_page(&session, None, 100)?);
            warm_latest_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            black_box(storage.load_message_page(&session, before, 100)?);
            warm_older_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    for samples in [&mut cold_latest_ms, &mut warm_latest_ms, &mut warm_older_ms] {
        samples.sort_by(f64::total_cmp);
    }
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&json!({
            "messages": MESSAGE_COUNT,
            "shape": "alternating_user_assistant",
            "cold_latest_ms": cold_latest_ms,
            "warm_latest_ms": warm_latest_ms,
            "warm_older_before_101_ms": warm_older_ms,
        }))?,
    )?;
    Ok(())
}
