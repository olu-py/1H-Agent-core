//! Reproducible performance audit. No provider or external network is used.
use protium_core::{bridge::EventBridge, protocol::Event, provider::Role, storage::Storage};
use serde_json::{Value, json};
use std::{hint::black_box, path::PathBuf, time::Instant};

fn measure(name: &str, samples: usize, mut operation: impl FnMut()) -> Value {
    for _ in 0..3 {
        operation();
    }
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        operation();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let result = json!({"name": name, "samples": samples,
        "mean_ms": times.iter().sum::<f64>() / samples as f64,
        "median_ms": times[samples / 2], "p95_ms": times[(samples * 95 / 100).min(samples - 1)],
        "min_ms": times[0], "max_ms": times[samples - 1], "raw_ms": times});
    eprintln!("{name}: {} ms median", result["median_ms"]);
    result
}

fn main() -> anyhow::Result<()> {
    let output = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("design/performance/core.json"));
    std::fs::create_dir_all(output.parent().unwrap())?;
    let mut results = Vec::new();
    let fixture_only = std::env::args().any(|arg| arg == "--fixture-only");
    if !fixture_only {
        for count in [1_000usize, 10_000, 100_000] {
            for turns in [false, true] {
                let storage = Storage::in_memory()?;
                let session = storage.create_session(std::path::Path::new("benchmark"))?;
                let start = Instant::now();
                for i in 0..count {
                    let role = if turns && i % 2 == 0 {
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
                let shape = if turns {
                    "alternating"
                } else {
                    "assistant_only"
                };
                results.push(json!({"name": format!("seed/memory/{shape}/{count}"), "elapsed_ms": start.elapsed().as_secs_f64()*1000.0}));
                results.push(measure(&format!("page_latest/{shape}/{count}"), 30, || {
                    let page = storage.load_message_page(&session, None, 100).unwrap();
                    assert_eq!(page.len(), 100);
                    black_box(page);
                }));
                results.push(measure(&format!("page_oldest/{shape}/{count}"), 30, || {
                    let page = storage.load_message_page(&session, Some(101), 100).unwrap();
                    assert_eq!(page.len(), 100);
                    black_box(page);
                }));
                results.push(measure(
                    &format!("bounded_restore/{shape}/{count}"),
                    20,
                    || {
                        black_box(
                            storage
                                .load_messages_bounded(&session, 512, 1024 * 1024, 256 * 1024)
                                .unwrap(),
                        );
                    },
                ));
                results.push(measure(
                    &format!("full_restore/{shape}/{count}"),
                    10,
                    || {
                        let messages = storage.load_messages(&session).unwrap();
                        assert_eq!(messages.len(), count);
                        black_box(messages);
                    },
                ));
            }
        }
        for subscribers in [0, 1, 8, 64] {
            results.push(measure(
                &format!("bridge_1000_fanout/{subscribers}"),
                30,
                || {
                    let bridge = EventBridge::new(4096, 16 * 1024 * 1024);
                    let mut receivers = (0..subscribers)
                        .map(|_| bridge.subscribe())
                        .collect::<Vec<_>>();
                    for i in 0..1000 {
                        bridge.push(
                            "bench".into(),
                            Event::TextDelta {
                                delta: "abcdefgh".into(),
                            },
                        );
                        if i % 128 == 127 || i == 999 {
                            for receiver in &mut receivers {
                                while let Ok(event) = receiver.try_recv() {
                                    black_box(event);
                                }
                            }
                        }
                    }
                },
            ));
        }
    }
    // Public Storage API creates an isolated, on-disk corpus for HTTP/browser tests.
    // It is never opened by an adapter while the service is running.
    let fixture = output.parent().unwrap().join("fixture");
    std::fs::create_dir_all(fixture.join("workspace"))?;
    let workspace = fixture.join("workspace").canonicalize()?;
    let data_dir = fixture.join("data");
    std::fs::create_dir_all(&data_dir)?;
    let database = data_dir.join("agent.db");
    anyhow::ensure!(
        !database.exists(),
        "fixture already exists; choose a fresh output directory"
    );
    let storage = Storage::open(&database)?;
    let start = Instant::now();
    for i in 0..1000 {
        let sid = storage.create_session(&workspace)?;
        storage.rename_session(&sid, &format!("Benchmark session {i}"))?;
    }
    results.push(json!({"name":"disk_create_1000_sessions", "elapsed_ms":start.elapsed().as_secs_f64()*1000.0}));
    results.push(measure("disk_list_1000_sessions", 30, || {
        black_box(storage.list_sessions(&workspace).unwrap());
    }));
    let session = storage.create_session(&workspace)?;
    let start = Instant::now();
    for i in 0..10_000 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        storage.append_message(
            &session,
            role,
            "## Benchmark\n\nA **fixed** 中文 message with `inline code`.\n\n- one\n- two",
        )?;
    }
    results.push(json!({"name":"disk_append_10000_alternating", "elapsed_ms":start.elapsed().as_secs_f64()*1000.0}));
    results.push(measure("disk_page_100_from_10000", 30, || {
        black_box(storage.load_message_page(&session, None, 100).unwrap());
    }));
    std::fs::write(
        output.parent().unwrap().join("fixture.json"),
        serde_json::to_vec_pretty(&json!({
            "workspace":workspace, "data_dir":data_dir.canonicalize()?, "session_id":session,
            "messages":10000,"sessions":1001
        }))?,
    )?;
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&json!({"results":results}))?,
    )?;
    Ok(())
}
