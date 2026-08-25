//! Minimal consumer of the generic core interface.
//!
//! Every frontend adapter (TUI, WebUI, Desktop) drives the core through the
//! same surface demonstrated here: start the service from a [`CoreConfig`],
//! take a snapshot, subscribe atomically from the snapshot's event cursor,
//! consume `Envelope` events, and shut the core down cleanly. No business
//! logic lives in the consumer - the core owns sessions, tools, approvals,
//! and persistence.
//!
//! Run with:
//!
//! ```text
//! cargo run --example minimal -- /path/to/a/workspace
//! ```

use std::{path::PathBuf, time::Duration};

use protium_core::{
    config::Config,
    protocol::{DEFAULT_PAGE_SIZE, Event},
    service::{AppService, CoreConfig},
};

/// The wire tag of an event (`#[serde(tag = "type")]`), e.g. `text_delta`.
fn event_tag(event: &Event) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|tag| tag.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The workspace is the only directory the agent's file tools may touch.
    let workspace = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize()?;
    let mut config = Config::load(None, &workspace)?;

    // Keep the demo side-effect-free: an isolated data dir under the demo
    // workspace instead of the shared default (`AGENT_DATA_DIR` / local data
    // dir), so the example never touches a real `agent.db`.
    config.data_dir = workspace.join(".protium-example-data");

    println!("workspace: {}", workspace.display());
    println!("data dir:  {}", config.data_dir.display());

    let data_dir = config.data_dir.clone();
    let event_capacity = config.server.event_buffer;
    let event_max_bytes = config.server.event_max_bytes;
    let approval_timeout = Duration::from_secs(config.server.approval_timeout_seconds);
    let handle = AppService::start(CoreConfig {
        workspace,
        config,
        data_dir,
        event_capacity,
        event_max_bytes,
        approval_timeout,
        message_page_size: DEFAULT_PAGE_SIZE,
    })
    .await?;

    // Startup sequence every consumer must follow: snapshot FIRST, then an
    // atomic `subscribe_from(snapshot.event_cursor)` - the replay/live overlap
    // is deduplicated by cursor, and `ResyncRequired` means refetch the
    // snapshot and subscribe again from the fresh cursor.
    let snapshot = handle.snapshot().await?;
    println!(
        "snapshot: {} session(s), active={:?}, provider={}, model={}, mode={}",
        snapshot.sessions.len(),
        snapshot.active_session,
        snapshot.provider,
        snapshot.model,
        snapshot.mode
    );

    let subscription = handle
        .subscribe_from(snapshot.event_cursor)
        .map_err(|_| anyhow::anyhow!("event cursor evicted: resync required"))?;
    for envelope in &subscription.replay {
        println!(
            "replay cursor={} session={} {}",
            envelope.cursor,
            envelope.session_id,
            event_tag(&envelope.event)
        );
    }

    // Drain live events for a few seconds, then exit. A real consumer keeps
    // consuming forever and resyncs on lag or `ResyncRequired` instead.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut live = subscription.live;
    while let Some(envelope) = tokio::time::timeout_at(deadline, live.recv())
        .await
        .ok()
        .and_then(|received| received.ok())
    {
        println!(
            "live   cursor={} session={} {}",
            envelope.cursor,
            envelope.session_id,
            event_tag(&envelope.event)
        );
    }

    handle.shutdown().await?;
    println!("core shut down cleanly");
    Ok(())
}
