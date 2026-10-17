//! Provider decoder cost across network chunk sizes, with fixed frame content.
use bytes::Bytes;
use protium_core::provider::SseDecoder;
use serde_json::json;
use std::{hint::black_box, time::Instant};

fn main() -> anyhow::Result<()> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or("design/performance/provider.json".into());
    let frame = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"abcdefgh 中文 payload\"},\"finish_reason\":null}]}\n\n";
    let mut results = Vec::new();
    for count in [1000, 10000] {
        for size in [64, 4096, 65536, 1024 * 1024] {
            let bytes = Bytes::from(frame.repeat(count));
            let chunks = (0..bytes.len())
                .step_by(size)
                .map(|start| bytes.slice(start..(start + size).min(bytes.len())))
                .collect::<Vec<_>>();
            let mut times = Vec::new();
            for _ in 0..11 {
                let start = Instant::now();
                let mut decoder = SseDecoder::default();
                let mut decoded = 0;
                for chunk in &chunks {
                    let events = decoder
                        .try_push(chunk, 2 * 1024 * 1024, 4 * 1024 * 1024)
                        .unwrap();
                    decoded += events.len();
                    for event in events {
                        black_box(serde_json::from_str::<serde_json::Value>(&event.data).unwrap());
                    }
                }
                assert_eq!(decoded, count);
                times.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            times.remove(0);
            times.sort_by(f64::total_cmp);
            let result = json!({"frames":count,"chunk_bytes":size,"samples":times.len(),"median_ms":times[times.len()/2],"max_ms":times[times.len()-1],"raw_ms":times});
            eprintln!("{result}");
            results.push(result);
        }
    }
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&json!({"results":results}))?,
    )?;
    Ok(())
}
