//! PH-03 rerank latency bench (no server integration, no quality evaluation).
//!
//! usage:
//!   rerank_bench --config <base_hybrid.toml> --model-dir <dir> --out <dir> \
//!       --queries <a.jsonl> [--queries <b.jsonl> ...] [--top-n 20] [--rounds 3] [--warmup 5]
//!
//! Candidates are the hybrid retrieval top-N (pin excluded); passage is
//! "{rule} {title}\n{body}". Only the reranker forward pass is timed.

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use rules_core::{
    merge_search_route_reports, namespace_search_route_report, RuleFilter, RulesIndex, SearchHit,
    TantivyRulesIndex, VectorSearchOptions,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const SINGLE_PACK: &str = "cni";
const DEFAULT_BATCH: usize = 256; // fastembed DEFAULT_BATCH_SIZE (None)
const LIMIT_SINGLE_MS: f64 = 1000.0;
const LIMIT_4PACK_MS: f64 = 2000.0;

#[derive(Deserialize)]
struct GoldenCase {
    q: String,
}

struct Args {
    config: PathBuf,
    model_dir: PathBuf,
    out: PathBuf,
    queries: Vec<PathBuf>,
    top_n: usize,
    rounds: usize,
    warmup: usize,
    label: String,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut a = Args {
        config: PathBuf::new(),
        model_dir: PathBuf::new(),
        out: PathBuf::new(),
        queries: Vec::new(),
        top_n: 20,
        rounds: 3,
        warmup: 5,
        label: "ph03".to_string(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} requires a value"))
        };
        match arg.as_str() {
            "--config" => a.config = val("--config")?.into(),
            "--model-dir" => a.model_dir = val("--model-dir")?.into(),
            "--out" => a.out = val("--out")?.into(),
            "--queries" => a.queries.push(val("--queries")?.into()),
            "--top-n" => a.top_n = val("--top-n")?.parse()?,
            "--rounds" => a.rounds = val("--rounds")?.parse()?,
            "--warmup" => a.warmup = val("--warmup")?.parse()?,
            "--label" => a.label = val("--label")?,
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    if a.config.as_os_str().is_empty()
        || a.model_dir.as_os_str().is_empty()
        || a.out.as_os_str().is_empty()
        || a.queries.is_empty()
    {
        anyhow::bail!("--config, --model-dir, --out, --queries are required");
    }
    Ok(a)
}

fn ps_field(field: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", field, "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn rss_kb() -> Option<u64> {
    ps_field("rss=")?.parse().ok()
}

/// `ps -o time=` -> seconds ("H:MM:SS.cc" or "MM:SS.cc").
fn cpu_seconds() -> Option<f64> {
    let s = ps_field("time=")?;
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p / 100.0).ceil() as usize;
    sorted[idx.saturating_sub(1).min(sorted.len() - 1)]
}

fn passage_of(index: &TantivyRulesIndex, hit: &SearchHit) -> String {
    if hit.kind == "annex" {
        if let Some(annex) = index.get_annex(&hit.article_id) {
            return format!("{} {}\n{}", annex.rule, hit.title, annex.body);
        }
    } else if let Some(article) = index.get_article(&hit.article_id) {
        return format!("{} {}\n{}", article.rule, article.title, article.body);
    }
    format!("{} {}\n{}", hit.rule, hit.title, hit.snippet)
}

struct Measure {
    scenario: &'static str,
    batch_label: String,
    round: usize,
    latencies_ms: Vec<f64>,
    cpu_s_per_query: Option<f64>,
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let cfg: toml::Value = std::fs::read_to_string(&args.config)?.parse()?;
    let vectors = cfg.get("vectors");
    let vopts = VectorSearchOptions {
        enabled: vectors
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        cache_dir: vectors
            .and_then(|v| v.get("cache_dir"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
        model_dir: vectors
            .and_then(|v| v.get("model_dir"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
        rrf_k: vectors
            .and_then(|v| v.get("rrf_k"))
            .and_then(|v| v.as_integer())
            .map(|v| v as usize)
            .unwrap_or(60),
        vector_weight: vectors
            .and_then(|v| v.get("vector_weight"))
            .and_then(|v| v.as_float())
            .map(|v| v as f32)
            .unwrap_or(1.0),
    };

    let mut packs: Vec<(String, PathBuf)> = Vec::new();
    packs.push((
        cfg.get("institution")
            .and_then(|v| v.as_str())
            .unwrap_or(SINGLE_PACK)
            .to_string(),
        cfg["pack"]["path"].as_str().unwrap().into(),
    ));
    if let Some(extra) = cfg.get("extra_packs").and_then(|v| v.as_array()) {
        for e in extra {
            packs.push((
                e["institution"].as_str().unwrap().to_string(),
                e["pack"]["path"].as_str().unwrap().into(),
            ));
        }
    }

    let mut queries = Vec::new();
    for path in &args.queries {
        for line in std::fs::read_to_string(path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
            queries.push(serde_json::from_str::<GoldenCase>(line)?.q);
        }
    }
    eprintln!("queries={} packs={}", queries.len(), packs.len());

    let mut indexes: Vec<(String, TantivyRulesIndex)> = Vec::new();
    for (slug, path) in &packs {
        let idx = TantivyRulesIndex::from_pack_archive_with_vector_options(path, vopts.clone())?;
        indexes.push((slug.clone(), idx));
    }
    let vector_ready = indexes.iter().all(|(_, i)| i.vector_status().model_ready);
    eprintln!("vector_model_ready={vector_ready}");

    // Candidate preparation (not timed): single = cni pack; 4pack = merged retrieval.
    let n = args.top_n;
    let single_idx = indexes
        .iter()
        .find(|(s, _)| s == SINGLE_PACK)
        .ok_or_else(|| anyhow::anyhow!("single pack {SINGLE_PACK} missing"))?;
    let filter = || {
        Some(RuleFilter {
            institution: None,
            ..RuleFilter::default()
        })
    };
    let mut single_sets: Vec<(String, Vec<String>)> = Vec::new();
    let mut pack4_sets: Vec<(String, Vec<String>)> = Vec::new();
    for q in &queries {
        let rep = single_idx.1.search_with_routes(q, n + 1, filter());
        let docs: Vec<String> = rep
            .retrieval_hits
            .iter()
            .take(n)
            .map(|h| passage_of(&single_idx.1, h))
            .collect();
        single_sets.push((q.clone(), docs));

        let reports = indexes
            .iter()
            .map(|(slug, idx)| {
                namespace_search_route_report(
                    idx.search_with_routes(q, n + 1, filter()),
                    slug,
                    true,
                )
            })
            .collect();
        let merged = merge_search_route_reports(reports, n + 1);
        let docs: Vec<String> = merged
            .retrieval_hits
            .iter()
            .take(n)
            .map(|h| {
                let slug = h.institution.clone();
                let idx = indexes.iter().find(|(s, _)| *s == slug).map(|(_, i)| i);
                match idx {
                    Some(i) => {
                        let mut raw = h.clone();
                        raw.article_id = h
                            .article_id
                            .strip_prefix(&format!("{slug}/"))
                            .unwrap_or(&h.article_id)
                            .to_string();
                        passage_of(i, &raw)
                    }
                    None => format!("{} {}\n{}", h.rule, h.title, h.snippet),
                }
            })
            .collect();
        pack4_sets.push((q.clone(), docs));
    }
    let avg_docs = |sets: &[(String, Vec<String>)]| {
        sets.iter().map(|(_, d)| d.len()).sum::<usize>() as f64 / sets.len().max(1) as f64
    };
    let avg_chars = |sets: &[(String, Vec<String>)]| {
        let (c, d) = sets.iter().fold((0usize, 0usize), |(c, d), (_, docs)| {
            (
                c + docs.iter().map(|x| x.chars().count()).sum::<usize>(),
                d + docs.len(),
            )
        });
        c as f64 / d.max(1) as f64
    };
    eprintln!(
        "single avg_docs={:.1} avg_chars={:.0}; 4pack avg_docs={:.1} avg_chars={:.0}",
        avg_docs(&single_sets),
        avg_chars(&single_sets),
        avg_docs(&pack4_sets),
        avg_chars(&pack4_sets)
    );

    // Model load.
    std::fs::create_dir_all(&args.model_dir)?;
    let rss_before = rss_kb();
    let load_start = Instant::now();
    let mut model = TextRerank::try_new(
        RerankInitOptions::new(RerankerModel::BGERerankerV2M3)
            .with_cache_dir(args.model_dir.clone())
            .with_show_download_progress(false),
    )?;
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;
    let rss_after_load = rss_kb();
    eprintln!(
        "model_load_ms={load_ms:.0} rss_before_kb={rss_before:?} rss_after_kb={rss_after_load:?}"
    );

    // Timed rounds.
    let batches: [(&str, Option<usize>); 2] = [("default", None), ("20", Some(20))];
    let mut measures: Vec<Measure> = Vec::new();
    for (scenario, sets) in [("single", &single_sets), ("4pack", &pack4_sets)] {
        for (blabel, bsize) in batches {
            for round in 1..=args.rounds {
                let mut lat = Vec::new();
                let cpu0 = cpu_seconds();
                for (i, (q, docs)) in sets.iter().enumerate() {
                    if docs.is_empty() {
                        continue;
                    }
                    let start = Instant::now();
                    let res = model.rerank(q.clone(), docs, false, bsize)?;
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    std::hint::black_box(&res);
                    if i >= args.warmup {
                        lat.push(ms);
                    }
                }
                let cpu1 = cpu_seconds();
                // CPU delta covers warmup + timed queries of the round.
                let cpu_per = match (cpu0, cpu1) {
                    (Some(a), Some(b)) if !sets.is_empty() => Some((b - a) / sets.len() as f64),
                    _ => None,
                };
                eprintln!(
                    "{scenario} batch={blabel} round={round} n={} p50={:.0}ms",
                    lat.len(),
                    {
                        let mut s = lat.clone();
                        s.sort_by(|a, b| a.total_cmp(b));
                        percentile(&s, 50.0)
                    }
                );
                measures.push(Measure {
                    scenario,
                    batch_label: blabel.to_string(),
                    round,
                    latencies_ms: lat,
                    cpu_s_per_query: cpu_per,
                });
            }
        }
    }
    let rss_end = rss_kb();

    // Aggregate.
    std::fs::create_dir_all(&args.out)?;
    let mut tsv =
        String::from("scenario\tbatch\tround\tn\tp50_ms\tp95_ms\tmax_ms\tcpu_s_per_query\n");
    let mut groups = Vec::new();
    for m in &measures {
        let mut s = m.latencies_ms.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{:.1}\t{}\n",
            m.scenario,
            m.batch_label,
            m.round,
            s.len(),
            percentile(&s, 50.0),
            percentile(&s, 95.0),
            s.last().copied().unwrap_or(0.0),
            m.cpu_s_per_query
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "NA".into())
        ));
    }
    let mut summary = serde_json::Map::new();
    for scenario in ["single", "4pack"] {
        for (blabel, _) in batches {
            let mut all: Vec<f64> = measures
                .iter()
                .filter(|m| m.scenario == scenario && m.batch_label == blabel)
                .flat_map(|m| m.latencies_ms.clone())
                .collect();
            all.sort_by(|a, b| a.total_cmp(b));
            let cpus: Vec<f64> = measures
                .iter()
                .filter(|m| m.scenario == scenario && m.batch_label == blabel)
                .filter_map(|m| m.cpu_s_per_query)
                .collect();
            let cpu_avg = if cpus.is_empty() {
                None
            } else {
                Some(cpus.iter().sum::<f64>() / cpus.len() as f64)
            };
            let entry = serde_json::json!({
                "n": all.len(),
                "p50_ms": percentile(&all, 50.0),
                "p95_ms": percentile(&all, 95.0),
                "max_ms": all.last().copied().unwrap_or(0.0),
                "cpu_s_per_query": cpu_avg,
            });
            groups.push((scenario, blabel, entry.clone()));
            summary.insert(format!("{scenario}/{blabel}"), entry);
        }
    }
    // Verdict basis: the worse (larger) p95 across batch variants per scenario.
    let worst = |scenario: &str| {
        groups
            .iter()
            .filter(|(s, _, _)| *s == scenario)
            .map(|(_, _, e)| e["p95_ms"].as_f64().unwrap_or(f64::MAX))
            .fold(0.0_f64, f64::max)
    };
    let p95_single = worst("single");
    let p95_4pack = worst("4pack");
    let enter = p95_single <= LIMIT_SINGLE_MS && p95_4pack <= LIMIT_4PACK_MS;
    let verdict = format!(
        "VERDICT: {} (p95_single={:.0}ms ≤{:.0}, p95_4pack={:.0}ms ≤{:.0})",
        if enter { "ENTER" } else { "HOLD" },
        p95_single,
        LIMIT_SINGLE_MS,
        p95_4pack,
        LIMIT_4PACK_MS
    );

    let host = |cmd: &str, a: &[&str]| {
        Command::new(cmd)
            .args(a)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    };
    let json = serde_json::json!({
        "label": args.label,
        "model": "rozgo/bge-reranker-v2-m3 (fastembed RerankerModel::BGERerankerV2M3)",
        "quantized_variant": "미지원(fastembed RerankerModel enum에 없음)",
        "config": args.config,
        "top_n": n,
        "rounds": args.rounds,
        "warmup_queries_per_round": args.warmup,
        "queries": queries.len(),
        "vector_model_ready": vector_ready,
        "cores": std::thread::available_parallelism().map(|v| v.get()).ok(),
        "host": {
            "hostname": host("hostname", &[]),
            "cpu": host("sysctl", &["-n", "machdep.cpu.brand_string"]),
            "memsize": host("sysctl", &["-n", "hw.memsize"]),
        },
        "model_load_ms": load_ms,
        "rss_kb": {"before_load": rss_before, "after_load": rss_after_load, "end": rss_end,
                    "load_increase": match (rss_before, rss_after_load) { (Some(a), Some(b)) => Some(b as i64 - a as i64), _ => None }},
        "candidates": {
            "single_avg_docs": avg_docs(&single_sets), "single_avg_chars": avg_chars(&single_sets),
            "4pack_avg_docs": avg_docs(&pack4_sets), "4pack_avg_chars": avg_chars(&pack4_sets),
        },
        "note_batch": "fastembed 기본 batch_size=256 이라 N=20에서는 batch 20과 동일 경로",
        "summary": summary,
        "p95_single_ms": p95_single,
        "p95_4pack_ms": p95_4pack,
        "verdict": if enter { "ENTER" } else { "HOLD" },
    });
    std::fs::write(args.out.join("bench.tsv"), tsv)?;
    std::fs::write(
        args.out.join("bench.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    let _ = DEFAULT_BATCH;
    println!("{verdict}");
    Ok(())
}
