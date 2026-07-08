use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Json, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler, ServiceExt,
};
use rules_core::{
    default_pack_status, parse_article_markdown, prefixed_article_id, Annex, Article, GraphNode,
    LegalBasis, NodeKind, PackStatus, RuleFilter, RuleSummary, RulesIndex, SearchHit,
    SearchRouteReport, SourcePage, TantivyRulesIndex, VectorSearchOptions,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub const SEARCH_RULES_TOOL: &str = "search_rules";
pub const GET_ARTICLE_TOOL: &str = "get_article";
pub const LIST_RULES_TOOL: &str = "list_rules";
pub const GET_LEGAL_BASIS_TOOL: &str = "get_legal_basis";
pub const STATUS_TOOL: &str = "status";
pub const GET_ANNEX_TOOL: &str = "get_annex";
pub const GET_SOURCE_PAGE_TOOL: &str = "get_source_page";
pub const DEFAULT_HTTP_BIND_ADDR: &str = "127.0.0.1:8787";
const SEARCH_FANOUT_LIMIT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub config_path: PathBuf,
    pub transport: ServerTransport,
    pub bind_addr: SocketAddr,
    pub allowed_hosts: Vec<String>,
    pub query_log_path: Option<PathBuf>,
    pub extra_packs: Vec<ExtraPackConfig>,
}

#[derive(Debug, Clone)]
pub struct TransportArgs {
    pub transport: ServerTransport,
    pub bind_addr: SocketAddr,
    pub allowed_hosts: Vec<String>,
    pub query_log_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub institution: String,
    #[serde(default)]
    pub pack: PackConfig,
    #[serde(default)]
    pub extra_packs: Vec<ExtraPackConfig>,
    #[serde(default)]
    pub vectors: VectorConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PackConfig {
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub effective: Option<String>,
    pub source_commit: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExtraPackConfig {
    pub institution: String,
    #[serde(default)]
    pub pack: PackConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VectorConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default)]
    pub model_dir: Option<PathBuf>,
    #[serde(default)]
    pub rrf_k: Option<usize>,
    #[serde(default)]
    pub vector_weight: Option<f32>,
}

impl VectorConfig {
    fn to_search_options(&self) -> VectorSearchOptions {
        VectorSearchOptions {
            enabled: self.enabled,
            cache_dir: self.cache_dir.clone(),
            model_dir: self.model_dir.clone(),
            rrf_k: self.rrf_k.unwrap_or(rules_core::DEFAULT_RRF_K),
            vector_weight: self.vector_weight.unwrap_or(1.0),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct SearchRulesParams {
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub institution: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct SearchRulesResult {
    pub hits: Vec<SearchHit>,
    pub meta: FreshnessMeta,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hit_meta: BTreeMap<String, FreshnessMeta>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct GetArticleParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct GetArticleResult {
    pub article: Option<Article>,
    pub prev_id: Option<String>,
    pub next_id: Option<String>,
    pub legal_basis: Vec<LegalBasis>,
    pub meta: FreshnessMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct GetAnnexParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct GetAnnexResult {
    pub annex: Option<Annex>,
    pub text: Option<String>,
    pub table_structured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_markdown: Option<String>,
    pub pages: Vec<u32>,
    pub rule: Option<String>,
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub meta: FreshnessMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct GetSourcePageParams {
    #[serde(default)]
    pub institution: Option<String>,
    pub page: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct GetSourcePageResult {
    pub page: u32,
    pub institution: Option<String>,
    pub text: Option<String>,
    pub source_url: Option<String>,
    pub owner_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub meta: FreshnessMeta,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ListRulesParams {}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct ListRulesResult {
    pub rules: Vec<RuleSummary>,
    pub meta: FreshnessMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct GetLegalBasisParams {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct GetLegalBasisResult {
    pub basis: Vec<LegalBasis>,
    pub meta: FreshnessMeta,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct StatusParams {}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct StatusResult {
    pub institution: String,
    pub effective_date: String,
    pub source_commit: String,
    pub index_built_at: String,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub institutions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<LoadedPackStatus>,
    pub meta: FreshnessMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct LoadedPackStatus {
    pub institution: String,
    pub effective_date: String,
    pub source_commit: String,
    pub index_built_at: String,
    pub stale: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FreshnessMeta {
    pub effective: String,
    pub amended: String,
    pub source_commit: String,
    pub extra: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct PublicRulesServer {
    packs: Arc<Vec<LoadedPack>>,
    default_institution: String,
    multi_pack: bool,
    tool_router: ToolRouter<Self>,
    query_logger: Option<QueryLogger>,
}

#[derive(Debug, Clone)]
struct LoadedPack {
    institution: String,
    aliases: Vec<String>,
    index: Arc<TantivyRulesIndex>,
}

impl PublicRulesServer {
    pub fn new(index: TantivyRulesIndex) -> Self {
        let institution = index.status().institution;
        Self {
            packs: Arc::new(vec![LoadedPack {
                institution: institution.clone(),
                aliases: institution_aliases(&institution, None, &index),
                index: Arc::new(index),
            }]),
            default_institution: institution,
            multi_pack: false,
            tool_router: Self::tool_router(),
            query_logger: None,
        }
    }

    pub fn with_query_logger(mut self, query_logger: QueryLogger) -> Self {
        self.query_logger = Some(query_logger);
        self
    }

    pub fn from_config(config: ServerConfig) -> anyhow::Result<Self> {
        let default_institution = config.institution.clone();
        let mut seen = BTreeMap::new();
        seen.insert(default_institution.clone(), ());
        let vector_options = config.vectors.to_search_options();
        let default_pack_path = config.pack.path.clone();
        let default_index = load_pack(
            default_institution.clone(),
            config.pack,
            vector_options.clone(),
        )?;
        let mut packs = vec![LoadedPack {
            institution: default_institution.clone(),
            aliases: institution_aliases(
                &default_institution,
                default_pack_path.as_deref(),
                &default_index,
            ),
            index: Arc::new(default_index),
        }];
        for extra in config.extra_packs {
            if seen.insert(extra.institution.clone(), ()).is_some() {
                return Err(anyhow::anyhow!(
                    "duplicate pack institution: {}",
                    extra.institution
                ));
            }
            let extra_pack_path = extra.pack.path.clone();
            let extra_index = load_pack(
                extra.institution.clone(),
                extra.pack,
                vector_options.clone(),
            )?;
            packs.push(LoadedPack {
                institution: extra.institution.clone(),
                aliases: institution_aliases(
                    &extra.institution,
                    extra_pack_path.as_deref(),
                    &extra_index,
                ),
                index: Arc::new(extra_index),
            });
        }
        let multi_pack = packs.len() > 1;
        Ok(Self {
            packs: Arc::new(packs),
            default_institution,
            multi_pack,
            tool_router: Self::tool_router(),
            query_logger: None,
        })
    }

    pub fn from_config_toml(toml_text: &str) -> anyhow::Result<Self> {
        Self::from_config(toml::from_str(toml_text)?)
    }

    fn status_meta(&self, article: Option<&Article>) -> FreshnessMeta {
        let status = article
            .and_then(|article| self.pack_for_institution(&article.institution))
            .unwrap_or_else(|| self.default_pack())
            .index
            .status();
        FreshnessMeta {
            effective: article
                .map(|article| article.effective.clone())
                .unwrap_or_else(|| status.effective_date.clone()),
            amended: article
                .map(|article| article.amended.clone())
                .unwrap_or_else(|| status.effective_date.clone()),
            source_commit: status.source_commit,
            extra: source_url_extra(status.source_url),
        }
    }

    fn pack_meta(&self, pack: &LoadedPack) -> FreshnessMeta {
        let status = pack.index.status();
        FreshnessMeta {
            effective: status.effective_date.clone(),
            amended: status.effective_date,
            source_commit: status.source_commit,
            extra: source_url_extra(status.source_url),
        }
    }

    fn default_pack(&self) -> &LoadedPack {
        self.packs
            .iter()
            .find(|pack| pack.institution == self.default_institution)
            .unwrap_or_else(|| &self.packs[0])
    }

    fn pack_for_institution(&self, institution: &str) -> Option<&LoadedPack> {
        self.packs
            .iter()
            .find(|pack| pack.institution == institution)
    }

    fn selected_packs(&self, institution: Option<&str>) -> Vec<&LoadedPack> {
        match institution {
            Some(institution) => self
                .pack_for_institution(institution)
                .into_iter()
                .collect::<Vec<_>>(),
            None => self.packs.iter().collect(),
        }
    }

    fn selected_packs_for_query(&self, institution: Option<&str>, query: &str) -> Vec<&LoadedPack> {
        let selected = self.selected_packs(institution);
        if institution.is_some() || selected.len() <= 1 {
            return selected;
        }
        let normalized_query = normalize_alias(query);
        let routed = selected
            .iter()
            .copied()
            .filter(|pack| {
                pack.aliases.iter().any(|alias| {
                    let alias = normalize_alias(alias);
                    alias.chars().count() >= 2 && normalized_query.contains(alias.as_str())
                })
            })
            .collect::<Vec<_>>();
        if routed.is_empty() {
            selected
        } else {
            routed
        }
    }

    fn namespace_article(&self, mut article: Article, institution: &str) -> Article {
        article.institution = institution.to_string();
        if self.multi_pack {
            article.id = prefixed_article_id(institution, &article.id);
            article.prev_id = article
                .prev_id
                .map(|id| prefixed_article_id(institution, &id));
            article.next_id = article
                .next_id
                .map(|id| prefixed_article_id(institution, &id));
            for reference in &mut article.refs {
                reference.target = prefixed_article_id(institution, &reference.target);
            }
            article.annex_refs = article
                .annex_refs
                .into_iter()
                .map(|id| prefixed_article_id(institution, &id))
                .collect();
        }
        article
    }

    fn namespace_annex(&self, mut annex: Annex, institution: &str) -> Annex {
        annex.institution = institution.to_string();
        if self.multi_pack {
            annex.id = prefixed_article_id(institution, &annex.id);
        }
        annex
    }

    fn namespace_source_page(&self, mut page: SourcePage, institution: &str) -> SourcePage {
        page.institution = institution.to_string();
        if self.multi_pack {
            page.owner_ids = page
                .owner_ids
                .into_iter()
                .map(|id| prefixed_article_id(institution, &id))
                .collect();
        }
        page
    }

    fn resolve_article_id<'a>(&'a self, id: &'a str) -> Option<(&'a LoadedPack, &'a str)> {
        if let Some((institution, local_id)) = id.split_once('/') {
            if self.pack_for_institution(institution).is_some()
                && local_id.contains(rules_core::ARTICLE_ID_SEPARATOR)
            {
                return self
                    .pack_for_institution(institution)
                    .map(|pack| (pack, local_id));
            }
        }
        Some((self.default_pack(), id))
    }

    fn hit_meta(&self, hits: &[SearchHit]) -> BTreeMap<String, FreshnessMeta> {
        hits.iter()
            .filter_map(|hit| {
                let pack = self.pack_for_institution(&hit.institution)?;
                let status = pack.index.status();
                Some((
                    hit.article_id.clone(),
                    FreshnessMeta {
                        effective: hit.effective.clone(),
                        amended: hit.effective.clone(),
                        source_commit: status.source_commit,
                        extra: source_url_extra(status.source_url),
                    },
                ))
            })
            .collect()
    }

    fn log_query(&self, started_at: Instant, event: serde_json::Value) {
        if let Some(query_logger) = &self.query_logger {
            query_logger.log(started_at, event);
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryLogger {
    file: Arc<Mutex<std::fs::File>>,
    warned: Arc<AtomicBool>,
}

impl QueryLogger {
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            warned: Arc::new(AtomicBool::new(false)),
        })
    }

    fn log(&self, started_at: Instant, mut event: serde_json::Value) {
        if let Some(object) = event.as_object_mut() {
            object.insert("ts_ms".to_string(), serde_json::json!(unix_epoch_ms()));
            object.insert(
                "duration_ms".to_string(),
                serde_json::json!(started_at.elapsed().as_millis() as u64),
            );
        }
        let line = match serde_json::to_string(&event) {
            Ok(line) => line,
            Err(error) => {
                self.warn_once(format!("query log serialization failed: {error}"));
                return;
            }
        };
        let result = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("query log mutex poisoned"))
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = result {
            self.warn_once(format!("query log write failed: {error}"));
        }
    }

    fn warn_once(&self, message: String) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!("{message}");
        }
    }
}

fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn optional_query_logger(path: Option<PathBuf>) -> Option<QueryLogger> {
    let path = path?;
    match QueryLogger::new(&path) {
        Ok(logger) => Some(logger),
        Err(error) => {
            eprintln!(
                "query log disabled: failed to open {}: {error}",
                path.display()
            );
            None
        }
    }
}

fn load_pack(
    institution: String,
    pack: PackConfig,
    vector_options: VectorSearchOptions,
) -> anyhow::Result<TantivyRulesIndex> {
    let path = pack
        .path
        .ok_or_else(|| anyhow::anyhow!("pack.path is required for M0 local stdio server"))?;
    let index = if path.is_file() {
        TantivyRulesIndex::from_pack_archive_with_vector_options(path, vector_options)?
    } else if path.join("manifest.json").is_file() {
        TantivyRulesIndex::from_pack_dir_with_vector_options(path, vector_options)?
    } else {
        if vector_options.enabled {
            eprintln!("vector search disabled for bare articles dir: pack manifest is required for a stable cache key");
        }
        let effective = if let Some(effective) = pack.effective.clone() {
            effective
        } else {
            most_frequent_effective(&path)?.unwrap_or_else(|| "unknown".to_string())
        };
        let source_commit = pack
            .source_commit
            .clone()
            .unwrap_or_else(|| "bare-dir".to_string());
        let mut status = default_pack_status(institution, effective);
        status.source_commit = source_commit;
        TantivyRulesIndex::from_articles_dir(path, status)?
    };
    Ok(index)
}

fn institution_aliases(
    institution: &str,
    pack_path: Option<&Path>,
    index: &TantivyRulesIndex,
) -> Vec<String> {
    let mut aliases = Vec::<String>::new();
    push_alias(&mut aliases, institution);
    push_alias(&mut aliases, &index.status().institution);
    if let Some(label) =
        pack_path.and_then(|path| institution_label_from_pack_path(path, institution))
    {
        push_alias(&mut aliases, &label);
        for derived in derived_institution_aliases(&label) {
            push_alias(&mut aliases, &derived);
        }
    }
    aliases
}

fn institution_label_from_pack_path(path: &Path, institution: &str) -> Option<String> {
    let root = pack_root_for_metadata(path);
    let nodes_path = root.join("graph/nodes.jsonl");
    let text = fs::read_to_string(nodes_path).ok()?;
    text.lines()
        .filter_map(|line| serde_json::from_str::<GraphNode>(line).ok())
        .find(|node| {
            node.kind == NodeKind::Institution
                && (node.id == institution
                    || node
                        .meta
                        .get("institution")
                        .and_then(|value| value.as_str())
                        == Some(institution))
        })
        .map(|node| node.label)
}

fn pack_root_for_metadata(path: &Path) -> PathBuf {
    if path.file_name().and_then(|name| name.to_str()) == Some("articles") {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    }
}

fn derived_institution_aliases(label: &str) -> Vec<String> {
    let compact = normalize_alias(label);
    ["충청남도", "충남", "한국", "재단법인", "(재)", "재단"]
        .iter()
        .filter_map(|prefix| compact.strip_prefix(&normalize_alias(prefix)))
        .filter(|alias| alias.chars().count() >= 3)
        .map(ToString::to_string)
        .collect()
}

fn push_alias(aliases: &mut Vec<String>, alias: &str) {
    let alias = alias.trim();
    if !alias.is_empty() && !aliases.iter().any(|existing| existing == alias) {
        aliases.push(alias.to_string());
    }
}

fn normalize_alias(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_' && *ch != '·')
        .flat_map(char::to_lowercase)
        .collect()
}

fn source_url_extra(source_url: Option<String>) -> BTreeMap<String, String> {
    let mut extra = BTreeMap::new();
    if let Some(source_url) = source_url {
        extra.insert("source_url".to_string(), source_url);
    }
    extra
}

fn search_pack_report(
    pack: &LoadedPack,
    query: &str,
    limit: usize,
    rule: Option<String>,
    multi_pack: bool,
) -> SearchRouteReport {
    let report = pack.index.search_with_routes(
        query,
        limit,
        Some(RuleFilter {
            institution: None,
            rule,
            ..RuleFilter::default()
        }),
    );
    rules_core::namespace_search_route_report(report, &pack.institution, multi_pack)
}

async fn search_pack_reports_blocking(
    packs: Vec<&LoadedPack>,
    query: String,
    limit: usize,
    rule: Option<String>,
    multi_pack: bool,
) -> Vec<SearchRouteReport> {
    let max_parallel = SEARCH_FANOUT_LIMIT.min(packs.len().max(1));
    let semaphore = Arc::new(Semaphore::new(max_parallel));
    let mut handles = Vec::with_capacity(packs.len());
    for pack in packs {
        let permit = semaphore.clone().acquire_owned().await;
        let pack = pack.clone();
        let query = query.clone();
        let rule = rule.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let _permit = permit.ok();
            search_pack_report(&pack, &query, limit, rule, multi_pack)
        }));
    }

    let mut rankings = Vec::with_capacity(handles.len());
    for handle in handles {
        rankings.push(handle.await.unwrap_or_default());
    }
    rankings
}

fn most_frequent_effective(path: &Path) -> anyhow::Result<Option<String>> {
    let mut counts = BTreeMap::<String, usize>::new();
    collect_effective_counts(path, &mut counts)?;
    Ok(counts
        .into_iter()
        .max_by(|(effective_a, count_a), (effective_b, count_b)| {
            count_a
                .cmp(count_b)
                .then_with(|| effective_b.cmp(effective_a))
        })
        .map(|(effective, _)| effective))
}

fn collect_effective_counts(
    path: &Path,
    counts: &mut BTreeMap<String, usize>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_effective_counts(&path, counts)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let article = parse_article_markdown(&path)?;
        *counts.entry(article.effective).or_default() += 1;
    }
    Ok(())
}

#[tool_router(router = tool_router)]
impl PublicRulesServer {
    #[tool(
        name = "search_rules",
        description = "Search institutional rule articles."
    )]
    pub async fn search_rules(
        &self,
        Parameters(params): Parameters<SearchRulesParams>,
    ) -> Json<SearchRulesResult> {
        let started_at = Instant::now();
        let query = params.query.clone();
        let top_k = params.top_k;
        let rule = params.rule.clone();
        let institution = params.institution.clone();
        let limit = top_k.unwrap_or(5);
        let selected_packs = self.selected_packs_for_query(institution.as_deref(), &query);
        let reports = if selected_packs.len() <= 1 {
            selected_packs
                .into_iter()
                .map(|pack| search_pack_report(pack, &query, limit, rule.clone(), self.multi_pack))
                .collect::<Vec<_>>()
        } else {
            search_pack_reports_blocking(
                selected_packs,
                query.clone(),
                limit,
                rule.clone(),
                self.multi_pack,
            )
            .await
        };
        let hits = rules_core::merge_search_route_reports(reports, limit).hits;
        let hit_meta = if self.multi_pack {
            self.hit_meta(&hits)
        } else {
            BTreeMap::new()
        };
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": SEARCH_RULES_TOOL,
                "query": query.clone(),
                "params": {
                    "query": query.clone(),
                    "top_k": top_k,
                    "rule": rule,
                    "institution": institution,
                },
                "result": {
                    "article_ids": hits.iter().map(|hit| hit.article_id.as_str()).collect::<Vec<_>>(),
                    "hit_count": hits.len(),
                },
            }),
        );
        Json(SearchRulesResult {
            hits,
            meta: self.status_meta(None),
            hit_meta,
        })
    }

    #[tool(
        name = "get_annex",
        description = "Get one annex/bylaw attachment with source metadata."
    )]
    pub async fn get_annex(
        &self,
        Parameters(params): Parameters<GetAnnexParams>,
    ) -> Json<GetAnnexResult> {
        let started_at = Instant::now();
        let id = params.id;
        let resolved = self.resolve_article_id(&id);
        let annex = resolved.and_then(|(pack, local_id)| {
            pack.index
                .get_annex(local_id)
                .map(|annex| self.namespace_annex(annex, &pack.institution))
        });
        let pack = annex
            .as_ref()
            .and_then(|annex| self.pack_for_institution(&annex.institution))
            .or_else(|| resolved.map(|(pack, _)| pack))
            .unwrap_or_else(|| self.default_pack());
        let status = pack.index.status();
        let warning = annex.as_ref().and_then(|annex| {
            (!annex.table_structured).then(|| "표 구조 미보장 — 원문 페이지 확인 권장".to_string())
        });
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": GET_ANNEX_TOOL,
                "params": { "id": id.clone() },
                "result": {
                    "id": id,
                    "found": annex.is_some(),
                },
            }),
        );
        Json(GetAnnexResult {
            text: annex.as_ref().map(|annex| annex.body.clone()),
            table_structured: annex
                .as_ref()
                .map(|annex| annex.table_structured)
                .unwrap_or(false),
            table_markdown: annex
                .as_ref()
                .and_then(|annex| annex.table_markdown.clone()),
            pages: annex
                .as_ref()
                .map(|annex| annex.pages.clone())
                .unwrap_or_default(),
            rule: annex.as_ref().map(|annex| annex.rule.clone()),
            source_url: status.source_url,
            warning,
            meta: self.pack_meta(pack),
            annex,
        })
    }

    #[tool(
        name = "get_source_page",
        description = "Get raw source text for one page."
    )]
    pub async fn get_source_page(
        &self,
        Parameters(params): Parameters<GetSourcePageParams>,
    ) -> Json<GetSourcePageResult> {
        let started_at = Instant::now();
        let selected = if self.multi_pack && params.institution.is_none() {
            None
        } else {
            params
                .institution
                .as_deref()
                .and_then(|institution| self.pack_for_institution(institution))
                .or_else(|| (!self.multi_pack).then(|| self.default_pack()))
        };
        let mut error = None;
        let page = if self.multi_pack && params.institution.is_none() {
            error = Some("다중 팩 모드에서는 institution이 필요합니다".to_string());
            None
        } else if let Some(pack) = selected {
            if !pack.index.has_source_pages() {
                error = Some("이 팩은 페이지 원문 미포함".to_string());
                None
            } else {
                pack.index
                    .get_source_page(params.page)
                    .map(|page| self.namespace_source_page(page, &pack.institution))
                    .or_else(|| {
                        error = Some(format!("페이지 원문을 찾을 수 없습니다: {}", params.page));
                        None
                    })
            }
        } else {
            error = Some("기관 팩을 찾을 수 없습니다".to_string());
            None
        };
        let pack = selected.unwrap_or_else(|| self.default_pack());
        let status = pack.index.status();
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": GET_SOURCE_PAGE_TOOL,
                "params": {
                    "institution": params.institution,
                    "page": params.page,
                },
                "result": {
                    "found": page.is_some(),
                    "error": error,
                },
            }),
        );
        Json(GetSourcePageResult {
            page: params.page,
            institution: page
                .as_ref()
                .map(|page| page.institution.clone())
                .or_else(|| selected.map(|pack| pack.institution.clone())),
            text: page.as_ref().map(|page| page.text.clone()),
            source_url: status.source_url,
            owner_ids: page.map(|page| page.owner_ids).unwrap_or_default(),
            error,
            meta: self.pack_meta(pack),
        })
    }

    #[tool(
        name = "get_article",
        description = "Get one rule article with freshness metadata."
    )]
    pub async fn get_article(
        &self,
        Parameters(params): Parameters<GetArticleParams>,
    ) -> Json<GetArticleResult> {
        let started_at = Instant::now();
        let id = params.id;
        let article = self.resolve_article_id(&id).and_then(|(pack, local_id)| {
            pack.index
                .get_article(local_id)
                .map(|article| self.namespace_article(article, &pack.institution))
        });
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": GET_ARTICLE_TOOL,
                "params": { "id": id.clone() },
                "result": {
                    "id": id,
                    "found": article.is_some(),
                },
            }),
        );
        Json(GetArticleResult {
            prev_id: article.as_ref().and_then(|article| article.prev_id.clone()),
            next_id: article.as_ref().and_then(|article| article.next_id.clone()),
            legal_basis: article
                .as_ref()
                .map(|article| article.legal_basis.clone())
                .unwrap_or_default(),
            meta: self.status_meta(article.as_ref()),
            article,
        })
    }

    #[tool(name = "list_rules", description = "List rules in the loaded pack.")]
    pub async fn list_rules(&self) -> Json<ListRulesResult> {
        let started_at = Instant::now();
        let rules = self
            .packs
            .iter()
            .flat_map(|pack| pack.index.list_rules())
            .collect::<Vec<_>>();
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": LIST_RULES_TOOL,
                "params": {},
                "result": {
                    "count": rules.len(),
                },
            }),
        );
        Json(ListRulesResult {
            rules,
            meta: self.status_meta(None),
        })
    }

    #[tool(
        name = "get_legal_basis",
        description = "Get legal basis for an article."
    )]
    pub async fn get_legal_basis(
        &self,
        Parameters(params): Parameters<GetLegalBasisParams>,
    ) -> Json<GetLegalBasisResult> {
        let started_at = Instant::now();
        let id = params.id;
        let resolved = self.resolve_article_id(&id);
        let article = resolved.and_then(|(pack, local_id)| pack.index.get_article(local_id));
        let basis = article
            .as_ref()
            .map(|article| article.legal_basis.clone())
            .unwrap_or_else(|| {
                resolved
                    .map(|(pack, local_id)| pack.index.related_laws(local_id))
                    .unwrap_or_default()
            });
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": GET_LEGAL_BASIS_TOOL,
                "params": { "id": id.clone() },
                "result": {
                    "count": basis.len(),
                },
            }),
        );
        Json(GetLegalBasisResult {
            basis,
            meta: self.status_meta(article.as_ref()),
        })
    }

    #[tool(name = "status", description = "Return loaded pack freshness status.")]
    pub async fn status(&self) -> Json<StatusResult> {
        let started_at = Instant::now();
        let status = self.status_result();
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": STATUS_TOOL,
                "params": {},
                "result": {
                    "count": self.packs.len(),
                },
            }),
        );
        Json(status)
    }
}

impl PublicRulesServer {
    fn status_result(&self) -> StatusResult {
        let default_status = self.default_pack().index.status();
        let packs = if self.multi_pack {
            self.packs
                .iter()
                .map(|pack| LoadedPackStatus::from(pack.index.status()))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let institutions = if self.multi_pack {
            packs
                .iter()
                .map(|pack| pack.institution.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        StatusResult {
            institution: default_status.institution.clone(),
            effective_date: default_status.effective_date.clone(),
            source_commit: default_status.source_commit.clone(),
            index_built_at: default_status.index_built_at.clone(),
            stale: default_status.stale,
            institutions,
            packs,
            meta: FreshnessMeta {
                effective: default_status.effective_date.clone(),
                amended: default_status.effective_date,
                source_commit: default_status.source_commit,
                extra: source_url_extra(default_status.source_url),
            },
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PublicRulesServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("public-rules-mcp", env!("CARGO_PKG_VERSION")),
        )
    }
}

impl From<PackStatus> for StatusResult {
    fn from(status: PackStatus) -> Self {
        let meta = FreshnessMeta {
            effective: status.effective_date.clone(),
            amended: status.effective_date.clone(),
            source_commit: status.source_commit.clone(),
            extra: source_url_extra(status.source_url.clone()),
        };
        Self {
            institution: status.institution,
            effective_date: status.effective_date,
            source_commit: status.source_commit,
            index_built_at: status.index_built_at,
            stale: status.stale,
            institutions: Vec::new(),
            packs: Vec::new(),
            meta,
        }
    }
}

impl From<PackStatus> for LoadedPackStatus {
    fn from(status: PackStatus) -> Self {
        Self {
            institution: status.institution,
            effective_date: status.effective_date,
            source_commit: status.source_commit,
            index_built_at: status.index_built_at,
            stale: status.stale,
        }
    }
}

pub async fn run_stdio_server(config: ServerConfig) -> anyhow::Result<()> {
    run_stdio_server_with_query_log(config, None).await
}

pub async fn run_stdio_server_with_query_log(
    config: ServerConfig,
    query_log_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let server = PublicRulesServer::from_config(config)?;
    let server = if let Some(logger) = optional_query_logger(query_log_path) {
        server.with_query_logger(logger)
    } else {
        server
    };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

pub async fn run_http_server(config: ServerConfig, bind_addr: SocketAddr) -> anyhow::Result<()> {
    run_http_server_with_allowed_hosts(config, bind_addr, Vec::<String>::new()).await
}

pub async fn run_http_server_with_allowed_hosts(
    config: ServerConfig,
    bind_addr: SocketAddr,
    extra_allowed_hosts: Vec<String>,
) -> anyhow::Result<()> {
    run_http_server_with_options(config, bind_addr, extra_allowed_hosts, None).await
}

pub async fn run_http_server_with_options(
    config: ServerConfig,
    bind_addr: SocketAddr,
    extra_allowed_hosts: Vec<String>,
    query_log_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let server = PublicRulesServer::from_config(config)?;
    let server = if let Some(logger) = optional_query_logger(query_log_path) {
        server.with_query_logger(logger)
    } else {
        server
    };
    let cancellation_token = CancellationToken::new();
    let mut http_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_cancellation_token(cancellation_token.child_token());
    if !extra_allowed_hosts.is_empty() {
        let mut allowed_hosts = default_allowed_hosts();
        allowed_hosts.extend(extra_allowed_hosts);
        http_config = http_config.with_allowed_hosts(allowed_hosts);
    }
    let service: StreamableHttpService<PublicRulesServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), http_config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            wait_for_shutdown_signal().await;
            cancellation_token.cancel();
        })
        .await?;
    Ok(())
}

pub async fn run_server(
    config: ServerConfig,
    transport: ServerTransport,
    bind_addr: SocketAddr,
) -> anyhow::Result<()> {
    run_server_with_transport_args(
        config,
        TransportArgs {
            transport,
            bind_addr,
            allowed_hosts: Vec::new(),
            query_log_path: None,
        },
    )
    .await
}

pub async fn run_server_with_transport_args(
    config: ServerConfig,
    args: TransportArgs,
) -> anyhow::Result<()> {
    match args.transport {
        ServerTransport::Stdio => {
            run_stdio_server_with_query_log(config, args.query_log_path).await
        }
        ServerTransport::Http => {
            run_http_server_with_options(
                config,
                args.bind_addr,
                args.allowed_hosts,
                args.query_log_path,
            )
            .await
        }
    }
}

pub async fn run_stdio_server_from_toml(toml_text: &str) -> anyhow::Result<()> {
    run_stdio_server(toml::from_str(toml_text)?).await
}

pub fn load_cli_args() -> anyhow::Result<CliArgs> {
    parse_cli_args(std::env::args().skip(1))
}

fn parse_cli_args<I, S>(args: I) -> anyhow::Result<CliArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let mut config_path = None;
    let mut transport_args = TransportArgs::default();
    let mut extra_packs = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow::anyhow!("--config requires a TOML path"))?,
                );
            }
            "--extra-pack" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--extra-pack requires <slug>=<path>"))?;
                extra_packs.push(parse_extra_pack_arg(&value)?);
            }
            _ => parse_transport_arg(&arg, &mut args, &mut transport_args)?,
        }
    }
    let path =
        config_path.ok_or_else(|| anyhow::anyhow!("usage: public-rules-mcp --config <toml>"))?;
    Ok(CliArgs {
        config_path: path,
        transport: transport_args.transport,
        bind_addr: transport_args.bind_addr,
        allowed_hosts: transport_args.allowed_hosts,
        query_log_path: transport_args.query_log_path,
        extra_packs,
    })
}

pub fn load_transport_args() -> anyhow::Result<TransportArgs> {
    parse_transport_args(std::env::args().skip(1))
}

fn parse_transport_args<I, S>(args: I) -> anyhow::Result<TransportArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let mut transport_args = TransportArgs::default();
    while let Some(arg) = args.next() {
        parse_transport_arg(&arg, &mut args, &mut transport_args)?;
    }
    Ok(transport_args)
}

fn parse_transport_arg<I>(
    arg: &str,
    args: &mut I,
    transport_args: &mut TransportArgs,
) -> anyhow::Result<()>
where
    I: Iterator<Item = String>,
{
    match arg {
        "--transport" => {
            transport_args.transport = parse_transport(
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("--transport requires stdio or http"))?,
            )?;
        }
        "--bind" => {
            transport_args.bind_addr = parse_bind_addr(
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("--bind requires <addr:port>"))?,
            )?;
        }
        "--allowed-host" => {
            transport_args.allowed_hosts.push(
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("--allowed-host requires <host>"))?,
            );
        }
        "--query-log" => {
            transport_args.query_log_path = Some(
                args.next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--query-log requires <path>"))?,
            );
        }
        _ => return Err(anyhow::anyhow!("unknown argument: {arg}")),
    }
    Ok(())
}

impl Default for TransportArgs {
    fn default() -> Self {
        Self {
            transport: ServerTransport::Stdio,
            bind_addr: DEFAULT_HTTP_BIND_ADDR
                .parse()
                .expect("default HTTP bind address must be valid"),
            allowed_hosts: Vec::new(),
            query_log_path: None,
        }
    }
}

fn parse_transport(value: String) -> anyhow::Result<ServerTransport> {
    match value.as_str() {
        "stdio" => Ok(ServerTransport::Stdio),
        "http" => Ok(ServerTransport::Http),
        _ => Err(anyhow::anyhow!(
            "--transport must be either stdio or http, got {value}"
        )),
    }
}

fn parse_bind_addr(value: String) -> anyhow::Result<SocketAddr> {
    value.parse().map_err(|error| {
        anyhow::anyhow!(
            "invalid --bind address {value}: expected numeric IP:port (예: 0.0.0.0:8787): {error}"
        )
    })
}

fn parse_extra_pack_arg(value: &str) -> anyhow::Result<ExtraPackConfig> {
    let (institution, path) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--extra-pack requires <slug>=<path>"))?;
    if institution.trim().is_empty() || path.trim().is_empty() {
        return Err(anyhow::anyhow!("--extra-pack requires <slug>=<path>"));
    }
    if !institution
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(anyhow::anyhow!(
            "--extra-pack slug may contain only ASCII letters, digits, '_' or '-'"
        ));
    }
    Ok(ExtraPackConfig {
        institution: institution.to_string(),
        pack: PackConfig {
            path: Some(PathBuf::from(path)),
            ..PackConfig::default()
        },
    })
}

fn default_allowed_hosts() -> Vec<String> {
    StreamableHttpServerConfig::default().allowed_hosts
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler must install");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn fixture_server() -> PublicRulesServer {
        let articles = [r#"---
institution: cni
rule: 여비지급규칙
article: 제12조
title: 항공운임의 지급
effective: 2026-02-27
amended: 2026-02-27
status: active
supersedes: null
legal_basis:
  - law: 근로기준법
    article: 제60조
    mst: "265959"
refs: []
---
① 원장은 출장업무가 시급을 요할 때 항공운임 지급 여부를 결정한다.
"#]
        .into_iter()
        .map(rules_core::parse_article_markdown_str)
        .collect::<rules_core::Result<Vec<_>>>()
        .unwrap();

        let status = default_pack_status("cni", "2026-02-27");
        let index = TantivyRulesIndex::from_articles(articles, status).unwrap();
        PublicRulesServer::new(index)
    }

    fn fixture_article(
        institution: &str,
        rule: &str,
        article: &str,
        title: &str,
        effective: &str,
        body: &str,
    ) -> Article {
        rules_core::parse_article_markdown_str(&format!(
            r#"---
institution: {institution}
rule: {rule}
article: {article}
title: {title}
effective: {effective}
amended: {effective}
status: active
supersedes: null
legal_basis: []
refs: []
---
{body}
"#
        ))
        .unwrap()
    }

    fn multi_pack_fixture_server() -> PublicRulesServer {
        let cni = TantivyRulesIndex::from_articles(
            vec![fixture_article(
                "cni",
                "인사규정",
                "제10조",
                "육아휴직",
                "2026-02-27",
                "① 직원은 육아휴직을 신청할 수 있다.",
            )],
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();
        let ctp = TantivyRulesIndex::from_articles(
            vec![fixture_article(
                "ctp",
                "인사규정",
                "제10조",
                "육아휴직",
                "2026-03-01",
                "① 임직원 육아휴직 기간은 별도로 정한다.",
            )],
            default_pack_status("ctp", "2026-03-01"),
        )
        .unwrap();

        PublicRulesServer {
            packs: Arc::new(vec![
                LoadedPack {
                    institution: "cni".to_string(),
                    aliases: vec!["cni".to_string(), "충남연구원".to_string()],
                    index: Arc::new(cni),
                },
                LoadedPack {
                    institution: "ctp".to_string(),
                    aliases: vec![
                        "ctp".to_string(),
                        "충남테크노파크".to_string(),
                        "테크노파크".to_string(),
                    ],
                    index: Arc::new(ctp),
                },
            ]),
            default_institution: "cni".to_string(),
            multi_pack: true,
            tool_router: PublicRulesServer::tool_router(),
            query_logger: None,
        }
    }

    fn unique_temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "public-rules-mcp-{name}-{}-{}",
            std::process::id(),
            unix_epoch_ms()
        ))
    }

    fn write_manifest(root: &Path, institution: &str) {
        let mut files = BTreeMap::<String, String>::new();
        collect_fixture_files(root, root, &mut files);
        let manifest = serde_json::json!({
            "schema_version": 1,
            "institution": institution,
            "effective_date": "2026-02-27",
            "source_commit": format!("fixture-{institution}"),
            "created_at": "2026-07-07T00:00:00Z",
            "source_url": format!("https://example.test/{institution}.pdf"),
            "files": files,
        });
        fs::write(
            root.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn collect_fixture_files(root: &Path, current: &Path, files: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                collect_fixture_files(root, &path, files);
            } else if path.file_name().and_then(|name| name.to_str()) != Some("manifest.json") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.insert(relative, sha256_hex(&path));
            }
        }
    }

    fn sha256_hex(path: &Path) -> String {
        let bytes = fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn make_r5_pack(institution: &str) -> PathBuf {
        let root = unique_temp_root(institution);
        let article_dir = root.join("articles/여비지급규칙");
        let annex_dir = root.join("annexes/여비지급규칙");
        let page_dir = root.join("pages");
        fs::create_dir_all(&article_dir).unwrap();
        fs::create_dir_all(&annex_dir).unwrap();
        fs::create_dir_all(&page_dir).unwrap();
        fs::write(
            article_dir.join("제10조.md"),
            format!(
                r#"---
institution: {institution}
rule: 여비지급규칙
article: 제10조
title: 국내여비
effective: 2026-02-27
amended: 2026-02-27
status: active
pages: [345, 345]
legal_basis: []
refs:
  - target: "여비지급규칙#별표1"
    type: "인용"
---
① 국내 출장 여비 지급 기준은 <별표 1>에 따른다.
"#
            ),
        )
        .unwrap();
        fs::write(
            annex_dir.join("별표1.md"),
            format!(
                r#"---
type: annex
institution: {institution}
rule: 여비지급규칙
annex: 별표1
title: 국내출장여비
effective: 2026-02-27
status: active
pages: [350, 350]
table_structured: true
---
<별표 1>
국내출장여비 지급 기준

## Extracted tables

| 구분 | 항공 |
| --- | --- |
| 원장 | 실비 |
"#
            ),
        )
        .unwrap();
        fs::write(
            page_dir.join("0350.txt"),
            "- 350 -\n<별표 1>\n국내출장여비 지급 기준\n",
        )
        .unwrap();
        write_manifest(&root, institution);
        root
    }

    // §6 계약: "모든 응답 meta에 effective·amended·source_commit 포함(신선도 계약)."

    #[tokio::test]
    async fn search_rules_returns_hits_matching_schema() {
        let server = fixture_server();
        let Json(result) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "항공운임".to_string(),
                top_k: Some(5),
                rule: None,
                institution: None,
            }))
            .await;

        assert!(!result.hits.is_empty());
        assert_eq!(result.hits[0].article_id, "여비지급규칙#제12조");
        assert_eq!(result.hits[0].institution, "cni");
        assert_eq!(result.hits[0].rule, "여비지급규칙");
    }

    #[tokio::test]
    async fn multi_pack_search_labels_filters_and_prefixes_article_ids() {
        let server = multi_pack_fixture_server();
        let Json(result) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "육아휴직".to_string(),
                top_k: Some(5),
                rule: None,
                institution: None,
            }))
            .await;

        let ids = result
            .hits
            .iter()
            .map(|hit| (hit.institution.as_str(), hit.article_id.as_str()))
            .collect::<Vec<_>>();
        assert!(ids.contains(&("cni", "cni/인사규정#제10조")));
        assert!(ids.contains(&("ctp", "ctp/인사규정#제10조")));
        assert_eq!(
            result
                .hit_meta
                .get("ctp/인사규정#제10조")
                .map(|meta| meta.effective.as_str()),
            Some("2026-03-01")
        );

        let Json(filtered) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "육아휴직".to_string(),
                top_k: Some(5),
                rule: None,
                institution: Some("ctp".to_string()),
            }))
            .await;
        assert_eq!(filtered.hits.len(), 1);
        assert_eq!(filtered.hits[0].institution, "ctp");
        assert_eq!(filtered.hits[0].article_id, "ctp/인사규정#제10조");
    }

    #[tokio::test]
    async fn multi_pack_query_institution_name_routes_to_matching_pack() {
        let server = multi_pack_fixture_server();

        let Json(ctp_result) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "테크노파크 육아휴직".to_string(),
                top_k: Some(5),
                rule: None,
                institution: None,
            }))
            .await;
        assert!(!ctp_result.hits.is_empty());
        assert!(ctp_result.hits.iter().all(|hit| hit.institution == "ctp"));

        let Json(cni_result) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "충남연구원 육아휴직".to_string(),
                top_k: Some(5),
                rule: None,
                institution: None,
            }))
            .await;
        assert!(!cni_result.hits.is_empty());
        assert!(cni_result.hits.iter().all(|hit| hit.institution == "cni"));
    }

    #[tokio::test]
    async fn multi_pack_get_article_accepts_prefixed_and_default_unprefixed_ids() {
        let server = multi_pack_fixture_server();
        let Json(prefixed) = server
            .get_article(Parameters(GetArticleParams {
                id: "ctp/인사규정#제10조".to_string(),
            }))
            .await;
        let prefixed_article = prefixed.article.expect("prefixed article must resolve");
        assert_eq!(prefixed_article.id, "ctp/인사규정#제10조");
        assert_eq!(prefixed_article.institution, "ctp");

        let Json(unprefixed) = server
            .get_article(Parameters(GetArticleParams {
                id: "인사규정#제10조".to_string(),
            }))
            .await;
        let default_article = unprefixed.article.expect("default article must resolve");
        assert_eq!(default_article.id, "cni/인사규정#제10조");
        assert_eq!(default_article.institution, "cni");
    }

    #[tokio::test]
    async fn get_article_response_includes_freshness_meta_for_existing_article() {
        let server = fixture_server();
        let Json(result) = server
            .get_article(Parameters(GetArticleParams {
                id: "여비지급규칙#제12조".to_string(),
            }))
            .await;

        let article = result.article.expect("article must be found");
        assert_eq!(article.id, "여비지급규칙#제12조");
        assert_eq!(result.legal_basis[0].law, "근로기준법");
        assert_eq!(result.meta.effective, "2026-02-27");
        assert_eq!(result.meta.amended, "2026-02-27");
        assert_eq!(result.meta.source_commit, "fixture");
    }

    #[tokio::test]
    async fn r5_pack_search_get_annex_and_source_page_round_trip() {
        let root = make_r5_pack("cni");
        let server = PublicRulesServer::from_config(ServerConfig {
            institution: "cni".to_string(),
            pack: PackConfig {
                path: Some(root),
                ..PackConfig::default()
            },
            extra_packs: Vec::new(),
            vectors: VectorConfig::default(),
        })
        .unwrap();

        let Json(search) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "국내출장여비 항공".to_string(),
                top_k: Some(5),
                rule: None,
                institution: None,
            }))
            .await;
        assert!(search
            .hits
            .iter()
            .any(|hit| hit.article_id == "여비지급규칙#별표1" && hit.kind == "annex"));

        let Json(article) = server
            .get_article(Parameters(GetArticleParams {
                id: "여비지급규칙#제10조".to_string(),
            }))
            .await;
        assert_eq!(
            article.article.unwrap().annex_refs,
            vec!["여비지급규칙#별표1".to_string()]
        );

        let Json(annex) = server
            .get_annex(Parameters(GetAnnexParams {
                id: "여비지급규칙#별표1".to_string(),
            }))
            .await;
        assert!(annex.table_structured);
        assert!(annex.table_markdown.unwrap().contains("| 구분 | 항공 |"));
        assert_eq!(annex.pages, vec![350, 350]);
        assert_eq!(
            annex.source_url.as_deref(),
            Some("https://example.test/cni.pdf")
        );

        let Json(page) = server
            .get_source_page(Parameters(GetSourcePageParams {
                institution: None,
                page: 350,
            }))
            .await;
        assert!(page.error.is_none());
        assert!(page.text.unwrap().contains("국내출장여비"));
        assert!(page.owner_ids.contains(&"여비지급규칙#별표1".to_string()));
    }

    #[tokio::test]
    async fn multi_pack_annex_ids_are_prefixed_round_trip() {
        let cni_root = make_r5_pack("cni");
        let ctp_root = make_r5_pack("ctp");
        let mut config = ServerConfig {
            institution: "cni".to_string(),
            pack: PackConfig {
                path: Some(cni_root),
                ..PackConfig::default()
            },
            extra_packs: Vec::new(),
            vectors: VectorConfig::default(),
        };
        config.extra_packs.push(ExtraPackConfig {
            institution: "ctp".to_string(),
            pack: PackConfig {
                path: Some(ctp_root),
                ..PackConfig::default()
            },
        });
        let server = PublicRulesServer::from_config(config).unwrap();

        let Json(search) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "국내출장여비 항공".to_string(),
                top_k: Some(10),
                rule: None,
                institution: None,
            }))
            .await;
        assert!(search
            .hits
            .iter()
            .any(|hit| hit.article_id == "cni/여비지급규칙#별표1" && hit.kind == "annex"));
        assert!(search
            .hits
            .iter()
            .any(|hit| hit.article_id == "ctp/여비지급규칙#별표1" && hit.kind == "annex"));

        let Json(annex) = server
            .get_annex(Parameters(GetAnnexParams {
                id: "ctp/여비지급규칙#별표1".to_string(),
            }))
            .await;
        assert_eq!(annex.annex.unwrap().id, "ctp/여비지급규칙#별표1");

        let Json(page) = server
            .get_source_page(Parameters(GetSourcePageParams {
                institution: Some("ctp".to_string()),
                page: 350,
            }))
            .await;
        assert!(page
            .owner_ids
            .contains(&"ctp/여비지급규칙#별표1".to_string()));
    }

    #[tokio::test]
    async fn source_page_reports_missing_pages_for_legacy_pack() {
        let server = fixture_server();
        let Json(page) = server
            .get_source_page(Parameters(GetSourcePageParams {
                institution: None,
                page: 1,
            }))
            .await;

        assert_eq!(page.error.as_deref(), Some("이 팩은 페이지 원문 미포함"));
        assert!(page.text.is_none());
    }

    #[tokio::test]
    async fn get_article_missing_id_still_returns_pack_level_freshness_meta() {
        // Freshness metadata is a response-shape guarantee independent of
        // whether the requested article exists — callers must never receive
        // a response with an empty/missing meta block.
        let server = fixture_server();
        let Json(result) = server
            .get_article(Parameters(GetArticleParams {
                id: "존재하지않는규정#제1조".to_string(),
            }))
            .await;

        assert!(result.article.is_none());
        assert!(result.legal_basis.is_empty());
        assert!(!result.meta.effective.is_empty());
        assert!(!result.meta.source_commit.is_empty());
    }

    #[tokio::test]
    async fn get_legal_basis_returns_related_laws() {
        let server = fixture_server();
        let Json(result) = server
            .get_legal_basis(Parameters(GetLegalBasisParams {
                id: "여비지급규칙#제12조".to_string(),
            }))
            .await;

        assert_eq!(result.basis.len(), 1);
        assert_eq!(result.basis[0].law, "근로기준법");
        assert_eq!(result.basis[0].article, "제60조");
    }

    #[tokio::test]
    async fn status_tool_reports_loaded_pack_status() {
        let server = fixture_server();
        let Json(result) = server.status().await;

        assert_eq!(result.institution, "cni");
        assert_eq!(result.effective_date, "2026-02-27");
        assert!(!result.stale);
    }

    #[test]
    fn parse_transport_args_rejects_unknown_transport() {
        let error = parse_transport_args(["--transport", "websocket"])
            .expect_err("unknown transport must fail")
            .to_string();

        assert!(error.contains("--transport must be either stdio or http"));
    }

    #[test]
    fn parse_transport_args_rejects_missing_bind_value() {
        let error = parse_transport_args(["--bind"])
            .expect_err("missing bind value must fail")
            .to_string();

        assert!(error.contains("--bind requires <addr:port>"));
    }

    #[test]
    fn parse_transport_args_rejects_non_numeric_bind_address() {
        let error = parse_transport_args(["--bind", "localhost:8787"])
            .expect_err("non-numeric bind host must fail")
            .to_string();

        assert!(error.contains("expected numeric IP:port"));
    }

    #[test]
    fn parse_transport_args_accumulates_allowed_hosts() {
        let args = parse_transport_args([
            "--allowed-host",
            "public.example.test",
            "--allowed-host",
            "public.example.test:443",
        ])
        .expect("allowed hosts must parse");

        assert_eq!(
            args.allowed_hosts,
            vec![
                "public.example.test".to_string(),
                "public.example.test:443".to_string()
            ]
        );
    }

    #[test]
    fn parse_transport_args_accepts_query_log_path() {
        let args = parse_transport_args(["--query-log", "/tmp/public-rules-query.jsonl"])
            .expect("query log path must parse");

        assert_eq!(
            args.query_log_path.as_deref(),
            Some(Path::new("/tmp/public-rules-query.jsonl"))
        );
    }

    #[test]
    fn parse_extra_pack_rejects_path_like_slug() {
        let error = parse_extra_pack_arg("bad/slug=/tmp/pack")
            .expect_err("path-like slug must fail")
            .to_string();

        assert!(error.contains("slug may contain only ASCII"));
    }

    #[tokio::test]
    async fn resolve_article_id_treats_unknown_prefix_as_default_rule_name() {
        let server = multi_pack_fixture_server();
        let Json(result) = server
            .get_article(Parameters(GetArticleParams {
                id: "unknown/인사규정#제10조".to_string(),
            }))
            .await;

        assert!(result.article.is_none());
    }

    #[test]
    fn query_logger_appends_jsonl_with_required_fields() {
        let root = std::env::temp_dir().join(format!(
            "public-rules-mcp-query-log-{}-{}",
            std::process::id(),
            unix_epoch_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let log_path = root.join("queries.jsonl");
        let logger = QueryLogger::new(&log_path).expect("query logger must open");

        logger.log(
            Instant::now(),
            serde_json::json!({
                "tool": SEARCH_RULES_TOOL,
                "query": "항공운임",
                "params": {
                    "query": "항공운임",
                    "top_k": 5,
                    "rule": null,
                },
                "result": {
                    "article_ids": ["여비지급규칙#제12조"],
                    "hit_count": 1,
                },
            }),
        );
        logger.log(
            Instant::now(),
            serde_json::json!({
                "tool": STATUS_TOOL,
                "query": "",
                "params": {},
                "result": {
                    "count": 1,
                },
            }),
        );

        let lines = fs::read_to_string(&log_path).unwrap();
        let events = lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        for event in &events {
            assert!(event
                .get("ts_ms")
                .and_then(serde_json::Value::as_u64)
                .is_some());
            assert!(event
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .is_some());
            assert!(event
                .get("duration_ms")
                .and_then(serde_json::Value::as_u64)
                .is_some());
            assert!(event.get("query").is_some());
        }
    }

    #[tokio::test]
    async fn bare_dir_config_uses_modal_article_effective_and_injected_source_commit() {
        let root = std::env::temp_dir().join(format!(
            "public-rules-mcp-bare-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let rule_dir = root.join("여비지급규칙");
        fs::create_dir_all(&rule_dir).unwrap();
        fs::write(
            rule_dir.join("제12조.md"),
            r#"---
institution: cni
rule: 여비지급규칙
article: 제12조
title: 항공운임의 지급
effective: 2026-02-27
amended: 2026-02-27
status: active
supersedes: null
legal_basis: []
refs: []
---
① 항공운임을 지급한다.
"#,
        )
        .unwrap();
        fs::write(
            rule_dir.join("제13조.md"),
            r#"---
institution: cni
rule: 여비지급규칙
article: 제13조
title: 숙박비 지급
effective: 2026-02-27
amended: 2026-02-27
status: active
supersedes: null
legal_basis: []
refs: []
---
① 숙박비를 지급한다.
"#,
        )
        .unwrap();
        fs::write(
            rule_dir.join("제14조.md"),
            r#"---
institution: cni
rule: 여비지급규칙
article: 제14조
title: 이전 규정
effective: 2025-01-01
amended: 2025-01-01
status: active
supersedes: null
legal_basis: []
refs: []
---
① 이전 기준이다.
"#,
        )
        .unwrap();

        let server = PublicRulesServer::from_config(ServerConfig {
            institution: "cni".to_string(),
            pack: PackConfig {
                path: Some(root),
                url: None,
                effective: None,
                source_commit: Some("local-test".to_string()),
            },
            extra_packs: Vec::new(),
            vectors: VectorConfig::default(),
        })
        .unwrap();
        let Json(result) = server.status().await;

        assert_eq!(result.effective_date, "2026-02-27");
        assert_eq!(result.source_commit, "local-test");
    }
}
