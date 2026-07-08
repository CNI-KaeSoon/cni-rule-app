use rules_core::{
    default_pack_status, merge_search_route_reports, namespace_search_route_report, RuleFilter,
    SearchHit, SearchRouteReport, TantivyRulesIndex,
};
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

#[derive(Debug)]
struct EvalArgs {
    golden_path: PathBuf,
    rules_dir: PathBuf,
    packs: Vec<PackArg>,
    institution: String,
    per_question: bool,
}

#[derive(Debug)]
struct PackArg {
    institution: String,
    rules_dir: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let cases = load_golden(&args.golden_path)?;
    let multi_pack = args.packs.len() > 1;
    let indexes = load_indexes(&args)?;

    let mut hit_count = 0_usize;
    let mut pin_hit_count = 0_usize;
    let mut retrieval_hit_count = 0_usize;
    let mut latencies = Vec::with_capacity(cases.len());
    for (idx, case) in cases.iter().enumerate() {
        let start = Instant::now();
        let report = search_indexes(&indexes, &case.q, 5, multi_pack);
        let elapsed = start.elapsed().as_micros();
        latencies.push(elapsed);

        let hit = any_expected_hit(&case.expect, &report.hits);
        let pin_hit = report
            .pin_hit
            .as_ref()
            .is_some_and(|result| expected_matches_any(&case.expect, result));
        let retrieval_hit = any_expected_hit(&case.expect, &report.retrieval_hits);
        if hit {
            hit_count += 1;
        }
        if pin_hit {
            pin_hit_count += 1;
        }
        if retrieval_hit {
            retrieval_hit_count += 1;
        }

        print!(
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
        if args.per_question {
            print!("\t{}\t{}\t{}", idx + 1, case.q, case.expect.join(","));
        }
        println!();
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

fn parse_args() -> anyhow::Result<EvalArgs> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crates/rules-core has a repository ancestor")
        .to_path_buf();
    let mut golden_path = None;
    let mut rules_dir = None;
    let mut packs = Vec::new();
    let mut institution = "cni".to_string();
    let mut per_question = false;
    let mut positional = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--golden" => {
                golden_path =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--golden requires a path")
                    })?));
            }
            "--rules" => {
                rules_dir =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--rules requires a directory")
                    })?));
            }
            "--institution" => {
                institution = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--institution requires a slug"))?;
            }
            "--pack" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--pack requires <slug>=<rules-dir>"))?;
                packs.push(parse_pack_arg(&value)?);
            }
            "--per-question" => per_question = true,
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            _ if arg.starts_with("--") => {
                anyhow::bail!("unknown argument: {arg}");
            }
            _ => positional.push(PathBuf::from(arg)),
        }
    }

    if golden_path.is_none() {
        golden_path = positional.first().cloned();
    }
    if rules_dir.is_none() {
        rules_dir = positional.get(1).cloned();
    }

    Ok(EvalArgs {
        golden_path: golden_path.unwrap_or_else(|| workspace.join("01_docs/eval/golden.jsonl")),
        rules_dir: rules_dir.unwrap_or_else(|| workspace.join("04_data/90_index-build/rules")),
        packs,
        institution,
        per_question,
    })
}

fn parse_pack_arg(value: &str) -> anyhow::Result<PackArg> {
    let (institution, rules_dir) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--pack requires <slug>=<rules-dir>"))?;
    if institution.trim().is_empty() || rules_dir.trim().is_empty() {
        anyhow::bail!("--pack requires <slug>=<rules-dir>");
    }
    Ok(PackArg {
        institution: institution.to_string(),
        rules_dir: PathBuf::from(rules_dir),
    })
}

fn print_usage() {
    eprintln!(
        "usage: eval [--golden PATH] [--rules DIR] [--institution SLUG] [--per-question] [--pack SLUG=DIR ...]"
    );
    eprintln!("legacy positional usage is still supported: eval <golden.jsonl> <rules-dir>");
}

fn load_indexes(args: &EvalArgs) -> anyhow::Result<Vec<(String, TantivyRulesIndex)>> {
    let packs = if args.packs.is_empty() {
        vec![PackArg {
            institution: args.institution.clone(),
            rules_dir: args.rules_dir.clone(),
        }]
    } else {
        args.packs
            .iter()
            .map(|pack| PackArg {
                institution: pack.institution.clone(),
                rules_dir: pack.rules_dir.clone(),
            })
            .collect()
    };

    packs
        .into_iter()
        .map(|pack| {
            let index = TantivyRulesIndex::from_articles_dir(
                &pack.rules_dir,
                default_pack_status(&pack.institution, "eval"),
            )?;
            Ok((pack.institution, index))
        })
        .collect()
}

fn search_indexes(
    indexes: &[(String, TantivyRulesIndex)],
    query: &str,
    k: usize,
    multi_pack: bool,
) -> SearchRouteReport {
    let reports = indexes
        .iter()
        .map(|(institution, index)| {
            let report = index.search_with_routes(
                query,
                k,
                Some(RuleFilter {
                    institution: None,
                    ..RuleFilter::default()
                }),
            );
            namespace_search_route_report(report, institution, multi_pack)
        })
        .collect();
    merge_search_route_reports(reports, k)
}

fn any_expected_hit(expected: &[String], hits: &[SearchHit]) -> bool {
    hits.iter()
        .any(|result| expected_matches_any(expected, result))
}

fn expected_matches_any(expected: &[String], hit: &SearchHit) -> bool {
    expected
        .iter()
        .any(|expected_id| expected_matches_hit(expected_id, hit))
}

fn expected_matches_hit(expected_id: &str, hit: &SearchHit) -> bool {
    if expected_id == hit.article_id {
        return true;
    }
    let hit_local_id = hit
        .article_id
        .split_once('/')
        .map_or(hit.article_id.as_str(), |(_, id)| id);
    match expected_id.split_once('/') {
        Some((institution, local_id)) => institution == hit.institution && local_id == hit_local_id,
        None => expected_id == hit_local_id,
    }
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
