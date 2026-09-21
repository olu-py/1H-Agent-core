use std::path::PathBuf;

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use protium_core::{bridge::EventBridge, protocol::Event, provider::Role, storage::Storage};
use tempfile::TempDir;

fn text_event(index: usize) -> Event {
    Event::TextDelta {
        delta: format!("event-{index}"),
    }
}

fn bench_event_bridge(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_bridge");
    for count in [1_000usize, 10_000] {
        group.bench_function(format!("publish_{count}"), |b| {
            b.iter_batched(
                || EventBridge::new(4_096, 16 * 1024 * 1024),
                |bridge| {
                    for index in 0..count {
                        black_box(bridge.push("bench-session".to_owned(), text_event(index)));
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.bench_function("replay_1k", |b| {
        b.iter_batched(
            || {
                let bridge = EventBridge::new(4_096, 16 * 1024 * 1024);
                for index in 0..1_000 {
                    bridge.push("bench-session".to_owned(), text_event(index));
                }
                bridge
            },
            |bridge| black_box(bridge.replay_after(0)),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("eviction_replay_10k", |b| {
        b.iter_batched(
            || {
                let bridge = EventBridge::new(4_096, 16 * 1024 * 1024);
                for index in 0..10_000 {
                    bridge.push("bench-session".to_owned(), text_event(index));
                }
                bridge
            },
            |bridge| black_box(bridge.replay_after(0)),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn seeded_storage(message_count: usize) -> (TempDir, Storage, String) {
    let temp = tempfile::tempdir().expect("benchmark tempdir");
    let path = PathBuf::from(temp.path()).join("bench.sqlite");
    let storage = Storage::open(&path).expect("open benchmark storage");
    let session = storage
        .create_session(temp.path())
        .expect("create benchmark session");
    for index in 0..message_count {
        storage
            .append_message(
                &session,
                Role::Assistant,
                &format!("message-{index}: fixed benchmark payload"),
            )
            .expect("append benchmark message");
    }
    (temp, storage, session)
}

fn bench_storage(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage");
    group.bench_function("append_10k_messages", |b| {
        b.iter_batched(
            || seeded_storage(0),
            |(_temp, storage, session)| {
                for index in 0..10_000 {
                    black_box(storage.append_message(
                        &session,
                        Role::Assistant,
                        &format!("message-{index}: fixed benchmark payload"),
                    ))
                    .expect("append benchmark message");
                }
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("page_100_from_10k", |b| {
        b.iter_batched(
            || seeded_storage(10_000),
            |(_temp, storage, session)| {
                black_box(storage.load_message_page(&session, None, 100))
                    .expect("load benchmark page");
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("restore_10k_messages", |b| {
        b.iter_batched(
            || seeded_storage(10_000),
            |(_temp, storage, session)| {
                black_box(storage.load_messages(&session)).expect("restore benchmark messages");
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(maintainability, bench_event_bridge, bench_storage);
criterion_main!(maintainability);
