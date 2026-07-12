use anyhow::{bail, Context, Result};
use rules_core::{parse_article_markdown, slugify_rule, Article, EdgeKind, NodeKind, PackManifest};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use tantivy::doc;
use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions, STORED, STRING};
use tantivy::Index;
use walkdir::WalkDir;

const INSTITUTION: &str = "cni";
const INSTITUTION_NAME: &str = "충남연구원";
const EFFECTIVE_DATE: &str = "2026-02-27";

#[derive(Debug)]
struct Args {
    rules_dir: PathBuf,
    output_dir: PathBuf,
    archive_path: PathBuf,
    institution: String,
    institution_name: String,
    effective_date: String,
    source_commit: String,
    created_at: String,
    golden_path: PathBuf,
    max_unresolved_refs: Option<usize>,
    source_url: Option<String>,
}

#[derive(Debug)]
struct SourceArticle {
    article: Article,
    source_path: PathBuf,
    relative_path: PathBuf,
    source_pages: Vec<u32>,
}

#[derive(Debug)]
struct SourceAnnex {
    id: String,
    institution: String,
    rule: String,
    annex: String,
    title: String,
    status: String,
    effective: String,
    source_path: PathBuf,
    relative_path: PathBuf,
    source_pages: Vec<u32>,
    table_structured: bool,
}

#[derive(Debug, serde::Deserialize)]
struct AnnexFrontmatter {
    #[serde(rename = "type")]
    node_type: Option<String>,
    institution: String,
    rule: String,
    annex: String,
    title: Option<String>,
    effective: Option<String>,
    status: Option<String>,
    #[serde(default)]
    source_pages: Vec<u32>,
    #[serde(default)]
    pages: Vec<u32>,
    #[serde(default)]
    table_structured: bool,
}

#[derive(Debug, Serialize)]
struct GraphNode {
    id: String,
    kind: NodeKind,
    label: String,
    meta: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct GraphEdge {
    src: String,
    dst: String,
    kind: EdgeKind,
    meta: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct QualityReport {
    schema_version: u32,
    institution: String,
    effective_date: String,
    article_count: usize,
    rule_count: usize,
    node_count: usize,
    edge_count: usize,
    ref_edge_count: usize,
    legal_basis_edge_count: usize,
    broken_edges: usize,
    orphans: usize,
    unresolved_refs: usize,
    unresolved_refs_before_annex: usize,
    unresolved_refs_after_annex: usize,
    external_ref_nodes: usize,
    annex_count: usize,
    annex_ref_edges: usize,
    annex_table_structured: usize,
    page_coverage: Option<serde_json::Value>,
    coverage: BTreeMap<&'static str, usize>,
}

#[derive(Debug, serde::Deserialize, Serialize)]
struct GoldenCase {
    q: String,
    expect: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    build_pack(&args)
}

fn build_pack(args: &Args) -> Result<()> {
    let mut articles = load_source_articles(&args.rules_dir)?;
    if articles.is_empty() {
        bail!(
            "no article markdown files found under {}",
            args.rules_dir.display()
        );
    }
    articles.sort_by(|a, b| a.article.id.cmp(&b.article.id));
    let mut annexes = load_source_annexes(&args.rules_dir)?;
    annexes.sort_by(|a, b| a.id.cmp(&b.id));

    let stage = args.output_dir.join(args.pack_name());
    if stage.exists() {
        move_existing_aside(&stage)?;
    }
    fs::create_dir_all(&stage)?;

    let article_ids = articles
        .iter()
        .map(|source| source.article.id.clone())
        .collect::<BTreeSet<_>>();
    let annex_ids = annexes
        .iter()
        .map(|source| source.id.clone())
        .collect::<BTreeSet<_>>();
    let document_ids = article_ids
        .union(&annex_ids)
        .cloned()
        .collect::<BTreeSet<_>>();
    let rule_ids = articles
        .iter()
        .map(|source| rule_node_id(&source.article.rule))
        .chain(annexes.iter().map(|source| rule_node_id(&source.rule)))
        .collect::<BTreeSet<_>>();

    copy_articles(&articles, &stage.join("articles"))?;
    copy_annexes(&annexes, &stage.join("annexes"))?;
    copy_pages_sidecar(&args.rules_dir, &stage)?;
    let (nodes, edges, unresolved_before, unresolved_after) = build_graph(
        args,
        &articles,
        &annexes,
        &article_ids,
        &document_ids,
        &rule_ids,
    );
    write_jsonl(&stage.join("graph/nodes.jsonl"), &nodes)?;
    write_jsonl(&stage.join("graph/edges.jsonl"), &edges)?;
    build_tantivy_index(&articles, &stage.join("tantivy"))?;
    File::create(stage.join("vectors.db"))?;

    let quality = quality_report(
        args,
        &articles,
        &annexes,
        &rule_ids,
        &nodes,
        &edges,
        unresolved_before,
        unresolved_after,
    );
    if quality.broken_edges != 0 || quality.orphans != 0 {
        bail!(
            "QA gate failed: broken_edges={} orphans={}",
            quality.broken_edges,
            quality.orphans
        );
    }
    if let Some(max_unresolved_refs) = args.max_unresolved_refs {
        if quality.unresolved_refs > max_unresolved_refs {
            bail!(
                "QA gate failed: unresolved_refs={} max_unresolved_refs={}",
                quality.unresolved_refs,
                max_unresolved_refs
            );
        }
    } else if quality.unresolved_refs != 0 {
        eprintln!(
            "QA warning: unresolved_refs={} (set --max-unresolved-refs to fail this gate)",
            quality.unresolved_refs
        );
    }
    write_json_pretty(&stage.join("quality/report.json"), &quality)?;
    write_json_pretty(
        &stage.join("sample_queries.json"),
        &load_sample_queries(&args.golden_path, 5)?,
    )?;

    let manifest = build_manifest(args, &stage)?;
    write_json_pretty(&stage.join("manifest.json"), &manifest)?;
    write_archive(&stage, &args.archive_path)?;

    println!(
        "built {} articles={} annexes={} rules={} nodes={} edges={} broken_edges={} orphans={} unresolved_refs_before={} unresolved_refs_after={}",
        args.archive_path.display(),
        quality.article_count,
        quality.annex_count,
        quality.rule_count,
        quality.node_count,
        quality.edge_count,
        quality.broken_edges,
        quality.orphans,
        quality.unresolved_refs_before_annex,
        quality.unresolved_refs_after_annex
    );
    Ok(())
}

impl Args {
    fn parse() -> Result<Self> {
        let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("pipeline/pack-builder has a project ancestor")
            .to_path_buf();
        let mut rules_dir = project_root.join("04_data/90_index-build/rules");
        let mut output_dir = project_root.join("04_data/90_index-build");
        let mut institution = INSTITUTION.to_string();
        let mut institution_name = INSTITUTION_NAME.to_string();
        let mut effective_date = EFFECTIVE_DATE.to_string();
        let mut archive_path = None;
        let mut source_commit = None;
        let mut created_at = None;
        let mut golden_path = project_root.join("01_docs/eval/golden.jsonl");
        let mut max_unresolved_refs = None;
        let mut source_url = None;

        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .with_context(|| format!("{flag} requires a value"))?;
            match flag.as_str() {
                "--rules-dir" => rules_dir = PathBuf::from(value),
                "--output-dir" => output_dir = PathBuf::from(value),
                "--archive" => archive_path = Some(PathBuf::from(value)),
                "--institution" => institution = value,
                "--institution-name" => institution_name = value,
                "--effective-date" => effective_date = value,
                "--source-commit" => source_commit = Some(value),
                "--created-at" => created_at = Some(value),
                "--golden" => golden_path = PathBuf::from(value),
                "--source-url" => source_url = Some(value),
                "--max-unresolved-refs" => {
                    max_unresolved_refs = Some(
                        value
                            .parse::<usize>()
                            .with_context(|| format!("invalid --max-unresolved-refs: {value}"))?,
                    )
                }
                _ => bail!("unknown argument: {flag}"),
            }
        }
        validate_slug(&institution)?;
        validate_effective_date(&effective_date)?;
        let archive_path = archive_path.unwrap_or_else(|| {
            output_dir.join(format!("pack-{institution}-{effective_date}.tar.zst"))
        });

        Ok(Self {
            rules_dir,
            output_dir,
            archive_path,
            institution,
            institution_name,
            effective_date,
            source_commit: source_commit.context("--source-commit is required")?,
            created_at: created_at.context("--created-at is required")?,
            golden_path,
            max_unresolved_refs,
            source_url,
        })
    }

    fn pack_name(&self) -> String {
        format!("pack-{}-{}", self.institution, self.effective_date)
    }
}

fn load_source_articles(rules_dir: &Path) -> Result<Vec<SourceArticle>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(rules_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        if markdown_type(entry.path())?.as_deref() == Some("annex") {
            continue;
        }
        let article = parse_article_markdown(entry.path())?;
        let relative_path = entry
            .path()
            .strip_prefix(rules_dir)
            .with_context(|| format!("strip {}", entry.path().display()))?
            .to_path_buf();
        let source_pages = read_source_pages(entry.path())?;
        out.push(SourceArticle {
            article,
            source_path: entry.path().to_path_buf(),
            relative_path,
            source_pages,
        });
    }
    Ok(out)
}

fn load_source_annexes(rules_dir: &Path) -> Result<Vec<SourceAnnex>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(rules_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        if markdown_type(entry.path())?.as_deref() != Some("annex") {
            continue;
        }
        let text = fs::read_to_string(entry.path())?;
        let (frontmatter, _body) = split_frontmatter(&text)?;
        let frontmatter: AnnexFrontmatter = serde_yaml::from_str(frontmatter)?;
        if frontmatter.node_type.as_deref() != Some("annex") {
            continue;
        }
        let relative_path = entry
            .path()
            .strip_prefix(rules_dir)
            .with_context(|| format!("strip {}", entry.path().display()))?
            .to_path_buf();
        let source_pages = if frontmatter.source_pages.is_empty() {
            frontmatter.pages
        } else {
            frontmatter.source_pages
        };
        out.push(SourceAnnex {
            id: format!("{}#{}", slugify_rule(&frontmatter.rule), frontmatter.annex),
            institution: frontmatter.institution,
            rule: frontmatter.rule,
            annex: frontmatter.annex,
            title: frontmatter.title.unwrap_or_default(),
            status: frontmatter.status.unwrap_or_else(|| "active".to_string()),
            effective: frontmatter
                .effective
                .unwrap_or_else(|| EFFECTIVE_DATE.to_string()),
            source_path: entry.path().to_path_buf(),
            relative_path,
            source_pages,
            table_structured: frontmatter.table_structured,
        });
    }
    Ok(out)
}

fn markdown_type(path: &Path) -> Result<Option<String>> {
    let text = fs::read_to_string(path)?;
    let (frontmatter, _) = split_frontmatter(&text)?;
    let value: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
    Ok(value
        .get("type")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string()))
}

fn validate_slug(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        bail!("--institution must contain only lowercase ASCII letters, digits, or hyphens");
    }
    Ok(())
}

fn validate_effective_date(value: &str) -> Result<()> {
    let valid = value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .chars()
            .enumerate()
            .all(|(idx, ch)| idx == 4 || idx == 7 || ch.is_ascii_digit());
    if !valid {
        bail!("--effective-date must use YYYY-MM-DD");
    }
    Ok(())
}

fn read_source_pages(path: &Path) -> Result<Vec<u32>> {
    let text = fs::read_to_string(path)?;
    let (frontmatter, _) = split_frontmatter(&text)?;
    let value: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
    let pages = value
        .get("source_pages")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_i64())
                .filter(|v| *v >= 0)
                .map(|v| v as u32)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(pages)
}

fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .context("missing YAML frontmatter")
}

fn copy_articles(articles: &[SourceArticle], articles_dir: &Path) -> Result<()> {
    for source in articles {
        let target = articles_dir.join(&source.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source.source_path, &target).with_context(|| {
            format!(
                "copy {} to {}",
                source.source_path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

fn copy_annexes(annexes: &[SourceAnnex], annexes_dir: &Path) -> Result<()> {
    for source in annexes {
        let target = annexes_dir.join(&source.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source.source_path, &target).with_context(|| {
            format!(
                "copy {} to {}",
                source.source_path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

fn copy_pages_sidecar(rules_dir: &Path, stage: &Path) -> Result<()> {
    let pages_dir = rules_dir
        .parent()
        .map(|parent| parent.join("pages"))
        .context("rules dir must have parent")?;
    if !pages_dir.is_dir() {
        return Ok(());
    }
    for entry in WalkDir::new(&pages_dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&pages_dir)?;
        let target = stage.join("pages").join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), target)?;
    }
    Ok(())
}

fn build_graph(
    args: &Args,
    articles: &[SourceArticle],
    annexes: &[SourceAnnex],
    article_ids: &BTreeSet<String>,
    document_ids: &BTreeSet<String>,
    rule_ids: &BTreeSet<String>,
) -> (Vec<GraphNode>, Vec<GraphEdge>, usize, usize) {
    let mut nodes = Vec::new();
    nodes.push(GraphNode {
        id: args.institution.clone(),
        kind: NodeKind::Institution,
        label: args.institution_name.clone(),
        meta: json!({ "institution": args.institution }),
    });

    let mut rule_labels = BTreeMap::<String, &str>::new();
    for source in articles {
        rule_labels.insert(
            rule_node_id(&source.article.rule),
            source.article.rule.as_str(),
        );
    }
    for source in annexes {
        rule_labels.insert(rule_node_id(&source.rule), source.rule.as_str());
    }
    for (id, label) in &rule_labels {
        nodes.push(GraphNode {
            id: id.clone(),
            kind: NodeKind::Rule,
            label: label.to_string(),
            meta: json!({ "slug": slugify_rule(label) }),
        });
    }

    for source in articles {
        nodes.push(GraphNode {
            id: source.article.id.clone(),
            kind: NodeKind::Article,
            label: source.article.title.clone(),
            meta: json!({
                "rule": source.article.rule,
                "article": source.article.article,
                "status": source.article.status,
                "effective": source.article.effective,
                "amended": source.article.amended,
                "source_pages": source.source_pages,
            }),
        });
    }
    for source in annexes {
        nodes.push(GraphNode {
            id: source.id.clone(),
            kind: NodeKind::Annex,
            label: if source.title.is_empty() {
                source.annex.clone()
            } else {
                format!("{} {}", source.annex, source.title)
            },
            meta: json!({
                "institution": source.institution,
                "rule": source.rule,
                "annex": source.annex,
                "status": source.status,
                "effective": source.effective,
                "source_pages": source.source_pages,
                "table_structured": source.table_structured,
            }),
        });
    }

    let mut external_nodes = BTreeMap::<String, (NodeKind, String, bool)>::new();
    let mut unresolved_before_annex = 0_usize;
    let mut unresolved_after_annex = 0_usize;
    for source in articles {
        for basis in &source.article.legal_basis {
            let id = law_node_id(&basis.law, &basis.article);
            external_nodes.insert(
                id,
                (
                    NodeKind::LawArticle,
                    format!("{} {}", basis.law, basis.article),
                    false,
                ),
            );
        }
        for article_ref in &source.article.refs {
            if !article_ids.contains(&article_ref.target) {
                unresolved_before_annex += 1;
            }
            if !document_ids.contains(&article_ref.target) {
                unresolved_after_annex += 1;
                external_nodes.insert(
                    article_ref.target.clone(),
                    (
                        if looks_like_law_article(&article_ref.target) {
                            NodeKind::LawArticle
                        } else {
                            NodeKind::Article
                        },
                        article_ref.target.clone(),
                        true,
                    ),
                );
            }
        }
    }
    for (id, (kind, label, unresolved_ref)) in &external_nodes {
        nodes.push(GraphNode {
            id: id.clone(),
            kind: *kind,
            label: label.clone(),
            meta: json!({ "unresolved_ref": unresolved_ref }),
        });
    }

    let mut edges = Vec::<GraphEdge>::new();
    for rule_id in rule_ids {
        edges.push(GraphEdge {
            src: args.institution.clone(),
            dst: rule_id.clone(),
            kind: EdgeKind::AppliesTo,
            meta: json!({ "source": "frontmatter.rule" }),
        });
    }
    for source in articles {
        edges.push(GraphEdge {
            src: rule_node_id(&source.article.rule),
            dst: source.article.id.clone(),
            kind: EdgeKind::AppliesTo,
            meta: json!({ "source": "frontmatter.article" }),
        });
    }
    for source in annexes {
        edges.push(GraphEdge {
            src: rule_node_id(&source.rule),
            dst: source.id.clone(),
            kind: EdgeKind::AppliesTo,
            meta: json!({ "source": "frontmatter.annex" }),
        });
    }
    for source in articles {
        for article_ref in &source.article.refs {
            edges.push(GraphEdge {
                src: source.article.id.clone(),
                dst: article_ref.target.clone(),
                kind: edge_kind_from_ref(&article_ref.kind),
                meta: json!({ "source": "frontmatter.refs", "type": article_ref.kind }),
            });
        }
        for basis in &source.article.legal_basis {
            edges.push(GraphEdge {
                src: source.article.id.clone(),
                dst: law_node_id(&basis.law, &basis.article),
                kind: EdgeKind::LegalBasis,
                meta: json!({ "source": "frontmatter.legal_basis", "mst": basis.mst }),
            });
        }
    }
    edges.sort_by_key(edge_sort_key);
    edges.dedup_by(|a, b| edge_sort_key(a) == edge_sort_key(b));

    (
        nodes,
        edges,
        unresolved_before_annex,
        unresolved_after_annex,
    )
}

fn quality_report(
    args: &Args,
    articles: &[SourceArticle],
    annexes: &[SourceAnnex],
    rule_ids: &BTreeSet<String>,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
    unresolved_refs_before_annex: usize,
    unresolved_refs_after_annex: usize,
) -> QualityReport {
    let node_ids = nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let mut degree = BTreeMap::<String, usize>::new();
    let mut broken_edges = 0_usize;
    let mut ref_edge_count = 0_usize;
    let mut legal_basis_edge_count = 0_usize;
    let mut annex_ref_edges = 0_usize;
    for edge in edges {
        if !node_ids.contains(&edge.src) || !node_ids.contains(&edge.dst) {
            broken_edges += 1;
        }
        *degree.entry(edge.src.clone()).or_default() += 1;
        *degree.entry(edge.dst.clone()).or_default() += 1;
        if matches!(
            edge.kind,
            EdgeKind::Cites | EdgeKind::ApplyMutatis | EdgeKind::Delegates | EdgeKind::ExceptWhen
        ) {
            ref_edge_count += 1;
            if edge.dst.contains("#별표") || edge.dst.contains("#별지제") {
                annex_ref_edges += 1;
            }
        }
        if edge.kind == EdgeKind::LegalBasis {
            legal_basis_edge_count += 1;
        }
    }
    let orphans = node_ids
        .iter()
        .filter(|id| degree.get(*id).copied().unwrap_or_default() == 0)
        .count();
    let active_articles = articles
        .iter()
        .filter(|source| source.article.status == "active")
        .count();
    let with_source_pages = articles
        .iter()
        .filter(|source| !source.source_pages.is_empty())
        .count();
    let with_refs = articles
        .iter()
        .filter(|source| !source.article.refs.is_empty())
        .count();
    let with_legal_basis = articles
        .iter()
        .filter(|source| !source.article.legal_basis.is_empty())
        .count();
    let page_coverage =
        read_pipeline_qa(&args.rules_dir).and_then(|qa| qa.get("coverage").cloned());

    QualityReport {
        schema_version: 1,
        institution: args.institution.clone(),
        effective_date: args.effective_date.clone(),
        article_count: articles.len(),
        rule_count: rule_ids.len(),
        node_count: nodes.len(),
        edge_count: edges.len(),
        ref_edge_count,
        legal_basis_edge_count,
        broken_edges,
        orphans,
        unresolved_refs: unresolved_refs_after_annex,
        unresolved_refs_before_annex,
        unresolved_refs_after_annex,
        external_ref_nodes: unresolved_refs_after_annex,
        annex_count: annexes.len(),
        annex_ref_edges,
        annex_table_structured: annexes
            .iter()
            .filter(|source| source.table_structured)
            .count(),
        page_coverage,
        coverage: BTreeMap::from([
            ("active_articles", active_articles),
            ("with_source_pages", with_source_pages),
            ("with_refs", with_refs),
            ("with_legal_basis", with_legal_basis),
        ]),
    }
}

fn build_tantivy_index(articles: &[SourceArticle], index_dir: &Path) -> Result<()> {
    fs::create_dir_all(index_dir)?;
    let mut schema_builder = Schema::builder();
    let ko_text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("ko")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let id = schema_builder.add_text_field("id", STRING | STORED);
    let rule = schema_builder.add_text_field("rule", ko_text.clone());
    let title = schema_builder.add_text_field("title", ko_text.clone());
    let effective = schema_builder.add_text_field("effective", STRING | STORED);
    let body = schema_builder.add_text_field("body", ko_text);
    let schema = schema_builder.build();
    let index = Index::create_in_dir(index_dir, schema)?;
    register_simple_ko_tokenizer(&index);

    let mut writer = index.writer(50_000_000)?;
    for source in articles {
        writer.add_document(doc!(
            id => source.article.id.clone(),
            rule => source.article.rule.clone(),
            title => source.article.title.clone(),
            effective => source.article.effective.clone(),
            body => source.article.body.clone(),
        ))?;
    }
    writer.commit()?;
    writer.wait_merging_threads()?;
    Ok(())
}

fn register_simple_ko_tokenizer(index: &Index) {
    use tantivy::tokenizer::TextAnalyzer;
    index.tokenizers().register(
        "ko",
        TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default()).build(),
    );
}

fn load_sample_queries(path: &Path, limit: usize) -> Result<Vec<GoldenCase>> {
    let reader = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line)?);
        if out.len() == limit {
            break;
        }
    }
    Ok(out)
}

fn read_pipeline_qa(rules_dir: &Path) -> Option<serde_json::Value> {
    let path = rules_dir.parent()?.join("qa.json");
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn build_manifest(args: &Args, root: &Path) -> Result<PackManifest> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root)?;
        let key = manifest_key(relative);
        if key == "manifest.json" {
            continue;
        }
        files.insert(key, sha256_file(entry.path())?);
    }
    Ok(PackManifest {
        schema_version: 1,
        institution: args.institution.clone(),
        effective_date: args.effective_date.clone(),
        source_commit: args.source_commit.clone(),
        created_at: args.created_at.clone(),
        source_url: args.source_url.clone(),
        quality: read_pipeline_qa(&args.rules_dir)
            .and_then(|qa| qa.get("coverage").cloned())
            .map(|coverage| json!({ "page_coverage": coverage })),
        files,
    })
}

fn write_archive(root: &Path, archive_path: &Path) -> Result<()> {
    if let Some(parent) = archive_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if archive_path.exists() {
        move_existing_aside(archive_path)?;
    }
    let file = File::create(archive_path)?;
    let encoder = zstd::stream::write::Encoder::new(file, 19)?;
    let mut tar = tar::Builder::new(encoder);
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_dir() {
            tar.append_dir(relative, path)?;
        } else if entry.file_type().is_file() {
            tar.append_path_with_name(path, relative)?;
        }
    }
    let encoder = tar.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn move_existing_aside(path: &Path) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("path must have a UTF-8 file name")?;
    let backup = path.with_file_name(format!("{file_name}.previous-{}", std::process::id()));
    if backup.exists() {
        bail!("backup path already exists: {}", backup.display());
    }
    fs::rename(path, &backup)
        .with_context(|| format!("move existing {} to {}", path.display(), backup.display()))?;
    Ok(())
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    for row in rows {
        serde_json::to_writer(&mut file, row)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 8192];
    loop {
        let len = file.read(&mut buf)?;
        if len == 0 {
            break;
        }
        hasher.update(&buf[..len]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn manifest_key(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn rule_node_id(rule: &str) -> String {
    format!("rule:{}", slugify_rule(rule))
}

fn law_node_id(law: &str, article: &str) -> String {
    format!("{law}#{article}")
}

fn looks_like_law_article(target: &str) -> bool {
    target.split('#').next().is_some_and(|name| {
        name.ends_with('법') || name.ends_with("시행령") || name.ends_with("조례")
    })
}

fn edge_kind_from_ref(kind: &str) -> EdgeKind {
    match kind {
        "준용" => EdgeKind::ApplyMutatis,
        "위임" => EdgeKind::Delegates,
        "단서예외" => EdgeKind::ExceptWhen,
        _ => EdgeKind::Cites,
    }
}

fn edge_sort_key(edge: &GraphEdge) -> (String, String, String) {
    (
        edge.src.clone(),
        edge.dst.clone(),
        format!("{:?}", edge.kind),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rules_core::ArticleRef;

    fn test_args() -> Args {
        Args {
            rules_dir: PathBuf::from("unused-rules"),
            output_dir: PathBuf::from("unused-output"),
            archive_path: PathBuf::from("unused.tar.zst"),
            institution: INSTITUTION.to_string(),
            institution_name: INSTITUTION_NAME.to_string(),
            effective_date: EFFECTIVE_DATE.to_string(),
            source_commit: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            golden_path: PathBuf::from("unused-golden.jsonl"),
            max_unresolved_refs: None,
            source_url: None,
        }
    }

    #[test]
    fn quality_report_counts_unresolved_refs_even_when_placeholder_nodes_hide_broken_edges() {
        let article = Article {
            id: "여비규정#제12조".to_string(),
            institution: "cni".to_string(),
            rule: "여비규정".to_string(),
            article: "제12조".to_string(),
            title: "교통비".to_string(),
            effective: "2026-02-27".to_string(),
            amended: "2026-02-27".to_string(),
            status: "active".to_string(),
            body: "교통비 지급 기준".to_string(),
            refs: vec![ArticleRef {
                target: "없는규정#제1조".to_string(),
                kind: "인용".to_string(),
            }],
            ..Article::default()
        };
        let source = SourceArticle {
            article,
            source_path: PathBuf::from("unused.md"),
            relative_path: PathBuf::from("여비규정/제12조.md"),
            source_pages: vec![1],
        };
        let articles = vec![source];
        let article_ids = articles
            .iter()
            .map(|source| source.article.id.clone())
            .collect::<BTreeSet<_>>();
        let annexes = Vec::<SourceAnnex>::new();
        let annex_ids = BTreeSet::<String>::new();
        let document_ids = article_ids
            .union(&annex_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        let rule_ids = articles
            .iter()
            .map(|source| rule_node_id(&source.article.rule))
            .collect::<BTreeSet<_>>();

        let args = test_args();
        let (nodes, edges, unresolved_before, unresolved_after) = build_graph(
            &args,
            &articles,
            &annexes,
            &article_ids,
            &document_ids,
            &rule_ids,
        );
        let report = quality_report(
            &args,
            &articles,
            &annexes,
            &rule_ids,
            &nodes,
            &edges,
            unresolved_before,
            unresolved_after,
        );

        assert_eq!(report.broken_edges, 0);
        assert_eq!(report.unresolved_refs, 1);
        assert_eq!(report.external_ref_nodes, 1);
    }

    #[test]
    fn annex_nodes_resolve_same_rule_annex_refs() {
        let article = Article {
            id: "여비규정#제12조".to_string(),
            institution: "cni".to_string(),
            rule: "여비규정".to_string(),
            article: "제12조".to_string(),
            title: "교통비".to_string(),
            effective: "2026-02-27".to_string(),
            amended: "2026-02-27".to_string(),
            status: "active".to_string(),
            body: "교통비는 별표 1에 따른다.".to_string(),
            refs: vec![ArticleRef {
                target: "여비규정#별표1".to_string(),
                kind: "인용".to_string(),
            }],
            ..Article::default()
        };
        let articles = vec![SourceArticle {
            article,
            source_path: PathBuf::from("unused.md"),
            relative_path: PathBuf::from("여비규정/제12조.md"),
            source_pages: vec![1],
        }];
        let annexes = vec![SourceAnnex {
            id: "여비규정#별표1".to_string(),
            institution: "cni".to_string(),
            rule: "여비규정".to_string(),
            annex: "별표1".to_string(),
            title: "여비 지급 기준".to_string(),
            status: "active".to_string(),
            effective: "2026-02-27".to_string(),
            source_path: PathBuf::from("unused-annex.md"),
            relative_path: PathBuf::from("여비규정/별표1.md"),
            source_pages: vec![2],
            table_structured: true,
        }];
        let article_ids = articles
            .iter()
            .map(|source| source.article.id.clone())
            .collect::<BTreeSet<_>>();
        let annex_ids = annexes
            .iter()
            .map(|source| source.id.clone())
            .collect::<BTreeSet<_>>();
        let document_ids = article_ids
            .union(&annex_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        let rule_ids = BTreeSet::from([rule_node_id("여비규정")]);
        let args = test_args();

        let (nodes, edges, unresolved_before, unresolved_after) = build_graph(
            &args,
            &articles,
            &annexes,
            &article_ids,
            &document_ids,
            &rule_ids,
        );
        let report = quality_report(
            &args,
            &articles,
            &annexes,
            &rule_ids,
            &nodes,
            &edges,
            unresolved_before,
            unresolved_after,
        );

        assert_eq!(report.unresolved_refs_before_annex, 1);
        assert_eq!(report.unresolved_refs_after_annex, 0);
        assert_eq!(report.annex_count, 1);
        assert_eq!(report.annex_ref_edges, 1);
        assert!(nodes
            .iter()
            .any(|node| node.id == "여비규정#별표1" && node.kind == NodeKind::Annex));
        assert!(edges
            .iter()
            .any(|edge| edge.src == "rule:여비규정" && edge.dst == "여비규정#별표1"));
    }

    #[test]
    fn annex_only_rules_get_rule_nodes() {
        let articles = Vec::<SourceArticle>::new();
        let annexes = vec![SourceAnnex {
            id: "서식규정#별지제1호".to_string(),
            institution: "cni".to_string(),
            rule: "서식규정".to_string(),
            annex: "별지제1호".to_string(),
            title: "신청서".to_string(),
            status: "active".to_string(),
            effective: "2026-02-27".to_string(),
            source_path: PathBuf::from("unused-annex.md"),
            relative_path: PathBuf::from("서식규정/별지제1호.md"),
            source_pages: vec![2],
            table_structured: false,
        }];
        let article_ids = BTreeSet::<String>::new();
        let document_ids = BTreeSet::from(["서식규정#별지제1호".to_string()]);
        let rule_ids = BTreeSet::from([rule_node_id("서식규정")]);
        let args = test_args();

        let (nodes, edges, _, _) = build_graph(
            &args,
            &articles,
            &annexes,
            &article_ids,
            &document_ids,
            &rule_ids,
        );

        assert!(nodes
            .iter()
            .any(|node| node.id == "rule:서식규정" && node.kind == NodeKind::Rule));
        assert!(edges
            .iter()
            .all(|edge| nodes.iter().any(|node| node.id == edge.src)
                && nodes.iter().any(|node| node.id == edge.dst)));
    }
}
