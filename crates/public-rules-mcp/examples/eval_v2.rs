//! 평가 하니스 v2 (PH-01).
//!
//! 서버 코드 경로를 그대로 탄다: TOML -> `PublicRulesServer::from_config` -> `search_rules`.
//! 지표는 `rules_core::eval_metrics`. 출력 `<out>/{run.json, items.jsonl, summary.json}`.
//! `items.jsonl`에는 질의문(q)과 rels를 기록하지 않는다.

use anyhow::{bail, Context, Result};
use public_rules_mcp::{
    GetAnnexParams, GetArticleParams, PublicRulesServer, SearchRulesParams, ServerConfig,
};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rules_core::eval_metrics as em;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;

// rules-core의 캐시 키 상수와 같은 값(비공개 상수라 run.json 기록용으로 복제).
const MODEL_ID: &str = "multilingual-e5-small";
const MODEL_REVISION: &str = "fastembed-5:EmbeddingModel::MultilingualE5Small";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetFormat {
    V2,
    Legacy,
}

#[derive(Debug)]
struct Args {
    config: PathBuf,
    set: PathBuf,
    out: PathBuf,
    k: usize,
    format: SetFormat,
    legacy_scope: String,
    repeat: usize,
    warmup: usize,
    label: Option<String>,
}

#[derive(Debug, Clone)]
struct Item {
    qid: String,
    q: String,
    typ: String,
    scope: String,
    /// v2: 셋의 rels(접두어 ID). legacy: 검색 후 채운다.
    rels: BTreeMap<String, u8>,
    /// legacy 전용: 원본 expect 목록.
    expect: Vec<String>,
    oid: String,
}

#[derive(Debug, Clone)]
struct HitRec {
    /// rels와 같은 접두어 ID(서버가 접두어 없이 돌려주면 보정).
    id: String,
    /// 서버가 돌려준 원래 ID.
    raw_id: String,
    institution: String,
    score: f32,
    kind: String,
}

#[derive(Debug, Clone)]
struct PassItem {
    hits: Vec<HitRec>,
    latency_us: u128,
}

fn usage() -> &'static str {
    "usage: eval_v2 --config <toml> --set <jsonl> --out <dir> [--k 20] [--format v2|legacy] \
     [--legacy-scope <slug>|all] [--repeat N] [--warmup 5] [--label <text>]"
}

fn parse_args() -> Result<Args> {
    let mut config = None;
    let mut set = None;
    let mut out = None;
    let mut k = 20_usize;
    let mut format = SetFormat::V2;
    let mut legacy_scope = "all".to_string();
    let mut repeat = 1_usize;
    let mut warmup = 5_usize;
    let mut label = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| -> Result<String> {
            it.next()
                .with_context(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(val("--config")?)),
            "--set" => set = Some(PathBuf::from(val("--set")?)),
            "--out" => out = Some(PathBuf::from(val("--out")?)),
            "--k" => k = val("--k")?.parse()?,
            "--format" => {
                format = match val("--format")?.as_str() {
                    "v2" => SetFormat::V2,
                    "legacy" => SetFormat::Legacy,
                    other => bail!("--format must be v2|legacy, got {other}"),
                }
            }
            "--legacy-scope" => legacy_scope = val("--legacy-scope")?,
            "--repeat" => repeat = val("--repeat")?.parse()?,
            "--warmup" => warmup = val("--warmup")?.parse()?,
            "--label" => label = Some(val("--label")?),
            "--help" | "-h" => {
                eprintln!("{}", usage());
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}\n{}", usage()),
        }
    }
    if k == 0 || repeat == 0 {
        bail!("--k and --repeat must be >= 1");
    }
    Ok(Args {
        config: config.context("--config is required")?,
        set: set.context("--set is required")?,
        out: out.context("--out is required")?,
        k,
        format,
        legacy_scope,
        repeat,
        warmup,
        label,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

fn oid_of(q: &str) -> String {
    let nfc: String = q.nfc().collect();
    sha256_hex(nfc.as_bytes())[..12].to_string()
}

/// 셋 파서. 알 수 없는 키(`kws`, `src.label_slot` 등)는 무시한다.
fn load_set(path: &Path, format: SetFormat, legacy_scope: &str) -> Result<Vec<Item>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut items = Vec::new();
    let mut seen_qid = BTreeSet::new();
    for (lineno, line) in text.lines().enumerate() {
        let lineno = lineno + 1;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .with_context(|| format!("set line {lineno}: invalid JSON"))?;
        let q = v
            .get("q")
            .and_then(Value::as_str)
            .with_context(|| format!("set line {lineno}: q missing"))?
            .to_string();
        if q.trim().is_empty() {
            bail!("set line {lineno}: empty q");
        }
        let item = match format {
            SetFormat::V2 => {
                let qid = v
                    .get("qid")
                    .and_then(Value::as_str)
                    .with_context(|| format!("set line {lineno}: qid missing"))?
                    .to_string();
                let typ = v
                    .get("type")
                    .and_then(Value::as_str)
                    .with_context(|| format!("set line {lineno}: type missing"))?
                    .to_string();
                let scope = v
                    .get("institution_scope")
                    .and_then(Value::as_str)
                    .with_context(|| format!("set line {lineno}: institution_scope missing"))?
                    .to_string();
                let mut rels = BTreeMap::new();
                let rels_obj = v
                    .get("rels")
                    .and_then(Value::as_object)
                    .with_context(|| format!("set line {lineno}: rels missing"))?;
                for (id, grade) in rels_obj {
                    let g = grade
                        .as_u64()
                        .filter(|g| *g <= 2)
                        .with_context(|| format!("set line {lineno}: rel grade must be 0..=2"))?;
                    rels.insert(id.clone(), g as u8);
                }
                Item {
                    oid: oid_of(&q),
                    qid,
                    q,
                    typ,
                    scope,
                    rels,
                    expect: Vec::new(),
                }
            }
            SetFormat::Legacy => {
                let expect = v
                    .get("expect")
                    .and_then(Value::as_array)
                    .with_context(|| format!("set line {lineno}: expect missing"))?
                    .iter()
                    .map(|e| {
                        e.as_str()
                            .map(ToString::to_string)
                            .with_context(|| format!("set line {lineno}: expect must be strings"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Item {
                    oid: oid_of(&q),
                    qid: format!("L{:04}", items.len() + 1),
                    q,
                    typ: "legacy".to_string(),
                    scope: legacy_scope.to_string(),
                    rels: BTreeMap::new(),
                    expect,
                }
            }
        };
        if !seen_qid.insert(item.qid.clone()) {
            bail!("set line {lineno}: duplicate qid {}", item.qid);
        }
        items.push(item);
    }
    if items.is_empty() {
        bail!("set is empty");
    }
    Ok(items)
}

/// 유형 규칙만: mp_named/mp_topic 또는 scope=all -> 전체 팩, 그 외 -> 해당 기관.
fn institution_for(item: &Item) -> Option<String> {
    if item.typ == "mp_named" || item.typ == "mp_topic" || item.scope == "all" {
        None
    } else {
        Some(item.scope.clone())
    }
}

async fn search_once(server: &PublicRulesServer, item: &Item, k: usize) -> PassItem {
    let params = SearchRulesParams {
        query: item.q.clone(),
        top_k: Some(k),
        rule: None,
        institution: institution_for(item),
    };
    let started = Instant::now();
    let Json(result) = server.search_rules(Parameters(params)).await;
    let latency_us = started.elapsed().as_micros();
    let hits = result
        .hits
        .into_iter()
        .map(|hit| HitRec {
            id: em::canonical_id(&hit.institution, &hit.article_id),
            raw_id: hit.article_id,
            institution: hit.institution,
            score: hit.score,
            kind: hit.kind,
        })
        .collect();
    PassItem { hits, latency_us }
}

/// ev(`rules-core/examples/eval.rs`)의 `expected_matches_hit`와 같은 규칙.
fn legacy_matches(expected: &str, hit: &HitRec) -> bool {
    if expected == hit.raw_id {
        return true;
    }
    let local = hit
        .raw_id
        .split_once('/')
        .map_or(hit.raw_id.as_str(), |(_, id)| id);
    match expected.split_once('/') {
        Some((institution, local_id)) => institution == hit.institution && local_id == local,
        None => expected == local,
    }
}

/// legacy: expect 각각에 대해 매칭되는 모든 결과를 등급 2로, 매칭이 없으면 추정 접두어 ID를 등급 2로.
fn legacy_rels(item: &Item, hits: &[HitRec]) -> BTreeMap<String, u8> {
    let mut rels = BTreeMap::new();
    for expected in &item.expect {
        let matched: Vec<&HitRec> = hits
            .iter()
            .filter(|h| legacy_matches(expected, h))
            .collect();
        if matched.is_empty() {
            let guess = if expected.contains('/') || item.scope == "all" {
                expected.clone()
            } else {
                format!("{}/{}", item.scope, expected)
            };
            rels.insert(guess, 2);
        } else {
            for hit in matched {
                rels.insert(hit.id.clone(), 2);
            }
        }
    }
    rels
}

fn opt(v: Option<f64>) -> Value {
    v.and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

fn num(v: f64) -> Value {
    opt(Some(v))
}

fn metrics(item: &Item, rels: &BTreeMap<String, u8>, hits: &[HitRec]) -> Map<String, Value> {
    let mut m = Map::new();
    let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
    let insts: Vec<String> = hits.iter().map(|h| h.institution.clone()).collect();
    let has_rel = rels.values().any(|g| *g >= 1);
    let keys = [
        "ndcg10", "recall5", "recall20", "mrr10", "hit5", "p5", "misattr5",
    ];
    if !has_rel {
        for key in keys {
            m.insert(key.to_string(), Value::Null);
        }
        return m;
    }
    m.insert("ndcg10".into(), opt(em::ndcg_at(&ids, rels, 10)));
    m.insert("recall5".into(), opt(em::recall_at(&ids, rels, 5)));
    m.insert("recall20".into(), opt(em::recall_at(&ids, rels, 20)));
    m.insert("mrr10".into(), num(em::mrr_at(&ids, rels, 10)));
    m.insert(
        "hit5".into(),
        num(if em::hit_at(&ids, rels, 5, 2) {
            1.0
        } else {
            0.0
        }),
    );
    m.insert("p5".into(), num(em::precision_at(&ids, rels, 5)));
    m.insert(
        "misattr5".into(),
        if item.typ == "mp_named" {
            opt(em::misattr_at(&insts, &item.scope, 5))
        } else {
            Value::Null
        },
    );
    m
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() * p).div_ceil(100)).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn latency_stats(values: &[u128]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    json!({"n": sorted.len(), "p50_us": percentile(&sorted, 50), "p95_us": percentile(&sorted, 95)})
}

fn mean_of(rows: &[&Map<String, Value>], key: &str) -> Value {
    let vals: Vec<f64> = rows.iter().filter_map(|m| m.get(key)?.as_f64()).collect();
    if vals.is_empty() {
        return Value::Null;
    }
    let mut sum = 0.0;
    for v in &vals {
        sum += v;
    }
    num(sum / vals.len() as f64)
}

fn mean_block(rows: &[&Map<String, Value>]) -> Value {
    let mut out = Map::new();
    out.insert("n".into(), json!(rows.len()));
    for key in [
        "ndcg10", "recall5", "recall20", "mrr10", "hit5", "p5", "misattr5",
    ] {
        out.insert(key.into(), mean_of(rows, key));
    }
    Value::Object(out)
}

fn cmd_out(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn walk_files(root: &Path, skip: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if skip(&path) {
            continue;
        }
        if path.is_dir() {
            walk_files(&path, skip, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// 파일이면 파일 sha, 디렉터리면 (상대경로, 파일 sha) 목록의 sha.
fn tree_sha(root: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<String> {
    if root.is_file() {
        return sha256_file(root);
    }
    let mut files = Vec::new();
    walk_files(root, skip, &mut files);
    let mut hasher = Sha256::new();
    for file in files {
        let rel = file.strip_prefix(root).unwrap_or(&file);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(sha256_file(&file)?.as_bytes());
        hasher.update([b'\n']);
    }
    Ok(hex(&hasher.finalize()))
}

fn skip_hidden_and_blobs(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') || n == "blobs")
}

fn run_dirty(workspace: &Path) -> bool {
    // 코드·설정 경로만 본다(문서 등 기존 미커밋 파일은 제외).
    cmd_out(
        "git",
        &[
            "-C",
            &workspace.to_string_lossy(),
            "status",
            "--porcelain",
            "--",
            "crates",
            "Cargo.toml",
            "Cargo.lock",
        ],
    )
    .is_none_or(|s| !s.is_empty())
}

fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/public-rules-mcp has a workspace ancestor")
        .to_path_buf();

    let mut items = load_set(&args.set, args.format, &args.legacy_scope)?;
    let set_sha256 = sha256_file(&args.set)?;
    let config_text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read {}", args.config.display()))?;
    let config: ServerConfig = toml::from_str(&config_text).context("parse config toml")?;
    let started_at = cmd_out("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_default();

    // 팩·모델 지문
    let mut pack_sha = Map::new();
    let mut pack_paths = vec![(config.institution.clone(), config.pack.path.clone())];
    for extra in &config.extra_packs {
        pack_paths.push((extra.institution.clone(), extra.pack.path.clone()));
    }
    for (inst, path) in pack_paths {
        if let Some(path) = path {
            pack_sha.insert(inst, json!(tree_sha(&path, &|_| false)?));
        }
    }
    let model_dir = config
        .vectors
        .model_dir
        .clone()
        .or_else(|| std::env::var_os("CNI_RULES_FASTEMBED_MODEL_DIR").map(PathBuf::from));
    let model_files_sha = if config.vectors.enabled {
        match &model_dir {
            Some(dir) => Some(tree_sha(dir, &skip_hidden_and_blobs)?),
            None => None,
        }
    } else {
        None
    };

    let load_started = Instant::now();
    let server = PublicRulesServer::from_config(config.clone())?;
    let cold_load_ms = load_started.elapsed().as_millis();
    let Json(status) = server.status().await;

    // 워밍업(측정·기록 제외)
    for idx in 0..args.warmup.min(items.len() * 4) {
        let item = &items[idx % items.len()];
        let _ = search_once(&server, item, args.k).await;
    }

    // 본 실행. 첫 패스가 결과, 나머지는 결정성 비교용.
    let mut passes: Vec<Vec<PassItem>> = Vec::new();
    for _ in 0..args.repeat {
        let mut pass = Vec::with_capacity(items.len());
        for item in &items {
            pass.push(search_once(&server, item, args.k).await);
        }
        passes.push(pass);
    }
    let mut diff_items = 0_usize;
    if passes.len() > 1 {
        let sig = |p: &PassItem| -> Vec<(String, u32)> {
            p.hits
                .iter()
                .map(|h| (h.id.clone(), h.score.to_bits()))
                .collect()
        };
        for idx in 0..items.len() {
            let first = sig(&passes[0][idx]);
            if passes[1..].iter().any(|pass| sig(&pass[idx]) != first) {
                diff_items += 1;
            }
        }
    }
    let determinism = if passes.len() < 2 {
        json!("not_run")
    } else if diff_items == 0 {
        json!("identical")
    } else {
        json!(format!("DIFF({diff_items})"))
    };
    let first = &passes[0];

    // 알 수 없는 rels ID(v2만)
    let mut unknown_ids = 0_usize;
    if args.format == SetFormat::V2 {
        let all_ids: BTreeSet<&String> = items.iter().flat_map(|i| i.rels.keys()).collect();
        for id in all_ids {
            let Json(art) = server
                .get_article(Parameters(GetArticleParams { id: id.clone() }))
                .await;
            if art.article.is_some() {
                continue;
            }
            let Json(annex) = server
                .get_annex(Parameters(GetAnnexParams { id: id.clone() }))
                .await;
            if annex.annex.is_none() {
                unknown_ids += 1;
            }
        }
    }

    // items.jsonl / summary
    std::fs::create_dir_all(&args.out)?;
    let mut lines = String::new();
    let mut all_m: Vec<Map<String, Value>> = Vec::new();
    let mut types: Vec<String> = Vec::new();
    let mut lat_single = Vec::new();
    let mut lat_all = Vec::new();
    let mut lat_overall = Vec::new();
    let mut neg_top1: Vec<f64> = Vec::new();
    for (idx, item) in items.iter_mut().enumerate() {
        let pass = &first[idx];
        if args.format == SetFormat::Legacy {
            item.rels = legacy_rels(item, &pass.hits);
        }
        let m = metrics(item, &item.rels, &pass.hits);
        let pin_used = Value::Null; // 서버 API가 pin 경로를 노출하지 않는다(검색 로직 무변경 원칙).
        let ranked: Vec<Value> = pass
            .hits
            .iter()
            .map(|h| json!({"id": h.id, "score": num(f64::from(h.score)), "kind": h.kind}))
            .collect();
        let row = json!({
            "oid": item.oid, "type": item.typ, "scope": item.scope,
            "ranked": ranked, "pin_used": pin_used,
            "latency_us": pass.latency_us as u64, "m": Value::Object(m.clone()),
        });
        lines.push_str(&serde_json::to_string(&row)?);
        lines.push('\n');
        if institution_for(item).is_some() {
            lat_single.push(pass.latency_us);
        } else {
            lat_all.push(pass.latency_us);
        }
        lat_overall.push(pass.latency_us);
        if item.typ == "neg" {
            if let Some(h) = pass.hits.first() {
                neg_top1.push(f64::from(h.score));
            }
        }
        if !types.contains(&item.typ) {
            types.push(item.typ.clone());
        }
        all_m.push(m);
    }
    std::fs::write(args.out.join("items.jsonl"), lines)?;

    let scored: Vec<&Map<String, Value>> = items
        .iter()
        .zip(&all_m)
        .filter(|(i, _)| i.typ != "neg")
        .map(|(_, m)| m)
        .collect();
    let mut per_type = Map::new();
    types.sort();
    for typ in &types {
        let rows: Vec<&Map<String, Value>> = items
            .iter()
            .zip(&all_m)
            .filter(|(i, _)| &i.typ == typ)
            .map(|(_, m)| m)
            .collect();
        per_type.insert(typ.clone(), mean_block(&rows));
    }
    neg_top1.sort_by(f64::total_cmp);
    let neg_block = if neg_top1.is_empty() {
        Value::Null
    } else {
        let p50 = neg_top1[((neg_top1.len() * 50).div_ceil(100)).saturating_sub(1)];
        json!({"n": neg_top1.len(), "min": neg_top1[0], "p50": p50, "max": neg_top1[neg_top1.len() - 1]})
    };
    let summary = json!({
        "n_items": items.len(),
        "overall": mean_block(&scored),
        "per_type": per_type,
        "latency": {
            "overall": latency_stats(&lat_overall),
            "single": latency_stats(&lat_single),
            "all": latency_stats(&lat_all),
        },
        "neg_top1_score": neg_block,
    });
    std::fs::write(
        args.out.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;

    // run.json
    let commit = cmd_out(
        "git",
        &["-C", &workspace.to_string_lossy(), "rev-parse", "HEAD"],
    )
    .unwrap_or_else(|| "unknown".to_string());
    let commit7: String = commit.chars().take(7).collect();
    let cfg_label = args.label.clone().unwrap_or_else(|| {
        args.config
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "cfg".to_string())
    });
    let stamp = cmd_out("date", &["+%y%m%d-%H%M"]).unwrap_or_else(|| "000000-0000".to_string());
    let run_id = format!("{stamp}-{}-{commit7}", sanitize(&cfg_label));
    let max_rss = cmd_out("ps", &["-o", "rss=", "-p", &std::process::id().to_string()])
        .and_then(|s| s.parse::<u64>().ok())
        .map(|kb| kb * 1024);
    let cores = std::thread::available_parallelism().map(|n| n.get()).ok();
    let host = json!({
        "hostname": cmd_out("hostname", &[]),
        "cpu": cmd_out("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "cores": cores,
        "mem_bytes": cmd_out("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse::<u64>().ok()),
    });
    let ended_at = cmd_out("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_default();
    let run = json!({
        "run_id": run_id,
        "label": args.label,
        "commit": commit,
        "dirty": run_dirty(&workspace),
        "config_sha256": sha256_hex(config_text.as_bytes()),
        "config": config_text,
        "pack_sha256": pack_sha,
        "model": {
            "id": MODEL_ID,
            "revision": MODEL_REVISION,
            "files_sha256": model_files_sha,
        },
        "vectors_enabled": status.vectors.enabled,
        "vectors_model_ready": status.vectors.model_ready,
        "tokenizer_degraded": Value::Null,
        "host": host,
        "set_sha256": set_sha256,
        "format": if args.format == SetFormat::V2 { "v2" } else { "legacy" },
        "k": args.k,
        "warmup": args.warmup,
        "repeat": args.repeat,
        "n_items": items.len(),
        "started_at": started_at,
        "ended_at": ended_at,
        "cold_load_ms": cold_load_ms as u64,
        "max_rss_bytes": max_rss,
        "determinism": determinism,
        "unknown_ids_count": unknown_ids,
    });
    std::fs::write(
        args.out.join("run.json"),
        serde_json::to_string_pretty(&run)?,
    )?;
    println!(
        "run_id={run_id} n={} k={} determinism={} out={}",
        items.len(),
        args.k,
        run["determinism"],
        args.out.display()
    );
    Ok(())
}
