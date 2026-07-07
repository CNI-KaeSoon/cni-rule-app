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
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
}

#[derive(Debug, Clone, Copy)]
pub struct TransportArgs {
    pub transport: ServerTransport,
    pub bind_addr: SocketAddr,
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
}

impl PublicRulesServer {
    pub fn new(index: TantivyRulesIndex) -> Self {
        Self {
            index: Arc::new(index),
            tool_router: Self::tool_router(),
        }
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
        let filter = params.rule.map(|rule| RuleFilter {
            rule: Some(rule),
            ..RuleFilter::default()
        });
        Json(SearchRulesResult {
            hits: self
                .index
                .search(&params.query, params.top_k.unwrap_or(5), filter),
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
        let article = self.index.get_article(&params.id);
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
        Json(ListRulesResult {
            rules: self.index.list_rules(),
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
        let article = self.index.get_article(&params.id);
        Json(GetLegalBasisResult {
            basis: article
                .as_ref()
                .map(|article| article.legal_basis.clone())
                .unwrap_or_else(|| self.index.related_laws(&params.id)),
            meta: self.status_meta(article.as_ref()),
        })
    }

    #[tool(name = "status", description = "Return loaded pack freshness status.")]
    pub async fn status(&self) -> Json<StatusResult> {
        Json(StatusResult::from(self.index.status()))
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
    let server = PublicRulesServer::from_config(config)?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

pub async fn run_http_server(config: ServerConfig, bind_addr: SocketAddr) -> anyhow::Result<()> {
    let server = PublicRulesServer::from_config(config)?;
    let cancellation_token = CancellationToken::new();
    let service: StreamableHttpService<PublicRulesServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Default::default(),
            StreamableHttpServerConfig::default()
                .disable_allowed_hosts()
                .with_cancellation_token(cancellation_token.child_token()),
        );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
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
    match transport {
        ServerTransport::Stdio => run_stdio_server(config).await,
        ServerTransport::Http => run_http_server(config, bind_addr).await,
    }
}

pub async fn run_stdio_server_from_toml(toml_text: &str) -> anyhow::Result<()> {
    run_stdio_server(toml::from_str(toml_text)?).await
}

pub fn load_config_arg() -> anyhow::Result<ServerConfig> {
    let args = load_cli_args()?;
    Ok(toml::from_str(&fs::read_to_string(args.config_path)?)?)
}

pub fn load_cli_args() -> anyhow::Result<CliArgs> {
    let mut args = std::env::args().skip(1);
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
            _ => return Err(anyhow::anyhow!("unknown argument: {arg}")),
        }
    }
    let path =
        config_path.ok_or_else(|| anyhow::anyhow!("usage: public-rules-mcp --config <toml>"))?;
    Ok(CliArgs {
        config_path: path,
        transport: transport_args.transport,
        bind_addr: transport_args.bind_addr,
    })
}

pub fn load_transport_args() -> anyhow::Result<TransportArgs> {
    let mut args = std::env::args().skip(1);
    let mut transport_args = TransportArgs::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
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
            _ => return Err(anyhow::anyhow!("unknown argument: {arg}")),
        }
    }
    Ok(transport_args)
}

impl Default for TransportArgs {
    fn default() -> Self {
        Self {
            transport: ServerTransport::Stdio,
            bind_addr: DEFAULT_HTTP_BIND_ADDR
                .parse()
                .expect("default HTTP bind address must be valid"),
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
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid --bind address {value}: {error}"))
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
