use rules_core::{default_pack_status, TantivyRulesIndex};
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Deserialize)]
struct GoldenCase {
    q: String,
    expect: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crates/rules-core has a repository ancestor")
        .to_path_buf();
    let golden_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("01_docs/eval/golden.jsonl"));
    let rules_dir = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("04_data/90_index-build/rules"));

    let cases = load_golden(&golden_path)?;
    let index =
        TantivyRulesIndex::from_articles_dir(&rules_dir, default_pack_status("cni", "2026-02-27"))?;

    let mut hit_count = 0_usize;
    let mut pin_hit_count = 0_usize;
    let mut retrieval_hit_count = 0_usize;
    let mut latencies = Vec::with_capacity(cases.len());
    for case in &cases {
        let start = Instant::now();
        let report = index.search_with_routes(&case.q, 5, None);
        let elapsed = start.elapsed().as_micros();
        latencies.push(elapsed);

        let hit = report.hits.iter().any(|result| {
            case.expect
                .iter()
                .any(|expected| expected == &result.article_id)
        });
        let pin_hit = report.pin_hit.as_ref().is_some_and(|result| {
            case.expect
                .iter()
                .any(|expected| expected == &result.article_id)
        });
        let retrieval_hit = report.retrieval_hits.iter().any(|result| {
            case.expect
                .iter()
                .any(|expected| expected == &result.article_id)
        });
        if hit {
            hit_count += 1;
        }
        if pin_hit {
            pin_hit_count += 1;
        }
        if retrieval_hit {
            retrieval_hit_count += 1;
        }

        println!(
            "{}\t{}\t{}\t{}",
            if hit { "hit" } else { "miss" },
            if pin_hit { "pin-hit" } else { "pin-miss" },
            if retrieval_hit {
                "retrieval-hit"
            } else {
                "retrieval-miss"
            },
            report
                .hits
                .iter()
                .map(|hit| hit.article_id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        eprintln!("case_latency_us={elapsed}");
    }

    latencies.sort_unstable();
    let p95 = percentile(&latencies, 95);
    let hit_rate = if cases.is_empty() {
        0.0
    } else {
        hit_count as f64 * 100.0 / cases.len() as f64
    };
    println!(
        "summary cases={} hit@5={}/{} ({:.1}%)",
        cases.len(),
        hit_count,
        cases.len(),
        hit_rate
    );
    eprintln!("p95_us={p95}");
    println!(
        "routes pin_hit@5={}/{} retrieval_hit@5={}/{}",
        pin_hit_count,
        cases.len(),
        retrieval_hit_count,
        cases.len()
    );
    Ok(())
}

fn load_golden(path: &PathBuf) -> anyhow::Result<Vec<GoldenCase>> {
    let reader = BufReader::new(File::open(path)?);
    let mut cases = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        cases.push(serde_json::from_str(&line)?);
    }
    Ok(cases)
}

fn percentile(sorted: &[u128], percentile: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() * percentile).div_ceil(100)).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}
