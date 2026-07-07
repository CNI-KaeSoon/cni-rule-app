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
    default_pack_status, parse_article_markdown, Article, LegalBasis, PackStatus, RuleFilter,
    RuleSummary, RulesIndex, SearchHit, TantivyRulesIndex,
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
use tokio_util::sync::CancellationToken;

pub const SEARCH_RULES_TOOL: &str = "search_rules";
pub const GET_ARTICLE_TOOL: &str = "get_article";
pub const LIST_RULES_TOOL: &str = "list_rules";
pub const GET_LEGAL_BASIS_TOOL: &str = "get_legal_basis";
pub const STATUS_TOOL: &str = "status";
pub const DEFAULT_HTTP_BIND_ADDR: &str = "127.0.0.1:8787";

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
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PackConfig {
    pub path: Option<PathBuf>,
    pub url: Option<String>,
    pub effective: Option<String>,
    pub source_commit: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct SearchRulesParams {
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub rule: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct SearchRulesResult {
    pub hits: Vec<SearchHit>,
    pub meta: FreshnessMeta,
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
    pub meta: FreshnessMeta,
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
    index: Arc<TantivyRulesIndex>,
    tool_router: ToolRouter<Self>,
    query_logger: Option<QueryLogger>,
}

impl PublicRulesServer {
    pub fn new(index: TantivyRulesIndex) -> Self {
        Self {
            index: Arc::new(index),
            tool_router: Self::tool_router(),
            query_logger: None,
        }
    }

    pub fn with_query_logger(mut self, query_logger: QueryLogger) -> Self {
        self.query_logger = Some(query_logger);
        self
    }

    pub fn from_config(config: ServerConfig) -> anyhow::Result<Self> {
        let institution = config.institution.clone();
        let path = config
            .pack
            .path
            .ok_or_else(|| anyhow::anyhow!("pack.path is required for M0 local stdio server"))?;
        let index = if path.is_file() {
            TantivyRulesIndex::from_pack_archive(path)?
        } else if path.join("manifest.json").is_file() {
            TantivyRulesIndex::from_pack_dir(path)?
        } else {
            let effective = if let Some(effective) = config.pack.effective.clone() {
                effective
            } else {
                most_frequent_effective(&path)?.unwrap_or_else(|| "unknown".to_string())
            };
            let source_commit = config
                .pack
                .source_commit
                .clone()
                .unwrap_or_else(|| "bare-dir".to_string());
            let mut status = default_pack_status(institution, effective);
            status.source_commit = source_commit;
            TantivyRulesIndex::from_articles_dir(path, status)?
        };
        Ok(Self::new(index))
    }

    pub fn from_config_toml(toml_text: &str) -> anyhow::Result<Self> {
        Self::from_config(toml::from_str(toml_text)?)
    }

    fn status_meta(&self, article: Option<&Article>) -> FreshnessMeta {
        let status = self.index.status();
        FreshnessMeta {
            effective: article
                .map(|article| article.effective.clone())
                .unwrap_or_else(|| status.effective_date.clone()),
            amended: article
                .map(|article| article.amended.clone())
                .unwrap_or_else(|| status.effective_date.clone()),
            source_commit: status.source_commit,
            extra: BTreeMap::new(),
        }
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
        let filter = rule.clone().map(|rule| RuleFilter {
            rule: Some(rule),
            ..RuleFilter::default()
        });
        let hits = self.index.search(&query, top_k.unwrap_or(5), filter);
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": SEARCH_RULES_TOOL,
                "query": query.clone(),
                "params": {
                    "query": query.clone(),
                    "top_k": top_k,
                    "rule": rule,
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
        let article = self.index.get_article(&id);
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
        let rules = self.index.list_rules();
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
        let article = self.index.get_article(&id);
        let basis = article
            .as_ref()
            .map(|article| article.legal_basis.clone())
            .unwrap_or_else(|| self.index.related_laws(&id));
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
        let status = StatusResult::from(self.index.status());
        self.log_query(
            started_at,
            serde_json::json!({
                "tool": STATUS_TOOL,
                "params": {},
                "result": {
                    "count": 1,
                },
            }),
        );
        Json(status)
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
            extra: BTreeMap::new(),
        };
        Self {
            institution: status.institution,
            effective_date: status.effective_date,
            source_commit: status.source_commit,
            index_built_at: status.index_built_at,
            stale: status.stale,
            meta,
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(
                    args.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow::anyhow!("--config requires a TOML path"))?,
                );
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

    // §6 계약: "모든 응답 meta에 effective·amended·source_commit 포함(신선도 계약)."

    #[tokio::test]
    async fn search_rules_returns_hits_matching_schema() {
        let server = fixture_server();
        let Json(result) = server
            .search_rules(Parameters(SearchRulesParams {
                query: "항공운임".to_string(),
                top_k: Some(5),
                rule: None,
            }))
            .await;

        assert!(!result.hits.is_empty());
        assert_eq!(result.hits[0].article_id, "여비지급규칙#제12조");
        assert_eq!(result.hits[0].rule, "여비지급규칙");
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
        })
        .unwrap();
        let Json(result) = server.status().await;

        assert_eq!(result.effective_date, "2026-02-27");
        assert_eq!(result.source_commit, "local-test");
    }
}
