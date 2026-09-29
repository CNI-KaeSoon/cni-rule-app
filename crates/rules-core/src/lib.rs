use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TantivyDocument, TextFieldIndexing, TextOptions, Value,
    STORED, STRING,
};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyError, Term};
use time::OffsetDateTime;
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

pub mod eval_metrics;

pub const ARTICLE_ID_SEPARATOR: &str = "#";
pub const DEFAULT_RRF_K: usize = 60;
const VECTOR_CACHE_VERSION: u32 = 1;
const VECTOR_MODEL_ID: &str = "multilingual-e5-small";
const VECTOR_MODEL_REVISION: &str = "fastembed-5:EmbeddingModel::MultilingualE5Small";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum NodeKind {
    Rule,
    Article,
    Annex,
    Institution,
    Dept,
    Position,
    Committee,
    Allowance,
    LawArticle,
    Amendment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EdgeKind {
    Cites,
    ApplyMutatis,
    Delegates,
    ExceptWhen,
    AppliesTo,
    PaymentCondition,
    AmendedBy,
    SupersededBy,
    HasStatus,
    LegalBasis,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub article_id: String,
    pub institution: String,
    pub score: f32,
    pub snippet: String,
    pub rule: String,
    pub title: String,
    pub effective: String,
    #[serde(default = "default_search_hit_kind")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchRouteReport {
    pub hits: Vec<SearchHit>,
    pub pin_hit: Option<SearchHit>,
    pub retrieval_hits: Vec<SearchHit>,
}

pub trait RulesIndex {
    fn search(&self, q: &str, k: usize, filter: Option<RuleFilter>) -> Vec<SearchHit>;
    fn get_article(&self, id: &str) -> Option<Article>;
    fn related_laws(&self, id: &str) -> Vec<LegalBasis>;
    fn impact(&self, id: &str) -> ImpactReport;
    fn list_rules(&self) -> Vec<RuleSummary>;
    fn status(&self) -> PackStatus;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleFilter {
    pub institution: Option<String>,
    pub rule: Option<String>,
    pub status: Option<String>,
    pub effective: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Article {
    pub id: String,
    pub institution: String,
    pub rule: String,
    pub article: String,
    pub title: String,
    pub effective: String,
    pub amended: String,
    pub status: String,
    pub body: String,
    pub legal_basis: Vec<LegalBasis>,
    pub refs: Vec<ArticleRef>,
    pub prev_id: Option<String>,
    pub next_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annex_refs: Vec<String>,
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Annex {
    pub id: String,
    pub institution: String,
    pub rule: String,
    pub annex: String,
    pub title: String,
    pub effective: String,
    pub status: String,
    pub body: String,
    pub table_structured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<u32>,
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SourcePage {
    pub institution: String,
    pub page: u32,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArticleRef {
    pub target: String,
    #[serde(rename = "type", alias = "kind")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LegalBasis {
    pub law: String,
    pub article: String,
    pub mst: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImpactReport {
    pub article_id: String,
    pub reverse_citations: Vec<String>,
    pub delegation_chain: Vec<String>,
    pub affected_articles: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuleSummary {
    pub slug: String,
    pub name: String,
    pub article_count: usize,
    pub effective: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PackStatus {
    pub institution: String,
    pub effective_date: String,
    pub source_commit: String,
    pub index_built_at: String,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RulesCoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tantivy error: {0}")]
    Tantivy(#[from] TantivyError),
    #[error("invalid article markdown: {0}")]
    InvalidArticle(String),
    #[error("manifest digest mismatch for {path}: expected {expected}, got {actual}")]
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("missing manifest entry for {0}")]
    MissingManifestEntry(String),
    #[error("unsupported pack schema_version: {0}")]
    UnsupportedSchema(u32),
    #[error("unsafe pack path: {0}")]
    UnsafePackPath(String),
    #[error("pack contains unlisted file: {0}")]
    UnlistedPackFile(String),
    #[error("unsupported archive entry: {0}")]
    UnsupportedArchiveEntry(String),
}

pub type Result<T> = std::result::Result<T, RulesCoreError>;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct PackManifest {
    pub schema_version: u32,
    pub institution: String,
    pub effective_date: String,
    pub source_commit: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<serde_json::Value>,
    pub files: BTreeMap<String, String>,
}

impl PackManifest {
    pub fn normalize_hashes(&mut self) {
        for digest in self.files.values_mut() {
            *digest = digest.to_ascii_lowercase();
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,
    #[serde(default)]
    pub meta: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphEdge {
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
    #[serde(default)]
    pub meta: BTreeMap<String, serde_json::Value>,
}

pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorSearchOptions {
    pub enabled: bool,
    pub cache_dir: Option<PathBuf>,
    pub model_dir: Option<PathBuf>,
    pub rrf_k: usize,
    pub vector_weight: f32,
}

/// Search-time behaviour flags shared by all packs (PRD 2.3). Every field
/// defaults to the legacy behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SearchOptions {
    /// Apply the e5 model-card `passage:` / `query:` prefixes to both sides.
    pub e5_prefix: bool,
    /// Part-of-speech filter / normalisation of the BM25 `ko` analyzer.
    pub ko_pos_filter: KoPosFilter,
    /// Deterministic metadata context prepended to passages (PH-07).
    pub context_prefix: ContextPrefix,
    /// RRF weight of the character-bigram list (PH-08). `0.0` (default) means
    /// the `body_ng` field is not indexed and the list is not computed.
    pub bigram_weight: f32,
}

/// Allowed range of `search.bigram_weight`.
pub const BIGRAM_WEIGHT_MAX: f32 = 2.0;

/// Validates a `bigram_weight` value (`0.0..=2.0`, finite).
pub fn validate_bigram_weight(weight: f32) -> std::result::Result<f32, String> {
    if weight.is_finite() && (0.0..=BIGRAM_WEIGHT_MAX).contains(&weight) {
        Ok(weight)
    } else {
        Err(format!(
            "bigram_weight must be within 0.0..=2.0, got {weight}"
        ))
    }
}

impl SearchOptions {
    /// Whether the bigram field is indexed and the list is computed.
    pub fn bigram_enabled(&self) -> bool {
        self.bigram_weight > 0.0
    }

    pub fn passage_format(&self) -> PassageFormat {
        match (self.e5_prefix, self.context_prefix != ContextPrefix::None) {
            (false, false) => PassageFormat::Legacy,
            (true, false) => PassageFormat::E5Prefix,
            (false, true) => PassageFormat::Ctx1,
            (true, true) => PassageFormat::E5PrefixCtx1,
        }
    }
}

/// Where the deterministic article/annex context string is injected (PH-07).
/// `Vector` = embedding passages only; `Both` = passages and the BM25 `body`
/// field. `None` is the legacy behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextPrefix {
    #[default]
    None,
    Vector,
    Both,
}

impl ContextPrefix {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextPrefix::None => "none",
            ContextPrefix::Vector => "vector",
            ContextPrefix::Both => "both",
        }
    }
}

/// BM25 analyzer variants (PRD PH-06). `None` is the legacy token stream.
/// `V1` = NFKC + lowercase + particle/symbol stop tags; `V2` = `V1` plus
/// verbal endings. Matching is exact on the first ko-dic feature, so compound
/// tags such as `VV+EC` are intentionally kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KoPosFilter {
    #[default]
    None,
    V1,
    V2,
}

/// Stop tags of `V1`: case/particle markers and punctuation-like symbols.
const KO_STOP_TAGS_V1: [&str; 15] = [
    "JKS", "JKC", "JKG", "JKO", "JKB", "JKV", "JKQ", "JX", "JC", "SF", "SE", "SSO", "SSC", "SC",
    "SY",
];
/// Additional stop tags of `V2`: pre-final, final, connective and
/// nominalising/adnominal endings.
const KO_STOP_TAGS_V2_EXTRA: [&str; 5] = ["EP", "EF", "EC", "ETN", "ETM"];

impl KoPosFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            KoPosFilter::None => "none",
            KoPosFilter::V1 => "v1",
            KoPosFilter::V2 => "v2",
        }
    }

    /// Exact tag set removed by this variant (empty for `None`).
    pub fn stop_tags(self) -> Vec<&'static str> {
        match self {
            KoPosFilter::None => Vec::new(),
            KoPosFilter::V1 => KO_STOP_TAGS_V1.to_vec(),
            KoPosFilter::V2 => KO_STOP_TAGS_V1
                .iter()
                .chain(KO_STOP_TAGS_V2_EXTRA.iter())
                .copied()
                .collect(),
        }
    }
}

/// Which analyzer the BM25 index really uses; a dictionary load failure is
/// reported here instead of silently changing behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenizerStatus {
    /// `lindera-ko-dic` or `simple-fallback`.
    pub kind: String,
    /// Filter variant actually applied (`none` when the fallback is active).
    pub filter: String,
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How passage text is rendered before embedding. `Legacy` keeps the original
/// text and the original cache key bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PassageFormat {
    #[default]
    Legacy,
    E5Prefix,
    /// Metadata context line before the passage (`ctx1`).
    Ctx1,
    /// `passage: ` prefix plus the context line (`e5p+ctx1`).
    E5PrefixCtx1,
}

impl PassageFormat {
    /// Value stored in the cache key; `None` for legacy so the key is unchanged.
    fn cache_tag(self) -> Option<&'static str> {
        match self {
            PassageFormat::Legacy => None,
            PassageFormat::E5Prefix => Some("e5p"),
            PassageFormat::Ctx1 => Some("ctx1"),
            PassageFormat::E5PrefixCtx1 => Some("e5p+ctx1"),
        }
    }

    fn has_e5_prefix(self) -> bool {
        matches!(self, PassageFormat::E5Prefix | PassageFormat::E5PrefixCtx1)
    }

    fn has_context(self) -> bool {
        matches!(self, PassageFormat::Ctx1 | PassageFormat::E5PrefixCtx1)
    }

    fn passage_text(self, text: String) -> String {
        if self.has_e5_prefix() {
            format!("passage: {text}")
        } else {
            text
        }
    }

    fn query_text(self, q: &str) -> std::borrow::Cow<'_, str> {
        if self.has_e5_prefix() {
            std::borrow::Cow::Owned(format!("query: {q}"))
        } else {
            std::borrow::Cow::Borrowed(q)
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VectorStatus {
    pub enabled: bool,
    pub model_ready: bool,
}

impl Default for VectorSearchOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_dir: None,
            model_dir: None,
            rrf_k: DEFAULT_RRF_K,
            vector_weight: 1.0,
        }
    }
}

impl VectorSearchOptions {
    pub fn enabled(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            enabled: true,
            cache_dir: Some(cache_dir.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopEmbeddingProvider;

impl EmbeddingProvider for NoopEmbeddingProvider {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(Vec::new())
    }
}

#[cfg(feature = "vectors")]
pub struct FastEmbedEmbeddingProvider {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "vectors")]
impl FastEmbedEmbeddingProvider {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_model_dir(None)
    }

    pub fn with_model_dir(model_dir: Option<&Path>) -> anyhow::Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

        let mut options = InitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_show_download_progress(false);
        if let Some(model_dir) = model_dir {
            options = options.with_cache_dir(model_dir.to_path_buf());
        }
        let model = TextEmbedding::try_new(options)?;
        Ok(Self {
            model: std::sync::Mutex::new(model),
        })
    }
}

#[cfg(feature = "vectors")]
impl EmbeddingProvider for FastEmbedEmbeddingProvider {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let embeddings = self
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("fastembed model mutex poisoned"))?
            .embed(vec![text], None)?;
        Ok(embeddings.into_iter().next().unwrap_or_default())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ArticleFrontmatter {
    institution: String,
    rule: String,
    article: String,
    title: String,
    effective: String,
    amended: String,
    status: String,
    #[serde(default)]
    supersedes: Option<String>,
    #[serde(default)]
    legal_basis: Vec<LegalBasis>,
    #[serde(default)]
    refs: Vec<ArticleRef>,
    #[serde(default)]
    pages: Vec<u32>,
    #[serde(default)]
    source_pages: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnnexFrontmatter {
    institution: String,
    rule: String,
    annex: String,
    title: String,
    effective: String,
    status: String,
    #[serde(default)]
    pages: Vec<u32>,
    #[serde(default)]
    source_pages: Vec<u32>,
    #[serde(default)]
    table_structured: bool,
}

#[derive(Debug, Clone)]
struct SearchFields {
    id: Field,
    rule: Field,
    title: Field,
    body: Field,
    /// Character-bigram field (PH-08); present only when `bigram_weight > 0`.
    body_ng: Option<Field>,
}

#[derive(Clone)]
struct SearchReader(IndexReader);

impl std::fmt::Debug for SearchReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SearchReader")
    }
}

#[derive(Debug, Clone)]
struct VectorCorpus {
    entries: Vec<VectorEntry>,
    /// `sqrt(sum(e*e))` per entry, precomputed with the same accumulation order as
    /// `cosine_similarity` so scores stay bit-identical.
    norms: Vec<f32>,
}

impl VectorCorpus {
    fn new(entries: Vec<VectorEntry>) -> Self {
        let norms = entries
            .iter()
            .map(|entry| squared_norm(&entry.embedding).sqrt())
            .collect();
        Self { entries, norms }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VectorEntry {
    id: String,
    kind: String,
    text_hash: String,
    embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VectorCacheFile {
    version: u32,
    model: String,
    cache_key: String,
    entries: Vec<VectorEntry>,
}

#[derive(Clone)]
struct StoredEmbeddingProvider(std::sync::Arc<dyn EmbeddingProvider>);

impl std::fmt::Debug for StoredEmbeddingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoredEmbeddingProvider")
    }
}

#[derive(Debug)]
pub struct TantivyRulesIndex {
    articles: BTreeMap<String, Article>,
    annexes: BTreeMap<String, Annex>,
    pages: Option<BTreeMap<u32, String>>,
    page_owners: BTreeMap<u32, Vec<String>>,
    summaries: Vec<RuleSummary>,
    status: PackStatus,
    graph: DiGraph<String, EdgeKind>,
    node_indices: HashMap<String, NodeIndex>,
    index: Index,
    reader: SearchReader,
    fields: SearchFields,
    vector_corpus: Option<VectorCorpus>,
    vector_provider: Option<StoredEmbeddingProvider>,
    vector_enabled_requested: bool,
    rrf_k: usize,
    vector_weight: f32,
    search_options: SearchOptions,
    tokenizer_status: TokenizerStatus,
    /// Deterministic article/annex context strings (empty unless
    /// `context_prefix` is on).
    contexts: BTreeMap<String, String>,
}

impl TantivyRulesIndex {
    pub fn from_articles<I>(articles: I, status: PackStatus) -> Result<Self>
    where
        I: IntoIterator<Item = Article>,
    {
        Self::from_articles_with_options(articles, status, SearchOptions::default())
    }

    pub fn from_articles_with_options<I>(
        articles: I,
        status: PackStatus,
        search_options: SearchOptions,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = Article>,
    {
        let articles = articles.into_iter().map(|a| (a.id.clone(), a)).collect();
        Self::assemble(
            articles,
            BTreeMap::new(),
            None,
            &[],
            &[],
            status,
            &VectorSearchOptions::default(),
            search_options,
            false,
        )
    }

    /// Analyzer actually used by the BM25 index.
    pub fn tokenizer_status(&self) -> &TokenizerStatus {
        &self.tokenizer_status
    }

    /// Test hook (like `enable_vectors_for_test`): pretend the analyzer degraded.
    #[doc(hidden)]
    pub fn set_tokenizer_status_for_test(&mut self, status: TokenizerStatus) {
        self.tokenizer_status = status;
    }

    /// Builds the search index, summaries and graph exactly once.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        mut articles: BTreeMap<String, Article>,
        annexes: BTreeMap<String, Annex>,
        pages: Option<BTreeMap<u32, String>>,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
        status: PackStatus,
        opts: &VectorSearchOptions,
        search_options: SearchOptions,
        with_page_owners: bool,
    ) -> Result<Self> {
        link_neighbors(&mut articles);
        let (graph, node_indices) = build_ref_graph(articles.values(), nodes, edges);
        let contexts = if search_options.context_prefix == ContextPrefix::None {
            BTreeMap::new()
        } else {
            build_contexts(&articles, &annexes, &graph, &node_indices, nodes)
        };
        let bm25_contexts =
            (search_options.context_prefix == ContextPrefix::Both).then_some(&contexts);
        let (index, fields, tokenizer_status) = build_search_index(
            articles.values(),
            annexes.values(),
            search_options.ko_pos_filter,
            bm25_contexts,
            search_options.bigram_enabled(),
        )?;
        let reader = SearchReader(
            index
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()?,
        );
        let summaries = build_rule_summaries(articles.values());
        let page_owners = if with_page_owners {
            build_page_owners(articles.values(), annexes.values())
        } else {
            BTreeMap::new()
        };

        Ok(Self {
            articles,
            annexes,
            pages,
            page_owners,
            summaries,
            status,
            graph,
            node_indices,
            index,
            reader,
            fields,
            vector_corpus: None,
            vector_provider: None,
            vector_enabled_requested: opts.enabled,
            rrf_k: opts.rrf_k,
            vector_weight: opts.vector_weight,
            search_options,
            tokenizer_status,
            contexts,
        })
    }

    pub fn from_articles_dir(path: impl AsRef<Path>, status: PackStatus) -> Result<Self> {
        let articles = load_articles_dir(path)?;
        Self::from_articles(articles, status)
    }

    pub fn enable_vectors_for_test<P: EmbeddingProvider + 'static>(
        &mut self,
        provider: P,
        cache_key: &str,
        cache_dir: Option<&Path>,
        rrf_k: usize,
        vector_weight: f32,
    ) -> anyhow::Result<()> {
        self.rrf_k = rrf_k;
        self.vector_weight = vector_weight;
        self.vector_enabled_requested = true;
        let provider = std::sync::Arc::new(provider);
        self.vector_corpus = Some(build_or_load_vector_corpus(
            self.articles.values(),
            self.annexes.values(),
            provider.as_ref(),
            cache_key,
            cache_dir,
            self.search_options.passage_format(),
            &self.contexts,
        )?);
        self.vector_provider = Some(StoredEmbeddingProvider(provider));
        Ok(())
    }

    pub fn from_pack_dir(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack_dir_with_vector_options(path, VectorSearchOptions::default())
    }

    pub fn from_pack_dir_with_vector_options(
        path: impl AsRef<Path>,
        vector_options: VectorSearchOptions,
    ) -> Result<Self> {
        Self::from_pack_dir_with_options(path, vector_options, SearchOptions::default())
    }

    pub fn from_pack_dir_with_options(
        path: impl AsRef<Path>,
        vector_options: VectorSearchOptions,
        search_options: SearchOptions,
    ) -> Result<Self> {
        let path = path.as_ref();
        let mut manifest: PackManifest =
            serde_json::from_reader(File::open(path.join("manifest.json"))?)?;
        manifest.normalize_hashes();
        verify_manifest(path, &manifest)?;
        let cache_key = pack_vector_cache_key(&manifest, search_options.passage_format());

        let articles = load_articles_dir(path.join("articles"))?;
        let annexes = load_annexes_dir(path.join("annexes"))?;
        let pages = load_pages_dir(path.join("pages"))?;
        let nodes = load_jsonl::<GraphNode>(&path.join("graph/nodes.jsonl"))?;
        let edges = load_jsonl::<GraphEdge>(&path.join("graph/edges.jsonl"))?;
        let status = PackStatus {
            institution: manifest.institution,
            effective_date: manifest.effective_date,
            source_commit: manifest.source_commit,
            index_built_at: manifest.created_at,
            stale: false,
            source_url: manifest.source_url,
        };

        let mut index = Self::assemble(
            articles.into_iter().map(|a| (a.id.clone(), a)).collect(),
            annexes.into_iter().map(|a| (a.id.clone(), a)).collect(),
            pages,
            &nodes,
            &edges,
            status,
            &vector_options,
            search_options,
            true,
        )?;
        if let Some((corpus, provider)) =
            index.try_build_vector_corpus_from_options(&cache_key, &vector_options)
        {
            index.vector_corpus = Some(corpus);
            index.vector_provider = Some(provider);
        }
        Ok(index)
    }

    pub fn from_pack_archive(path: impl AsRef<Path>) -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        unpack_pack_archive(path, tmp.path())?;
        Self::from_pack_dir(tmp.path())
    }

    pub fn from_pack_archive_with_vector_options(
        path: impl AsRef<Path>,
        vector_options: VectorSearchOptions,
    ) -> Result<Self> {
        Self::from_pack_archive_with_options(path, vector_options, SearchOptions::default())
    }

    pub fn from_pack_archive_with_options(
        path: impl AsRef<Path>,
        vector_options: VectorSearchOptions,
        search_options: SearchOptions,
    ) -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        unpack_pack_archive(path, tmp.path())?;
        Self::from_pack_dir_with_options(tmp.path(), vector_options, search_options)
    }

    pub fn search_with_routes(
        &self,
        q: &str,
        k: usize,
        filter: Option<RuleFilter>,
    ) -> SearchRouteReport {
        if q.trim().is_empty() || k == 0 {
            return SearchRouteReport::default();
        }
        let pinned = self.direct_article_hit(q, filter.as_ref());
        let retrieval_hits = self.search_retrieval(q, k, filter.as_ref());
        let hits = merge_pinned_hits(pinned.clone(), retrieval_hits.clone(), k);
        SearchRouteReport {
            hits,
            pin_hit: pinned,
            retrieval_hits,
        }
    }

    fn search_retrieval(&self, q: &str, k: usize, filter: Option<&RuleFilter>) -> Vec<SearchHit> {
        let query_terms = query_terms(q);
        let normalized_query = query_terms.join(" ");
        let parser_query = if normalized_query.is_empty() {
            q
        } else {
            normalized_query.as_str()
        };
        let mut parser =
            QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
        parser.set_field_boost(self.fields.title, 2.0);
        let (query, _errors) = parser.parse_query_lenient(parser_query);
        let searcher = self.reader.0.searcher();
        let candidate_limit = k * 4;
        let Ok(top_docs) = searcher.search(&query, &TopDocs::with_limit(candidate_limit)) else {
            return lexical_fallback(self.articles.values(), q, k, filter);
        };

        let mut bm25_hits = Vec::new();
        for (score, addr) in top_docs {
            let Ok(doc) = searcher.doc::<TantivyDocument>(addr) else {
                continue;
            };
            let Some(id) = first_text(&doc, self.fields.id) else {
                continue;
            };
            let Some(hit) = self.search_hit_for_id(id, score, q, filter) else {
                continue;
            };
            bm25_hits.push(hit);
        }
        sort_hits_by_score_and_id(&mut bm25_hits);

        let mut rule_hits = Vec::new();
        let rule_terms = rule_query_terms(&query_terms);
        if !rule_terms.is_empty() {
            let mut rule_parser = QueryParser::for_index(&self.index, vec![self.fields.rule]);
            rule_parser.set_field_boost(self.fields.rule, 3.0);
            let (rule_query, _errors) = rule_parser.parse_query_lenient(&rule_terms.join(" "));
            if let Ok(top_docs) =
                searcher.search(&rule_query, &TopDocs::with_limit(candidate_limit))
            {
                for (score, addr) in top_docs {
                    let Ok(doc) = searcher.doc::<TantivyDocument>(addr) else {
                        continue;
                    };
                    let Some(id) = first_text(&doc, self.fields.id) else {
                        continue;
                    };
                    let Some(hit) = self.search_hit_for_id(id, score, q, filter) else {
                        continue;
                    };
                    rule_hits.push(hit);
                }
                sort_hits_by_score_and_id(&mut rule_hits);
            }
        }

        let lexical_hits = lexical_rank(
            self.articles.values(),
            self.annexes.values(),
            q,
            &query_terms,
            candidate_limit,
            filter,
        );
        let annex_ref_hits = self.annex_reference_rank(q, candidate_limit, filter);
        let vector_hits = self.vector_rank(q, candidate_limit, filter);
        let bigram_hits = self.bigram_rank(q, candidate_limit, filter);
        let mut rankings = Vec::new();
        let mut weights = Vec::new();
        rankings.push(annex_ref_hits);
        weights.push(1.0);
        if !bm25_hits.is_empty() {
            rankings.push(bm25_hits);
            weights.push(1.0);
        }
        if !vector_hits.is_empty() {
            rankings.push(vector_hits);
            weights.push(self.vector_weight);
        }
        rankings.push(rule_hits);
        weights.push(1.0);
        rankings.push(lexical_hits);
        weights.push(1.0);
        if !bigram_hits.is_empty() {
            rankings.push(bigram_hits);
            weights.push(self.search_options.bigram_weight);
        }
        rrf_fuse_weighted(rankings, k, self.rrf_k, &weights)
    }

    /// BM25 ranking over the character-bigram field (PH-08). Empty unless the
    /// field was indexed (`bigram_weight > 0`). The query is compacted the same
    /// way as the indexed text and turned into a bag of bigram terms; a
    /// `QueryParser` would build a phrase query out of them (bigram tokens all
    /// share position 0), which cannot match.
    fn bigram_rank(&self, q: &str, limit: usize, filter: Option<&RuleFilter>) -> Vec<SearchHit> {
        let Some(field) = self.fields.body_ng else {
            return Vec::new();
        };
        if !self.search_options.bigram_enabled() {
            return Vec::new();
        }
        let compact = normalize_compact(q);
        if compact.is_empty() {
            return Vec::new();
        }
        let Some(mut analyzer) = self.index.tokenizers().get(BIGRAM_TOKENIZER) else {
            return Vec::new();
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut stream = analyzer.token_stream(&compact);
        stream.process(&mut |token| {
            if seen.insert(token.text.clone()) {
                clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, &token.text),
                        IndexRecordOption::WithFreqs,
                    )),
                ));
            }
        });
        if clauses.is_empty() {
            return Vec::new();
        }
        let query = BooleanQuery::new(clauses);
        let searcher = self.reader.0.searcher();
        let Ok(top_docs) = searcher.search(&query, &TopDocs::with_limit(limit)) else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        for (score, addr) in top_docs {
            let Ok(doc) = searcher.doc::<TantivyDocument>(addr) else {
                continue;
            };
            let Some(id) = first_text(&doc, self.fields.id) else {
                continue;
            };
            if let Some(hit) = self.search_hit_for_id(id, score, q, filter) {
                hits.push(hit);
            }
        }
        sort_hits_by_score_and_id(&mut hits);
        hits
    }
}

impl RulesIndex for TantivyRulesIndex {
    fn search(&self, q: &str, k: usize, filter: Option<RuleFilter>) -> Vec<SearchHit> {
        self.search_with_routes(q, k, filter).hits
    }

    fn get_article(&self, id: &str) -> Option<Article> {
        self.articles.get(id).cloned()
    }

    fn related_laws(&self, id: &str) -> Vec<LegalBasis> {
        self.articles
            .get(id)
            .map(|article| article.legal_basis.clone())
            .unwrap_or_default()
    }

    fn impact(&self, id: &str) -> ImpactReport {
        let reverse_citations = self.reverse_neighbors(id, |kind| {
            matches!(
                kind,
                EdgeKind::Cites
                    | EdgeKind::ApplyMutatis
                    | EdgeKind::Delegates
                    | EdgeKind::LegalBasis
            )
        });
        let delegation_chain = self.forward_traversal(id, |kind| {
            matches!(kind, EdgeKind::Delegates | EdgeKind::ApplyMutatis)
        });

        let mut affected = BTreeSet::new();
        affected.extend(reverse_citations.iter().cloned());
        affected.extend(delegation_chain.iter().cloned());

        ImpactReport {
            article_id: id.to_string(),
            reverse_citations,
            delegation_chain,
            affected_articles: affected.into_iter().collect(),
        }
    }

    fn list_rules(&self) -> Vec<RuleSummary> {
        self.summaries.clone()
    }

    fn status(&self) -> PackStatus {
        self.status.clone()
    }
}

impl TantivyRulesIndex {
    fn search_hit_for_id(
        &self,
        id: &str,
        score: f32,
        q: &str,
        filter: Option<&RuleFilter>,
    ) -> Option<SearchHit> {
        if let Some(article) = self.articles.get(id) {
            if !matches_filter(article, filter) {
                return None;
            }
            return Some(SearchHit {
                article_id: article.id.clone(),
                institution: article.institution.clone(),
                score,
                snippet: snippet(&article.body, q),
                rule: article.rule.clone(),
                title: article.title.clone(),
                effective: article.effective.clone(),
                kind: "article".to_string(),
            });
        }
        let annex = self.annexes.get(id)?;
        if !matches_annex_filter(annex, filter) {
            return None;
        }
        Some(search_hit_for_annex(annex, score, q))
    }

    pub fn get_annex(&self, id: &str) -> Option<Annex> {
        self.annexes.get(id).cloned()
    }

    pub fn get_source_page(&self, page: u32) -> Option<SourcePage> {
        let pages = self.pages.as_ref()?;
        let text = pages.get(&page)?.clone();
        Some(SourcePage {
            institution: self.status.institution.clone(),
            page,
            text,
            owner_ids: self.page_owners.get(&page).cloned().unwrap_or_default(),
        })
    }

    pub fn has_source_pages(&self) -> bool {
        self.pages.is_some()
    }

    pub fn vector_status(&self) -> VectorStatus {
        VectorStatus {
            enabled: self.vector_enabled_requested,
            model_ready: self.vector_provider.is_some() && self.vector_corpus.is_some(),
        }
    }

    pub fn set_vector_enabled_requested(&mut self, enabled: bool) {
        self.vector_enabled_requested = enabled;
    }

    fn direct_article_hit(&self, q: &str, filter: Option<&RuleFilter>) -> Option<SearchHit> {
        if let Some((rule_slug, article)) =
            direct_article_ref(q, filter.and_then(|f| f.rule.as_deref()))
        {
            let id = format!("{rule_slug}{ARTICLE_ID_SEPARATOR}{article}");
            let article = self.articles.get(&id)?;
            if !matches_filter(article, filter) {
                return None;
            }
            return Some(SearchHit {
                article_id: article.id.clone(),
                institution: article.institution.clone(),
                score: f32::MAX,
                snippet: snippet(&article.body, q),
                rule: article.rule.clone(),
                title: article.title.clone(),
                effective: article.effective.clone(),
                kind: "article".to_string(),
            });
        }
        self.direct_annex_hit(q, filter)
    }

    fn direct_annex_hit(&self, q: &str, filter: Option<&RuleFilter>) -> Option<SearchHit> {
        let (annex_start, _, annex_ref) = find_annex_ref(q)?;
        let fallback_rule = filter.and_then(|f| f.rule.as_deref());
        let annex = if let Some(rule) = fallback_rule {
            let id = format!(
                "{}{}{}",
                slugify_rule(rule),
                ARTICLE_ID_SEPARATOR,
                annex_ref
            );
            self.annexes.get(&id)?
        } else {
            let prefix = normalize_compact(&q[..annex_start]);
            let mut candidates = self
                .annexes
                .values()
                .filter(|annex| annex.annex == annex_ref && matches_annex_filter(annex, filter))
                .filter(|annex| {
                    prefix.contains(normalize_compact(&annex.rule).as_str())
                        || prefix.contains(slugify_rule(&annex.rule).as_str())
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| a.id.cmp(&b.id));
            candidates.into_iter().next()?
        };
        if !matches_annex_filter(annex, filter) {
            return None;
        }
        Some(search_hit_for_annex(annex, f32::MAX, q))
    }

    fn annex_reference_rank(
        &self,
        q: &str,
        k: usize,
        filter: Option<&RuleFilter>,
    ) -> Vec<SearchHit> {
        let Some((annex_start, _, annex_ref)) = find_annex_ref(q) else {
            return Vec::new();
        };
        let prefix = normalize_compact(&q[..annex_start]);
        let mut hits = self
            .annexes
            .values()
            .filter(|annex| annex.annex == annex_ref && matches_annex_filter(annex, filter))
            .map(|annex| {
                let mut hit = search_hit_for_annex(annex, 100.0, q);
                if prefix.contains(normalize_compact(&annex.rule).as_str())
                    || prefix.contains(slugify_rule(&annex.rule).as_str())
                {
                    hit.score += 10.0;
                }
                hit
            })
            .collect::<Vec<_>>();
        sort_hits_by_score_and_id(&mut hits);
        hits.truncate(k);
        hits
    }

    fn vector_rank(&self, q: &str, k: usize, filter: Option<&RuleFilter>) -> Vec<SearchHit> {
        let Some(corpus) = &self.vector_corpus else {
            return Vec::new();
        };
        let Some(provider) = &self.vector_provider else {
            return Vec::new();
        };
        let query_text = self.search_options.passage_format().query_text(q);
        let Ok(query) = provider.0.embed(&query_text) else {
            eprintln!("vector search query embedding failed; falling back for this query");
            return Vec::new();
        };
        if query.is_empty() {
            return Vec::new();
        }
        let mut hits = corpus
            .entries
            .iter()
            .zip(corpus.norms.iter())
            .filter_map(|(entry, &norm)| {
                let score = cosine_similarity_with_norm(&query, &entry.embedding, norm)?;
                self.vector_hit_for_id(&entry.id, score, q, filter)
            })
            .collect::<Vec<_>>();
        sort_hits_by_score_and_id(&mut hits);
        hits.truncate(k);
        hits
    }

    fn vector_hit_for_id(
        &self,
        id: &str,
        score: f32,
        q: &str,
        filter: Option<&RuleFilter>,
    ) -> Option<SearchHit> {
        if let Some(article) = self.articles.get(id) {
            if !matches_filter(article, filter) {
                return None;
            }
            return Some(SearchHit {
                article_id: article.id.clone(),
                institution: article.institution.clone(),
                score,
                snippet: snippet(&article.body, q),
                rule: article.rule.clone(),
                title: article.title.clone(),
                effective: article.effective.clone(),
                kind: "article".to_string(),
            });
        }
        let annex = self.annexes.get(id)?;
        if !matches_annex_filter(annex, filter) {
            return None;
        }
        Some(search_hit_for_annex(annex, score, q))
    }

    fn try_build_vector_corpus_from_options(
        &self,
        cache_key: &str,
        options: &VectorSearchOptions,
    ) -> Option<(VectorCorpus, StoredEmbeddingProvider)> {
        if !options.enabled {
            return None;
        }
        let Some(cache_dir) = options.cache_dir.as_deref() else {
            eprintln!("vector search disabled: cache_dir is required");
            return None;
        };
        match self.build_vector_corpus_with_fastembed(cache_key, cache_dir, options) {
            Ok(result) => Some(result),
            Err(error) => {
                eprintln!("vector search disabled: {error}");
                None
            }
        }
    }

    #[cfg(feature = "vectors")]
    fn build_vector_corpus_with_fastembed(
        &self,
        cache_key: &str,
        cache_dir: &Path,
        options: &VectorSearchOptions,
    ) -> anyhow::Result<(VectorCorpus, StoredEmbeddingProvider)> {
        let env_model_dir = std::env::var_os("CNI_RULES_FASTEMBED_MODEL_DIR").map(PathBuf::from);
        let model_dir = options.model_dir.as_deref().or(env_model_dir.as_deref());
        let provider = std::sync::Arc::new(FastEmbedEmbeddingProvider::with_model_dir(model_dir)?);
        let corpus = build_or_load_vector_corpus(
            self.articles.values(),
            self.annexes.values(),
            provider.as_ref(),
            cache_key,
            Some(cache_dir),
            self.search_options.passage_format(),
            &self.contexts,
        )?;
        Ok((corpus, StoredEmbeddingProvider(provider)))
    }

    #[cfg(not(feature = "vectors"))]
    fn build_vector_corpus_with_fastembed(
        &self,
        _cache_key: &str,
        _cache_dir: &Path,
        _options: &VectorSearchOptions,
    ) -> anyhow::Result<(VectorCorpus, StoredEmbeddingProvider)> {
        anyhow::bail!("rules-core was built without the vectors feature")
    }

    fn reverse_neighbors(&self, id: &str, accept: impl Fn(EdgeKind) -> bool) -> Vec<String> {
        reverse_neighbors_in(&self.graph, &self.node_indices, id, accept)
    }

    fn forward_traversal(&self, id: &str, accept: impl Fn(EdgeKind) -> bool) -> Vec<String> {
        let Some(start) = self.node_indices.get(id).copied() else {
            return Vec::new();
        };
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([start]);
        while let Some(node) = queue.pop_front() {
            for edge in self.graph.edges(node) {
                if !accept(*edge.weight()) {
                    continue;
                }
                let next = edge.target();
                let next_id = self.graph[next].clone();
                if seen.insert(next_id) {
                    queue.push_back(next);
                }
            }
        }
        seen.remove(id);
        seen.into_iter().collect()
    }
}

pub fn parse_article_markdown(path: impl AsRef<Path>) -> Result<Article> {
    parse_article_markdown_str(&fs::read_to_string(path.as_ref())?)
}

pub fn parse_article_markdown_str(input: &str) -> Result<Article> {
    let Some(rest) = input.strip_prefix("---\n") else {
        return Err(RulesCoreError::InvalidArticle(
            "missing YAML frontmatter".to_string(),
        ));
    };
    let Some((frontmatter, body)) = rest.split_once("\n---") else {
        return Err(RulesCoreError::InvalidArticle(
            "unterminated YAML frontmatter".to_string(),
        ));
    };
    let frontmatter: ArticleFrontmatter = serde_yaml::from_str(frontmatter)?;
    let body = body.trim_start_matches('\n').to_string();
    let id = format!(
        "{}{}{}",
        slugify_rule(&frontmatter.rule),
        ARTICLE_ID_SEPARATOR,
        frontmatter.article
    );
    let mut meta = BTreeMap::new();
    if let Some(supersedes) = frontmatter.supersedes {
        meta.insert("supersedes".to_string(), supersedes);
    }
    let pages = merged_pages(frontmatter.pages, frontmatter.source_pages);
    if !pages.is_empty() {
        meta.insert("pages".to_string(), page_range_string(&pages));
    }
    let annex_refs = frontmatter
        .refs
        .iter()
        .filter(|reference| is_annex_id(&reference.target))
        .map(|reference| reference.target.clone())
        .collect::<Vec<_>>();

    Ok(Article {
        id,
        institution: frontmatter.institution,
        rule: frontmatter.rule,
        article: frontmatter.article,
        title: frontmatter.title,
        effective: frontmatter.effective,
        amended: frontmatter.amended,
        status: frontmatter.status,
        body,
        legal_basis: frontmatter.legal_basis,
        refs: frontmatter.refs,
        prev_id: None,
        next_id: None,
        pages,
        annex_refs,
        meta,
    })
}

pub fn parse_annex_markdown(path: impl AsRef<Path>) -> Result<Annex> {
    parse_annex_markdown_str(&fs::read_to_string(path.as_ref())?)
}

pub fn parse_annex_markdown_str(input: &str) -> Result<Annex> {
    let Some(rest) = input.strip_prefix("---\n") else {
        return Err(RulesCoreError::InvalidArticle(
            "missing YAML frontmatter".to_string(),
        ));
    };
    let Some((frontmatter, body)) = rest.split_once("\n---") else {
        return Err(RulesCoreError::InvalidArticle(
            "unterminated YAML frontmatter".to_string(),
        ));
    };
    let frontmatter: AnnexFrontmatter = serde_yaml::from_str(frontmatter)?;
    let body = body.trim_start_matches('\n').to_string();
    let id = format!(
        "{}{}{}",
        slugify_rule(&frontmatter.rule),
        ARTICLE_ID_SEPARATOR,
        frontmatter.annex
    );
    let pages = merged_pages(frontmatter.pages, frontmatter.source_pages);
    let table_markdown = frontmatter
        .table_structured
        .then(|| extract_table_markdown(&body))
        .flatten();
    let mut meta = BTreeMap::new();
    if !pages.is_empty() {
        meta.insert("pages".to_string(), page_range_string(&pages));
    }
    Ok(Annex {
        id,
        institution: frontmatter.institution,
        rule: frontmatter.rule,
        annex: frontmatter.annex,
        title: frontmatter.title,
        effective: frontmatter.effective,
        status: frontmatter.status,
        body,
        table_structured: frontmatter.table_structured,
        table_markdown,
        pages,
        meta,
    })
}

pub fn load_articles_dir(path: impl AsRef<Path>) -> Result<Vec<Article>> {
    let mut articles = Vec::new();
    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
        {
            continue;
        }
        articles.push(parse_article_markdown(entry.path())?);
    }
    articles.sort_by(|a, b| {
        a.rule
            .cmp(&b.rule)
            .then_with(|| article_sort_key(&a.article).cmp(&article_sort_key(&b.article)))
    });
    Ok(articles)
}

pub fn load_annexes_dir(path: impl AsRef<Path>) -> Result<Vec<Annex>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut annexes = Vec::new();
    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
        {
            continue;
        }
        annexes.push(parse_annex_markdown(entry.path())?);
    }
    annexes.sort_by(|a, b| {
        a.rule
            .cmp(&b.rule)
            .then_with(|| article_sort_key(&a.annex).cmp(&article_sort_key(&b.annex)))
    });
    Ok(annexes)
}

fn load_pages_dir(path: impl AsRef<Path>) -> Result<Option<BTreeMap<u32, String>>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(None);
    }
    let mut pages = BTreeMap::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || path.extension().and_then(|e| e.to_str()) != Some("txt")
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(page) = stem.parse::<u32>() else {
            continue;
        };
        pages.insert(page, fs::read_to_string(path)?);
    }
    Ok(Some(pages))
}

pub fn verify_manifest(root: impl AsRef<Path>, manifest: &PackManifest) -> Result<()> {
    let root = root.as_ref();
    if manifest.schema_version != 1 {
        return Err(RulesCoreError::UnsupportedSchema(manifest.schema_version));
    }
    let manifest_files = normalize_manifest_paths(manifest)?;
    let actual_files = list_pack_files(root)?;
    for path in actual_files.difference(&manifest_files) {
        if path != "manifest.json" {
            return Err(RulesCoreError::UnlistedPackFile(path.clone()));
        }
    }
    for (relative_path, expected) in &manifest.files {
        let path = root.join(safe_relative_path(relative_path)?);
        if !path.is_file() {
            return Err(RulesCoreError::MissingManifestEntry(relative_path.clone()));
        }
        let actual = sha256_file(&path)?;
        let expected = expected.to_ascii_lowercase();
        if actual != expected {
            return Err(RulesCoreError::DigestMismatch {
                path: relative_path.clone(),
                expected,
                actual,
            });
        }
    }
    Ok(())
}

pub fn unpack_pack_archive(
    archive_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let path_key = path_to_manifest_key(&path);
        safe_relative_path(&path_key)
            .map_err(|_| RulesCoreError::UnsupportedArchiveEntry(path_key.clone()))?;

        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Directory => {
                if !entry.unpack_in(destination.as_ref())? {
                    return Err(RulesCoreError::UnsupportedArchiveEntry(path_key));
                }
            }
            _ => return Err(RulesCoreError::UnsupportedArchiveEntry(path_key)),
        }
    }
    Ok(())
}

pub fn slugify_rule(rule: &str) -> String {
    let slug: String = rule
        .nfc()
        .map(|c| if c == '\u{00a0}' { ' ' } else { c })
        .filter(|c| {
            !c.is_whitespace()
                && (c.is_alphanumeric()
                    || *c == '_'
                    || ('가'..='힣').contains(c)
                    || matches!(*c, '·' | 'ㆍ' | '․' | '.' | '-'))
        })
        .collect();
    if slug.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(rule.as_bytes());
        format!("{:x}", hasher.finalize())[..12].to_string()
    } else {
        slug
    }
}

pub fn rrf_fuse(rankings: Vec<Vec<SearchHit>>, k: usize) -> Vec<SearchHit> {
    rrf_fuse_weighted(rankings, k, DEFAULT_RRF_K, &[])
}

pub fn rrf_fuse_weighted(
    rankings: Vec<Vec<SearchHit>>,
    k: usize,
    rrf_k: usize,
    weights: &[f32],
) -> Vec<SearchHit> {
    let mut scored: BTreeMap<String, (SearchHit, f32)> = BTreeMap::new();
    let rrf_k = rrf_k as f32;
    for (list_idx, ranking) in rankings.into_iter().enumerate() {
        let weight = weights.get(list_idx).copied().unwrap_or(1.0);
        if weight <= 0.0 {
            continue;
        }
        for (rank, hit) in ranking.into_iter().enumerate() {
            let score = weight / (rrf_k + rank as f32 + 1.0);
            scored
                .entry(hit.article_id.clone())
                .and_modify(|(existing, total)| {
                    *total += score;
                    if hit.score > existing.score
                        || (hit.score == existing.score && hit.article_id < existing.article_id)
                    {
                        *existing = hit.clone();
                    }
                })
                .or_insert((hit, score));
        }
    }

    let mut hits: Vec<_> = scored
        .into_values()
        .map(|(mut hit, score)| {
            hit.score = score;
            hit
        })
        .collect();
    sort_hits_by_score_and_id(&mut hits);
    hits.truncate(k);
    hits
}

pub fn prefixed_article_id(institution: &str, article_id: &str) -> String {
    format!("{institution}/{article_id}")
}

pub fn namespace_search_route_report(
    mut report: SearchRouteReport,
    institution: &str,
    prefix_article_ids: bool,
) -> SearchRouteReport {
    for hit in &mut report.hits {
        namespace_search_hit(hit, institution, prefix_article_ids);
    }
    if let Some(hit) = &mut report.pin_hit {
        namespace_search_hit(hit, institution, prefix_article_ids);
    }
    for hit in &mut report.retrieval_hits {
        namespace_search_hit(hit, institution, prefix_article_ids);
    }
    report
}

pub fn merge_search_route_reports(reports: Vec<SearchRouteReport>, k: usize) -> SearchRouteReport {
    if reports.is_empty() || k == 0 {
        return SearchRouteReport::default();
    }
    if reports.len() == 1 {
        let mut report = reports.into_iter().next().expect("report exists");
        sort_hits_by_score_and_id(&mut report.hits);
        report.hits.truncate(k);
        sort_hits_by_score_and_id(&mut report.retrieval_hits);
        report.retrieval_hits.truncate(k);
        return report;
    }

    let hits = rrf_fuse(
        reports.iter().map(|report| report.hits.clone()).collect(),
        k,
    );
    let retrieval_hits = rrf_fuse(
        reports
            .iter()
            .map(|report| report.retrieval_hits.clone())
            .collect(),
        k,
    );
    let pin_hit = rrf_fuse(
        reports
            .iter()
            .filter_map(|report| report.pin_hit.clone().map(|hit| vec![hit]))
            .collect(),
        1,
    )
    .into_iter()
    .next();

    SearchRouteReport {
        hits,
        pin_hit,
        retrieval_hits,
    }
}

fn namespace_search_hit(hit: &mut SearchHit, institution: &str, prefix_article_ids: bool) {
    hit.institution = institution.to_string();
    if prefix_article_ids {
        hit.article_id = prefixed_article_id(institution, &hit.article_id);
    }
}

fn merge_pinned_hits(
    pinned: Option<SearchHit>,
    mut ranked: Vec<SearchHit>,
    k: usize,
) -> Vec<SearchHit> {
    let Some(pinned) = pinned else {
        sort_hits_by_score_and_id(&mut ranked);
        ranked.truncate(k);
        return ranked;
    };
    ranked.retain(|hit| hit.article_id != pinned.article_id);
    sort_hits_by_score_and_id(&mut ranked);
    let mut out = Vec::with_capacity(k.min(ranked.len() + 1));
    out.push(pinned);
    out.extend(ranked.into_iter().take(k.saturating_sub(1)));
    out
}

pub fn sort_hits_by_score_and_id(hits: &mut [SearchHit]) {
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.article_id.cmp(&b.article_id))
    });
}

fn direct_article_ref(q: &str, fallback_rule: Option<&str>) -> Option<(String, String)> {
    let (article_start, article_end, article) = find_article_ref(q)?;
    let rule = q[..article_start]
        .split_whitespace()
        .last()
        .map(trim_rule_reference_suffix)
        .filter(|candidate| !candidate.is_empty())
        .or(fallback_rule)?;
    let suffix = q[article_end..].chars().next();
    if suffix.is_some_and(|c| c.is_ascii_digit() || c == '의') {
        return None;
    }
    Some((slugify_rule(rule), article))
}

fn find_annex_ref(q: &str) -> Option<(usize, usize, String)> {
    for (start, _) in q.char_indices() {
        let rest = &q[start..];
        let (kind, mut cursor) = if rest.starts_with("별표") {
            ("별표", start + "별표".len())
        } else if rest.starts_with("별지") {
            ("별지", start + "별지".len())
        } else {
            continue;
        };

        cursor = skip_whitespace(q, cursor);
        if kind == "별지" && q[cursor..].starts_with('제') {
            cursor += '제'.len_utf8();
            cursor = skip_whitespace(q, cursor);
        }

        let digit_start = cursor;
        while let Some(ch) = q[cursor..].chars().next() {
            if ch.is_ascii_digit() || ch == '-' {
                cursor += ch.len_utf8();
            } else {
                break;
            }
        }
        if cursor == digit_start {
            continue;
        }
        let number = q[digit_start..cursor].replace(' ', "");
        if kind == "별지" {
            cursor = skip_whitespace(q, cursor);
            if q[cursor..].starts_with('호') {
                cursor += '호'.len_utf8();
            }
            return Some((start, cursor, format!("별지제{number}호")));
        }
        return Some((start, cursor, format!("별표{number}")));
    }
    None
}

fn skip_whitespace(s: &str, mut idx: usize) -> usize {
    while let Some(ch) = s[idx..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        idx += ch.len_utf8();
    }
    idx
}

fn find_article_ref(q: &str) -> Option<(usize, usize, String)> {
    let mut iter = q.char_indices().peekable();
    while let Some((start, ch)) = iter.next() {
        if ch != '제' {
            continue;
        }
        let Some((_, next)) = iter.peek().copied() else {
            continue;
        };
        if !next.is_ascii_digit() {
            continue;
        }

        let mut main_digits = String::new();
        while let Some((_, digit)) = iter.peek().copied() {
            if !digit.is_ascii_digit() {
                break;
            }
            main_digits.push(digit);
            iter.next();
        }
        let Some((idx, '조')) = iter.peek().copied() else {
            continue;
        };
        let mut end = idx + '조'.len_utf8();
        iter.next();

        let mut article = format!("제{main_digits}조");
        if let Some((idx, '의')) = iter.peek().copied() {
            let mut lookahead = iter.clone();
            lookahead.next();
            let mut sub_digits = String::new();
            let mut sub_end = idx + '의'.len_utf8();
            while let Some((digit_idx, digit)) = lookahead.peek().copied() {
                if !digit.is_ascii_digit() {
                    break;
                }
                sub_digits.push(digit);
                sub_end = digit_idx + digit.len_utf8();
                lookahead.next();
            }
            if !sub_digits.is_empty() {
                article.push('의');
                article.push_str(&sub_digits);
                end = sub_end;
            }
        }
        return Some((start, end, article));
    }
    None
}

fn trim_rule_reference_suffix(rule: &str) -> &str {
    rule.trim_end_matches(|c: char| {
        matches!(
            c,
            '상' | '의' | '은' | '는' | '이' | '가' | '을' | '를' | ':' | ',' | '，'
        )
    })
}

fn strip_korean_suffixes(mut term: &str) -> &str {
    let punctuation = ['?', '!', '.', ',', ':', ';', '，', '。'];
    term = term.trim_end_matches(punctuation);
    for suffix in ["에서", "에게", "으로", "로서", "로써"] {
        if let Some(stripped) = term.strip_suffix(suffix) {
            return stripped;
        }
    }
    trim_rule_reference_suffix(term)
}

pub fn default_pack_status(
    institution: impl Into<String>,
    effective_date: impl Into<String>,
) -> PackStatus {
    PackStatus {
        institution: institution.into(),
        effective_date: effective_date.into(),
        source_commit: "fixture".to_string(),
        index_built_at: OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "unknown".to_string()),
        stale: false,
        source_url: None,
    }
}

#[cfg(test)]
thread_local! {
    static SEARCH_INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn build_search_index<'a>(
    articles: impl Iterator<Item = &'a Article>,
    annexes: impl Iterator<Item = &'a Annex>,
    ko_pos_filter: KoPosFilter,
    body_contexts: Option<&BTreeMap<String, String>>,
    with_bigram: bool,
) -> Result<(Index, SearchFields, TokenizerStatus)> {
    #[cfg(test)]
    SEARCH_INDEX_BUILDS.with(|c| c.set(c.get() + 1));
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
    let body_ng = with_bigram.then(|| {
        schema_builder.add_text_field(
            "body_ng",
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(BIGRAM_TOKENIZER)
                    .set_index_option(IndexRecordOption::WithFreqs),
            ),
        )
    });
    let schema = schema_builder.build();
    let index = Index::create_in_ram(schema);
    let tokenizer_status = register_ko_tokenizer(&index, ko_pos_filter);
    if with_bigram {
        register_bigram_tokenizer(&index)?;
    }

    let fields = SearchFields {
        id,
        rule,
        title,
        body,
        body_ng,
    };
    let mut writer = index.writer_with_num_threads(1, 50_000_000)?;
    for entry in articles
        .map(SearchEntry::from_article)
        .chain(annexes.map(SearchEntry::from_annex))
    {
        // Bigram source: compacted title + body without any context prefix.
        let bigram_source = if with_bigram {
            normalize_compact(&format!("{}\n{}", entry.title, entry.body))
        } else {
            String::new()
        };
        let entry_body = match body_contexts.and_then(|contexts| contexts.get(&entry.id)) {
            Some(context) => format!("{context}\n{}", entry.body),
            None => entry.body,
        };
        let mut document = doc!(
            id => entry.id,
            rule => entry.rule,
            title => entry.title,
            effective => entry.effective,
            body => entry_body,
        );
        if let Some(field) = body_ng {
            document.add_text(field, bigram_source);
        }
        writer.add_document(document)?;
    }
    writer.commit()?;
    Ok((index, fields, tokenizer_status))
}

struct SearchEntry {
    id: String,
    rule: String,
    title: String,
    effective: String,
    body: String,
}

impl SearchEntry {
    fn from_article(article: &Article) -> Self {
        Self {
            id: article.id.clone(),
            rule: article.rule.clone(),
            title: article.title.clone(),
            effective: article.effective.clone(),
            body: article.body.clone(),
        }
    }

    fn from_annex(annex: &Annex) -> Self {
        let title = annex_search_title(annex);
        Self {
            id: annex.id.clone(),
            rule: annex.rule.clone(),
            title: searchable_text_with_compact(&title),
            effective: annex.effective.clone(),
            body: searchable_text_with_compact(&annex.body),
        }
    }
}

fn annex_search_title(annex: &Annex) -> String {
    let title = annex.title.trim();
    if !title.is_empty() {
        return format!("{} {}", annex.annex, title);
    }
    let heading = annex
        .body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("##") && !line.starts_with('|'))
        .find(|line| !line.starts_with("[별표") && !line.starts_with("<별표"))
        .unwrap_or_default();
    if heading.is_empty() {
        annex.annex.clone()
    } else {
        format!("{} {}", annex.annex, heading)
    }
}

fn searchable_text_with_compact(text: &str) -> String {
    let compact = normalize_compact(text);
    if compact.is_empty() || compact == text {
        text.to_string()
    } else {
        format!("{text}\n{compact}")
    }
}

const BIGRAM_TOKENIZER: &str = "ko_ng";

/// `ko_ng` analyzer: lower-cased character 2-grams (no whitespace handling,
/// the source text is already compacted).
fn register_bigram_tokenizer(index: &Index) -> Result<()> {
    use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
    let tokenizer = NgramTokenizer::new(2, 2, false)?;
    index.tokenizers().register(
        BIGRAM_TOKENIZER,
        TextAnalyzer::builder(tokenizer).filter(LowerCaser).build(),
    );
    Ok(())
}

fn normalize_compact(text: &str) -> String {
    text.nfkc()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
}

#[cfg(feature = "korean-tokenizer")]
fn register_ko_tokenizer(index: &Index, filter: KoPosFilter) -> TokenizerStatus {
    register_ko_tokenizer_with(index, filter, || {
        lindera::dictionary::load_dictionary("embedded://ko-dic")
    })
}

#[cfg(not(feature = "korean-tokenizer"))]
fn register_ko_tokenizer(index: &Index, _filter: KoPosFilter) -> TokenizerStatus {
    register_simple_ko_tokenizer(index);
    TokenizerStatus {
        kind: "simple-fallback".to_string(),
        filter: KoPosFilter::None.as_str().to_string(),
        degraded: true,
        reason: Some("korean-tokenizer feature disabled".to_string()),
    }
}

/// Registers the `ko` analyzer. `loader` supplies the dictionary so that a
/// load failure can be exercised; on failure the simple tokenizer is used, an
/// `ERROR tokenizer` line is written to stderr and the status says so.
#[cfg(feature = "korean-tokenizer")]
fn register_ko_tokenizer_with<F>(index: &Index, filter: KoPosFilter, loader: F) -> TokenizerStatus
where
    F: FnOnce() -> lindera::LinderaResult<lindera::dictionary::Dictionary>,
{
    use lindera::character_filter::unicode_normalize::{
        UnicodeNormalizeCharacterFilter, UnicodeNormalizeKind,
    };
    use lindera::character_filter::BoxCharacterFilter;
    use lindera::mode::Mode;
    use lindera::segmenter::Segmenter;
    use lindera::token_filter::korean_stop_tags::KoreanStopTagsTokenFilter;
    use lindera::token_filter::lowercase::LowercaseTokenFilter;
    use lindera::token_filter::BoxTokenFilter;

    match loader() {
        Ok(dictionary) => {
            let mut tokenizer = lindera_tantivy::tokenizer::LinderaTokenizer::from_segmenter(
                Segmenter::new(Mode::Normal, dictionary, None),
            );
            if filter != KoPosFilter::None {
                let tags = filter.stop_tags().into_iter().map(String::from).collect();
                tokenizer.append_character_filter(BoxCharacterFilter::from(
                    UnicodeNormalizeCharacterFilter::new(UnicodeNormalizeKind::NFKC),
                ));
                tokenizer.append_token_filter(BoxTokenFilter::from(LowercaseTokenFilter::new()));
                tokenizer.append_token_filter(BoxTokenFilter::from(
                    KoreanStopTagsTokenFilter::new(tags),
                ));
            }
            index.tokenizers().register("ko", tokenizer);
            TokenizerStatus {
                kind: "lindera-ko-dic".to_string(),
                filter: filter.as_str().to_string(),
                degraded: false,
                reason: None,
            }
        }
        Err(error) => {
            let reason = format!("ko-dic load failed: {error}");
            eprintln!(
                "ERROR tokenizer {reason}; using simple fallback (pos filter {} not applied)",
                filter.as_str()
            );
            register_simple_ko_tokenizer(index);
            TokenizerStatus {
                kind: "simple-fallback".to_string(),
                filter: KoPosFilter::None.as_str().to_string(),
                degraded: true,
                reason: Some(reason),
            }
        }
    }
}

fn register_simple_ko_tokenizer(index: &Index) {
    use tantivy::tokenizer::TextAnalyzer;
    index.tokenizers().register(
        "ko",
        TextAnalyzer::builder(tantivy::tokenizer::SimpleTokenizer::default()).build(),
    );
}

fn build_rule_summaries<'a>(articles: impl Iterator<Item = &'a Article>) -> Vec<RuleSummary> {
    let mut by_rule: BTreeMap<String, RuleSummary> = BTreeMap::new();
    for article in articles {
        let entry = by_rule
            .entry(article.rule.clone())
            .or_insert_with(|| RuleSummary {
                slug: slugify_rule(&article.rule),
                name: article.rule.clone(),
                article_count: 0,
                effective: article.effective.clone(),
            });
        entry.article_count += 1;
        if article.effective > entry.effective {
            entry.effective = article.effective.clone();
        }
    }
    by_rule.into_values().collect()
}

const CONTEXT_SEPARATOR: &str = " · ";
const ANNEX_REFS_LABEL: &str = "참조 조문:";
const ANNEX_REFS_MAX_TITLES: usize = 5;
const ANNEX_REFS_MAX_CHARS: usize = 200;

/// Institution slug -> display label from `Institution` graph nodes.
fn institution_labels(nodes: &[GraphNode]) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for node in nodes.iter().filter(|n| n.kind == NodeKind::Institution) {
        if node.label.trim().is_empty() {
            continue;
        }
        labels
            .entry(node.id.clone())
            .or_insert_with(|| node.label.clone());
        if let Some(slug) = node.meta.get("institution").and_then(|v| v.as_str()) {
            labels
                .entry(slug.to_string())
                .or_insert_with(|| node.label.clone());
        }
    }
    labels
}

/// `"{institution label} · {rule} · {article}({title})"`; the title
/// parentheses are omitted for an empty title.
fn article_context(institution_label: &str, article: &Article) -> String {
    let mut out = [institution_label, &article.rule, &article.article].join(CONTEXT_SEPARATOR);
    let title = article.title.trim();
    if !title.is_empty() {
        out.push('(');
        out.push_str(title);
        out.push(')');
    }
    out
}

/// `"{rule} · {annex title} · 참조 조문: {t1}, {t2}, …"`; the reference part is
/// omitted when no article references the annex. The joined title list is
/// cut at 200 characters (never inside a character).
fn annex_context(annex: &Annex, referencing_titles: &[String]) -> String {
    let mut out = [annex.rule.as_str(), annex_search_title(annex).as_str()].join(CONTEXT_SEPARATOR);
    if !referencing_titles.is_empty() {
        let joined = referencing_titles.join(", ");
        let cut: String = joined.chars().take(ANNEX_REFS_MAX_CHARS).collect();
        out.push_str(CONTEXT_SEPARATOR);
        out.push_str(ANNEX_REFS_LABEL);
        out.push(' ');
        out.push_str(&cut);
    }
    out
}

/// Titles of the first five same-institution articles (by article number) that
/// point at the annex through `AppliesTo` or `Cites`. An article without a
/// title contributes its article number.
fn annex_referencing_titles(
    articles: &BTreeMap<String, Article>,
    graph: &DiGraph<String, EdgeKind>,
    node_indices: &HashMap<String, NodeIndex>,
    annex: &Annex,
) -> Vec<String> {
    let mut sources = reverse_neighbors_in(graph, node_indices, &annex.id, |kind| {
        matches!(kind, EdgeKind::AppliesTo | EdgeKind::Cites)
    })
    .into_iter()
    .filter_map(|id| articles.get(&id))
    .filter(|article| article.institution == annex.institution)
    .collect::<Vec<_>>();
    sources.sort_by(|a, b| {
        article_sort_key(&a.article)
            .cmp(&article_sort_key(&b.article))
            .then_with(|| a.id.cmp(&b.id))
    });
    sources
        .into_iter()
        .take(ANNEX_REFS_MAX_TITLES)
        .map(|article| {
            let title = article.title.trim();
            if title.is_empty() {
                article.article.clone()
            } else {
                title.to_string()
            }
        })
        .collect()
}

/// Context string per article/annex id (pure function of the pack contents).
fn build_contexts(
    articles: &BTreeMap<String, Article>,
    annexes: &BTreeMap<String, Annex>,
    graph: &DiGraph<String, EdgeKind>,
    node_indices: &HashMap<String, NodeIndex>,
    nodes: &[GraphNode],
) -> BTreeMap<String, String> {
    let labels = institution_labels(nodes);
    let mut contexts = BTreeMap::new();
    for article in articles.values() {
        let label = labels
            .get(&article.institution)
            .unwrap_or(&article.institution);
        contexts.insert(article.id.clone(), article_context(label, article));
    }
    for annex in annexes.values() {
        let titles = annex_referencing_titles(articles, graph, node_indices, annex);
        contexts.insert(annex.id.clone(), annex_context(annex, &titles));
    }
    contexts
}

fn reverse_neighbors_in(
    graph: &DiGraph<String, EdgeKind>,
    node_indices: &HashMap<String, NodeIndex>,
    id: &str,
    accept: impl Fn(EdgeKind) -> bool,
) -> Vec<String> {
    let Some(node) = node_indices.get(id).copied() else {
        return Vec::new();
    };
    let mut out = BTreeSet::new();
    for edge in graph.edges_directed(node, petgraph::Incoming) {
        if accept(*edge.weight()) {
            out.insert(graph[edge.source()].clone());
        }
    }
    out.into_iter().collect()
}

fn build_ref_graph<'a>(
    articles: impl Iterator<Item = &'a Article>,
    nodes: &[GraphNode],
    edges: &[GraphEdge],
) -> (DiGraph<String, EdgeKind>, HashMap<String, NodeIndex>) {
    let mut graph = DiGraph::new();
    let mut node_indices = HashMap::new();
    let ensure = |id: &str,
                  graph: &mut DiGraph<String, EdgeKind>,
                  node_indices: &mut HashMap<String, NodeIndex>| {
        node_indices
            .entry(id.to_string())
            .or_insert_with(|| graph.add_node(id.to_string()))
            .to_owned()
    };

    for node in nodes {
        ensure(&node.id, &mut graph, &mut node_indices);
    }

    for article in articles {
        let src = ensure(&article.id, &mut graph, &mut node_indices);
        for article_ref in &article.refs {
            let dst = ensure(&article_ref.target, &mut graph, &mut node_indices);
            graph.add_edge(src, dst, edge_kind_from_ref(&article_ref.kind));
        }
        for basis in &article.legal_basis {
            let law_id = format!("{}#{}", basis.law, basis.article);
            let dst = ensure(&law_id, &mut graph, &mut node_indices);
            graph.add_edge(src, dst, EdgeKind::LegalBasis);
        }
    }

    for edge in edges {
        let src = ensure(&edge.src, &mut graph, &mut node_indices);
        let dst = ensure(&edge.dst, &mut graph, &mut node_indices);
        graph.add_edge(src, dst, edge.kind);
    }

    (graph, node_indices)
}

fn link_neighbors(articles: &mut BTreeMap<String, Article>) {
    let mut by_rule: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for article in articles.values() {
        by_rule
            .entry(article.rule.clone())
            .or_default()
            .push(article.id.clone());
    }
    for ids in by_rule.values_mut() {
        ids.sort_by(|a, b| {
            let aa = articles
                .get(a)
                .map(|article| article.article.as_str())
                .unwrap_or_default();
            let bb = articles
                .get(b)
                .map(|article| article.article.as_str())
                .unwrap_or_default();
            article_sort_key(aa).cmp(&article_sort_key(bb))
        });
        for (idx, id) in ids.iter().enumerate() {
            let prev = idx.checked_sub(1).and_then(|i| ids.get(i)).cloned();
            let next = ids.get(idx + 1).cloned();
            if let Some(article) = articles.get_mut(id) {
                article.prev_id = prev;
                article.next_id = next;
            }
        }
    }
}

fn article_sort_key(article: &str) -> (u32, u32, String) {
    let before_sub = article.split('의').next().unwrap_or(article);
    let main = before_sub
        .split('조')
        .next()
        .unwrap_or(before_sub)
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    let sub = article
        .split_once('의')
        .map(|(_, suffix)| suffix)
        .and_then(|suffix| {
            suffix
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0);
    (main, sub, article.to_string())
}

fn edge_kind_from_ref(kind: &str) -> EdgeKind {
    match kind {
        "준용" => EdgeKind::ApplyMutatis,
        "위임" => EdgeKind::Delegates,
        "단서예외" => EdgeKind::ExceptWhen,
        _ => EdgeKind::Cites,
    }
}

fn load_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
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

fn sha256_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn pack_vector_cache_key(manifest: &PackManifest, format: PassageFormat) -> String {
    let mut value = serde_json::json!({
        "cache_version": VECTOR_CACHE_VERSION,
        "model_id": VECTOR_MODEL_ID,
        "model_revision": VECTOR_MODEL_REVISION,
        "schema_version": manifest.schema_version,
        "institution": manifest.institution,
        "effective_date": manifest.effective_date,
        "source_commit": manifest.source_commit,
        "files": manifest.files,
    });
    // Inserted only for non-legacy formats so legacy keys stay byte-identical.
    if let (Some(tag), Some(map)) = (format.cache_tag(), value.as_object_mut()) {
        map.insert("passage_format".to_string(), serde_json::json!(tag));
    }
    sha256_text(&serde_json::to_string(&value).unwrap_or_default())
}

fn vector_cache_path(cache_dir: &Path, cache_key: &str) -> PathBuf {
    cache_dir.join(format!("{VECTOR_MODEL_ID}-{cache_key}.json"))
}

fn build_or_load_vector_corpus<'a>(
    articles: impl Iterator<Item = &'a Article>,
    annexes: impl Iterator<Item = &'a Annex>,
    provider: &dyn EmbeddingProvider,
    cache_key: &str,
    cache_dir: Option<&Path>,
    format: PassageFormat,
    contexts: &BTreeMap<String, String>,
) -> anyhow::Result<VectorCorpus> {
    let entries = vector_source_entries(articles, annexes, format, contexts);
    if let Some(cache_dir) = cache_dir {
        let path = vector_cache_path(cache_dir, cache_key);
        if let Some(corpus) = load_vector_corpus_cache(&path, cache_key, &entries)? {
            return Ok(corpus);
        }
        let corpus = compute_vector_corpus(entries, provider)?;
        write_vector_corpus_cache(&path, cache_key, &corpus)?;
        return Ok(corpus);
    }
    compute_vector_corpus(entries, provider)
}

fn load_vector_corpus_cache(
    path: &Path,
    cache_key: &str,
    sources: &[(String, String, String, String)],
) -> anyhow::Result<Option<VectorCorpus>> {
    if !path.is_file() {
        return Ok(None);
    }
    let cache: VectorCacheFile = serde_json::from_reader(File::open(path)?)?;
    if cache.version != VECTOR_CACHE_VERSION
        || cache.model != VECTOR_MODEL_ID
        || cache.cache_key != cache_key
        || cache.entries.len() != sources.len()
    {
        return Ok(None);
    }
    for (entry, (id, kind, text_hash, _)) in cache.entries.iter().zip(sources.iter()) {
        if &entry.id != id || &entry.kind != kind || &entry.text_hash != text_hash {
            return Ok(None);
        }
    }
    Ok(Some(VectorCorpus::new(cache.entries)))
}

fn write_vector_corpus_cache(
    path: &Path,
    cache_key: &str,
    corpus: &VectorCorpus,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let cache = VectorCacheFile {
        version: VECTOR_CACHE_VERSION,
        model: VECTOR_MODEL_ID.to_string(),
        cache_key: cache_key.to_string(),
        entries: corpus.entries.clone(),
    };
    let mut file = File::create(&tmp_path)?;
    serde_json::to_writer(&mut file, &cache)?;
    file.write_all(b"\n")?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn compute_vector_corpus(
    sources: Vec<(String, String, String, String)>,
    provider: &dyn EmbeddingProvider,
) -> anyhow::Result<VectorCorpus> {
    let mut entries = Vec::with_capacity(sources.len());
    for (id, kind, text_hash, text) in sources {
        let embedding = provider.embed(&text)?;
        if embedding.is_empty() {
            anyhow::bail!("embedding provider returned an empty vector for {id}");
        }
        entries.push(VectorEntry {
            id,
            kind,
            text_hash,
            embedding,
        });
    }
    Ok(VectorCorpus::new(entries))
}

fn vector_source_entries<'a>(
    articles: impl Iterator<Item = &'a Article>,
    annexes: impl Iterator<Item = &'a Annex>,
    fmt: PassageFormat,
    contexts: &BTreeMap<String, String>,
) -> Vec<(String, String, String, String)> {
    // `ctx1` passages start with the context line; a missing entry (never in
    // practice) degrades to the plain passage.
    let context_line = |id: &str| -> String {
        if fmt.has_context() {
            contexts
                .get(id)
                .map(|context| format!("{context}\n"))
                .unwrap_or_default()
        } else {
            String::new()
        }
    };
    let mut entries = articles
        .map(|article| {
            let text = fmt.passage_text(format!(
                "{}{} {}\n{}",
                context_line(&article.id),
                article.rule,
                article.title,
                article.body
            ));
            (
                article.id.clone(),
                "article".to_string(),
                sha256_text(&text),
                text,
            )
        })
        .chain(annexes.map(|annex| {
            let text = fmt.passage_text(format!(
                "{}{} {}\n{}",
                context_line(&annex.id),
                annex.rule,
                annex_search_title(annex),
                annex.body
            ));
            (
                annex.id.clone(),
                "annex".to_string(),
                sha256_text(&text),
                text,
            )
        }))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn squared_norm(v: &[f32]) -> f32 {
    let mut norm = 0.0_f32;
    for x in v {
        norm += x * x;
    }
    norm
}

/// Same arithmetic as the legacy `cosine_similarity`, with the corpus-side
/// `sqrt(norm)` supplied by the caller.
fn cosine_similarity_with_norm(left: &[f32], right: &[f32], right_norm_sqrt: f32) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    for (l, r) in left.iter().zip(right.iter()) {
        dot += l * r;
        left_norm += l * l;
    }
    if left_norm == 0.0 || right_norm_sqrt == 0.0 {
        return None;
    }
    Some(dot / (left_norm.sqrt() * right_norm_sqrt))
}

#[cfg(test)]
fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (l, r) in left.iter().zip(right.iter()) {
        dot += l * r;
        left_norm += l * l;
        right_norm += r * r;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return None;
    }
    Some(dot / (left_norm.sqrt() * right_norm.sqrt()))
}

fn lexical_fallback<'a>(
    articles: impl Iterator<Item = &'a Article>,
    q: &str,
    k: usize,
    filter: Option<&RuleFilter>,
) -> Vec<SearchHit> {
    let terms = query_terms(q);
    lexical_rank(articles, [].iter(), q, &terms, k, filter)
}

fn lexical_rank<'a>(
    articles: impl Iterator<Item = &'a Article>,
    annexes: impl Iterator<Item = &'a Annex>,
    q: &str,
    terms: &[String],
    k: usize,
    filter: Option<&RuleFilter>,
) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for article in articles {
        if !matches_filter(article, filter) {
            continue;
        }
        let score = terms.iter().fold(0.0, |score, term| {
            score
                + if article.rule.contains(term.as_str()) {
                    3.0
                } else {
                    0.0
                }
                + if article.title.contains(term.as_str()) {
                    2.0
                } else {
                    0.0
                }
                + if article.article.contains(term.as_str()) {
                    1.5
                } else {
                    0.0
                }
                + if article.body.contains(term.as_str()) {
                    1.0
                } else {
                    0.0
                }
        });
        if score > 0.0 {
            hits.push(SearchHit {
                article_id: article.id.clone(),
                institution: article.institution.clone(),
                score,
                snippet: snippet(&article.body, q),
                rule: article.rule.clone(),
                title: article.title.clone(),
                effective: article.effective.clone(),
                kind: "article".to_string(),
            });
        }
    }
    for annex in annexes {
        if !matches_annex_filter(annex, filter) {
            continue;
        }
        let title = annex_search_title(annex);
        let compact_body = normalize_compact(&annex.body);
        let compact_title = normalize_compact(&title);
        let score = terms.iter().fold(0.0, |score, term| {
            score
                + if annex.rule.contains(term.as_str()) {
                    3.0
                } else {
                    0.0
                }
                + if title.contains(term.as_str()) || compact_title.contains(term.as_str()) {
                    2.0
                } else {
                    0.0
                }
                + if annex.annex.contains(term.as_str()) {
                    1.5
                } else {
                    0.0
                }
                + if annex.body.contains(term.as_str()) || compact_body.contains(term.as_str()) {
                    1.0
                } else {
                    0.0
                }
        });
        if score > 0.0 {
            hits.push(search_hit_for_annex(annex, score, q));
        }
    }
    sort_hits_by_score_and_id(&mut hits);
    hits.truncate(k);
    hits
}

fn search_hit_for_annex(annex: &Annex, score: f32, q: &str) -> SearchHit {
    SearchHit {
        article_id: annex.id.clone(),
        institution: annex.institution.clone(),
        score,
        snippet: snippet(&annex.body, q),
        rule: annex.rule.clone(),
        title: annex_search_title(annex),
        effective: annex.effective.clone(),
        kind: "annex".to_string(),
    }
}

fn query_terms(q: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in q.split_whitespace() {
        let stripped = strip_korean_suffixes(raw);
        for term in [
            raw.trim_matches(|c: char| c.is_ascii_punctuation()),
            stripped,
        ] {
            if term.chars().count() >= 2 && !terms.iter().any(|existing| existing == term) {
                terms.push(term.to_string());
            }
        }
    }
    terms
}

fn rule_query_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .filter(|term| looks_like_rule_name(term))
        .cloned()
        .collect()
}

fn looks_like_rule_name(term: &str) -> bool {
    [
        "규정",
        "규칙",
        "강령",
        "정관",
        "조례",
        "법",
        "시행령",
        "시행규칙",
    ]
    .iter()
    .any(|suffix| term.ends_with(suffix))
}

fn matches_filter(article: &Article, filter: Option<&RuleFilter>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    filter
        .institution
        .as_ref()
        .is_none_or(|v| &article.institution == v)
        && filter.rule.as_ref().is_none_or(|v| &article.rule == v)
        && filter.status.as_ref().is_none_or(|v| &article.status == v)
        && filter
            .effective
            .as_ref()
            .is_none_or(|v| &article.effective == v)
}

fn matches_annex_filter(annex: &Annex, filter: Option<&RuleFilter>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    filter
        .institution
        .as_ref()
        .is_none_or(|v| &annex.institution == v)
        && filter.rule.as_ref().is_none_or(|v| &annex.rule == v)
        && filter.status.as_ref().is_none_or(|v| &annex.status == v)
        && filter
            .effective
            .as_ref()
            .is_none_or(|v| &annex.effective == v)
}

fn default_search_hit_kind() -> String {
    "article".to_string()
}

fn is_annex_id(id: &str) -> bool {
    id.split_once(ARTICLE_ID_SEPARATOR)
        .map(|(_, item)| item.starts_with("별표") || item.starts_with("별지"))
        .unwrap_or(false)
}

fn page_range_string(pages: &[u32]) -> String {
    match pages {
        [] => String::new(),
        [page] => page.to_string(),
        [start, end, ..] => format!("{start}-{end}"),
    }
}

fn merged_pages(pages: Vec<u32>, source_pages: Vec<u32>) -> Vec<u32> {
    if pages.is_empty() {
        source_pages
    } else {
        pages
    }
}

fn extract_table_markdown(body: &str) -> Option<String> {
    body.split_once("## Extracted tables")
        .map(|(_, tables)| format!("## Extracted tables{}", tables.trim_end()))
}

fn build_page_owners<'a>(
    articles: impl Iterator<Item = &'a Article>,
    annexes: impl Iterator<Item = &'a Annex>,
) -> BTreeMap<u32, Vec<String>> {
    let mut owners = BTreeMap::<u32, BTreeSet<String>>::new();
    for article in articles {
        for page in expand_pages(&article.pages) {
            owners.entry(page).or_default().insert(article.id.clone());
        }
    }
    for annex in annexes {
        for page in expand_pages(&annex.pages) {
            owners.entry(page).or_default().insert(annex.id.clone());
        }
    }
    owners
        .into_iter()
        .map(|(page, ids)| (page, ids.into_iter().collect()))
        .collect()
}

fn expand_pages(pages: &[u32]) -> Vec<u32> {
    match pages {
        [] => Vec::new(),
        [page] => vec![*page],
        [start, end, ..] if start <= end => (*start..=*end).collect(),
        [start, end, ..] => vec![*start, *end],
    }
}

fn normalize_manifest_paths(manifest: &PackManifest) -> Result<BTreeSet<String>> {
    manifest
        .files
        .keys()
        .map(|path| safe_relative_path(path).map(|p| path_to_manifest_key(&p)))
        .collect()
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let raw = Path::new(path);
    if raw.is_absolute() || path.is_empty() {
        return Err(RulesCoreError::UnsafePackPath(path.to_string()));
    }

    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => return Err(RulesCoreError::UnsafePackPath(path.to_string())),
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(RulesCoreError::UnsafePackPath(path.to_string()));
    }
    Ok(normalized)
}

fn list_pack_files(root: &Path) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    collect_files(root, root, &mut files)?;
    Ok(files)
}

fn collect_files(root: &Path, current: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).expect("path is inside root");
            files.insert(path_to_manifest_key(relative));
        }
    }
    Ok(())
}

fn path_to_manifest_key(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn first_text(doc: &TantivyDocument, field: Field) -> Option<&str> {
    doc.get_first(field).and_then(|value| value.as_str())
}

fn snippet(body: &str, q: &str) -> String {
    const SNIPPET_CHARS: usize = 120;
    const CONTEXT_BEFORE_CHARS: usize = 40;

    let terms = query_terms(q);
    if terms.is_empty() {
        return body.chars().take(120).collect();
    }

    let mut best: Option<(usize, usize, usize)> = None;
    for term in &terms {
        let mut search_from = 0;
        while let Some(relative_pos) = body[search_from..].find(term) {
            let pos = search_from + relative_pos;
            let start = byte_index_before(body, pos, CONTEXT_BEFORE_CHARS);
            let end = byte_index_after(body, start, SNIPPET_CHARS);
            let window = &body[start..end];
            let score = terms
                .iter()
                .filter(|candidate| {
                    let candidate: &String = candidate;
                    window.contains(candidate.as_str())
                })
                .count();
            let candidate = (score, start, end);
            if best.is_none_or(|current| {
                candidate.0 > current.0 || (candidate.0 == current.0 && candidate.1 < current.1)
            }) {
                best = Some(candidate);
            }
            search_from = pos + term.len();
        }
    }

    let Some((_, start, end)) = best else {
        return body.chars().take(120).collect();
    };
    body[start..end].to_string()
}

fn byte_index_before(s: &str, pos: usize, char_count: usize) -> usize {
    s[..pos]
        .char_indices()
        .rev()
        .nth(char_count)
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn byte_index_after(s: &str, pos: usize, char_count: usize) -> usize {
    s[pos..]
        .char_indices()
        .nth(char_count)
        .map(|(idx, _)| pos + idx)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn fixture_articles() -> Vec<Article> {
        [
            r#"---
institution: cni
rule: 복무규정
article: 제18조
title: 연차휴가
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
① 직원의 연차휴가는 근로기준법에 따른다.
"#,
            r#"---
institution: cni
rule: 출장규정
article: 제12조
title: 교통비
effective: 2026-02-27
amended: 2026-02-27
status: active
supersedes: null
legal_basis: []
refs:
  - target: 복무규정#제18조
    type: 준용
---
① 국내 출장자에게는 교통비를 지급한다. 휴가 중 출장은 복무규정을 준용한다.
"#,
            r#"---
institution: cni
rule: 출장규정
article: 제13조
title: 숙박비
effective: 2026-02-27
amended: 2026-02-27
status: active
supersedes: null
legal_basis: []
refs:
  - target: 출장규정#제12조
    type: 인용
---
① 숙박비는 출장지와 교통비 지급 기준을 고려하여 지급한다.
"#,
        ]
        .into_iter()
        .map(parse_article_markdown_str)
        .collect::<Result<Vec<_>>>()
        .unwrap()
    }

    fn article_fixture(
        institution: &str,
        rule: &str,
        article: &str,
        title: &str,
        body: &str,
    ) -> Article {
        parse_article_markdown_str(&format!(
            "---\ninstitution: {institution}\nrule: {rule}\narticle: {article}\ntitle: {title}\neffective: 2026-03-01\namended: 2026-03-01\nstatus: active\nsupersedes: null\nlegal_basis: []\nrefs: []\n---\n{body}\n"
        ))
        .unwrap()
    }

    #[derive(Clone)]
    struct MockEmbeddingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl MockEmbeddingProvider {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl EmbeddingProvider for MockEmbeddingProvider {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if text.contains("semantic-query") || text.contains("semantic-target") {
                Ok(vec![1.0, 0.0])
            } else {
                Ok(vec![0.0, 1.0])
            }
        }
    }

    #[derive(Clone, Default)]
    struct RecordingProvider {
        texts: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl EmbeddingProvider for RecordingProvider {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            self.texts.lock().unwrap().push(text.to_string());
            Ok(vec![1.0, 0.0])
        }
    }

    fn e5_test_index(e5_prefix: bool, provider: RecordingProvider) -> TantivyRulesIndex {
        let mut index = TantivyRulesIndex::from_articles(
            vec![article_fixture(
                "cni",
                "시험규칙",
                "제1조",
                "목적",
                "시험 본문",
            )],
            default_pack_status("cni", "2026-03-01"),
        )
        .unwrap();
        index.search_options = SearchOptions {
            e5_prefix,
            ..SearchOptions::default()
        };
        index
            .enable_vectors_for_test(provider, "test-key", None, 60, 1.0)
            .unwrap();
        index
    }

    fn test_manifest() -> PackManifest {
        PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-03-01".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-03-01T00:00:00Z".to_string(),
            source_url: None,
            quality: None,
            files: BTreeMap::from([
                ("articles/a.md".to_string(), "aa11".to_string()),
                ("manifest.json".to_string(), "bb22".to_string()),
            ]),
        }
    }

    #[test]
    fn e5_prefix_applies_to_both_passage_and_query() {
        let provider = RecordingProvider::default();
        let index = e5_test_index(true, provider.clone());
        index.search("시험질의", 5, None);

        let texts = provider.texts.lock().unwrap().clone();
        let passage = "passage: 시험규칙 목적\n시험 본문\n";
        assert!(
            texts.iter().any(|t| t.starts_with("passage: ")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t == passage), "{texts:?}");
        assert!(texts.iter().any(|t| t == "query: 시험질의"), "{texts:?}");
        // Neither side may be embedded without its prefix.
        assert!(!texts.iter().any(|t| t == "시험질의"), "{texts:?}");
        assert!(texts
            .iter()
            .all(|t| t.starts_with("passage: ") || t.starts_with("query: ")));
    }

    #[test]
    fn e5_prefix_off_keeps_legacy_texts() {
        let provider = RecordingProvider::default();
        let index = e5_test_index(false, provider.clone());
        index.search("시험질의", 5, None);

        let texts = provider.texts.lock().unwrap().clone();
        assert_eq!(
            texts,
            vec![
                "시험규칙 목적\n시험 본문\n".to_string(),
                "시험질의".to_string()
            ]
        );
        assert_eq!(texts[0].as_bytes(), "시험규칙 목적\n시험 본문\n".as_bytes());
    }

    #[test]
    fn legacy_passage_format_keeps_cache_key_bytes() {
        let manifest = test_manifest();
        // Replica of the 2f259192 key formula.
        let old_value = serde_json::json!({
            "cache_version": VECTOR_CACHE_VERSION,
            "model_id": VECTOR_MODEL_ID,
            "model_revision": VECTOR_MODEL_REVISION,
            "schema_version": manifest.schema_version,
            "institution": manifest.institution,
            "effective_date": manifest.effective_date,
            "source_commit": manifest.source_commit,
            "files": manifest.files,
        });
        let old_key = sha256_text(&serde_json::to_string(&old_value).unwrap());
        let new_key = pack_vector_cache_key(&manifest, PassageFormat::Legacy);
        println!("old cache key: {old_key}");
        println!("new cache key: {new_key}");
        assert_eq!(old_key, new_key);
        assert_eq!(
            SearchOptions::default().passage_format(),
            PassageFormat::Legacy
        );
    }

    #[test]
    fn prefixed_format_uses_distinct_cache_file() {
        let manifest = test_manifest();
        let legacy_key = pack_vector_cache_key(&manifest, PassageFormat::Legacy);
        let e5_key = pack_vector_cache_key(&manifest, PassageFormat::E5Prefix);
        assert_ne!(legacy_key, e5_key);

        let dir = tempfile::tempdir().unwrap();
        let articles = [article_fixture(
            "cni",
            "시험규칙",
            "제1조",
            "목적",
            "시험 본문",
        )];
        for (key, fmt) in [
            (&legacy_key, PassageFormat::Legacy),
            (&e5_key, PassageFormat::E5Prefix),
        ] {
            build_or_load_vector_corpus(
                articles.iter(),
                std::iter::empty(),
                &RecordingProvider::default(),
                key,
                Some(dir.path()),
                fmt,
                &BTreeMap::new(),
            )
            .unwrap();
        }
        assert!(vector_cache_path(dir.path(), &legacy_key).is_file());
        assert!(vector_cache_path(dir.path(), &e5_key).is_file());
        assert_ne!(
            vector_cache_path(dir.path(), &legacy_key),
            vector_cache_path(dir.path(), &e5_key)
        );
    }

    #[test]
    fn parses_frontmatter_article_id_and_refs() {
        let article = fixture_articles().remove(1);
        assert_eq!(article.id, "출장규정#제12조");
        assert_eq!(article.legal_basis.len(), 0);
        assert_eq!(article.refs[0].target, "복무규정#제18조");
    }

    #[test]
    fn builds_index_and_searches_articles() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let hits = index.search("교통비 출장", 5, None);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].article_id, "출장규정#제12조");

        let filtered = index.search(
            "연차휴가",
            5,
            Some(RuleFilter {
                rule: Some("복무규정".to_string()),
                ..RuleFilter::default()
            }),
        );
        assert_eq!(filtered[0].article_id, "복무규정#제18조");
    }

    #[test]
    fn vector_rank_is_fused_when_enabled_with_mock_provider() {
        let articles = vec![
            article_fixture(
                "cni",
                "의미규정",
                "제1조",
                "대상",
                "semantic-target only appears in this provision.",
            ),
            article_fixture("cni", "일반규정", "제2조", "기타", "unrelated body text"),
        ];
        let mut index =
            TantivyRulesIndex::from_articles(articles, default_pack_status("cni", "2026-03-01"))
                .unwrap();
        index
            .enable_vectors_for_test(MockEmbeddingProvider::new(), "mock-key", None, 60, 2.0)
            .unwrap();

        let hits = index.search("semantic-query", 5, None);

        assert_eq!(hits[0].article_id, "의미규정#제1조");
    }

    #[test]
    fn disabled_vector_path_preserves_existing_order() {
        let articles = fixture_articles();
        let left =
            TantivyRulesIndex::from_articles(articles.clone(), default_pack_status("cni", "base"))
                .unwrap();
        let right =
            TantivyRulesIndex::from_articles(articles, default_pack_status("cni", "base")).unwrap();

        let left_ids = left
            .search("교통비 출장", 5, None)
            .into_iter()
            .map(|hit| hit.article_id)
            .collect::<Vec<_>>();
        let right_ids = right
            .search("교통비 출장", 5, None)
            .into_iter()
            .map(|hit| hit.article_id)
            .collect::<Vec<_>>();

        assert_eq!(left_ids, right_ids);
        assert!(left.vector_corpus.is_none());
        assert!(right.vector_corpus.is_none());
    }

    #[test]
    fn vector_cache_miss_then_hit_avoids_reembedding_corpus() {
        let temp = tempfile::tempdir().unwrap();
        let articles = vec![article_fixture(
            "cni",
            "의미규정",
            "제1조",
            "대상",
            "semantic-target body",
        )];
        let first_provider = MockEmbeddingProvider::new();
        let mut first =
            TantivyRulesIndex::from_articles(articles.clone(), default_pack_status("cni", "base"))
                .unwrap();
        first
            .enable_vectors_for_test(
                first_provider.clone(),
                "cache-key",
                Some(temp.path()),
                60,
                1.0,
            )
            .unwrap();
        assert_eq!(first_provider.calls(), 1);

        let second_provider = MockEmbeddingProvider::new();
        let mut second =
            TantivyRulesIndex::from_articles(articles.clone(), default_pack_status("cni", "base"))
                .unwrap();
        second
            .enable_vectors_for_test(
                second_provider.clone(),
                "cache-key",
                Some(temp.path()),
                60,
                1.0,
            )
            .unwrap();
        assert_eq!(second_provider.calls(), 0);

        let miss_provider = MockEmbeddingProvider::new();
        let mut miss =
            TantivyRulesIndex::from_articles(articles, default_pack_status("cni", "base")).unwrap();
        miss.enable_vectors_for_test(
            miss_provider.clone(),
            "different-cache-key",
            Some(temp.path()),
            60,
            1.0,
        )
        .unwrap();
        assert_eq!(miss_provider.calls(), 1);
    }

    #[cfg(not(feature = "vectors"))]
    #[test]
    fn vector_opt_in_without_feature_falls_back_to_lexical_index() {
        let temp = tempfile::tempdir().unwrap();
        write_pack_fixture(temp.path());

        let index = TantivyRulesIndex::from_pack_dir_with_vector_options(
            temp.path(),
            VectorSearchOptions::enabled(temp.path().join("vector-cache")),
        )
        .unwrap();

        assert!(index.vector_corpus.is_none());
        let hits = index.search("항공운임", 5, None);
        assert_eq!(hits[0].article_id, "여비지급규칙#제12조");
    }

    #[test]
    fn searches_rule_name_and_title_fields_with_boost() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let by_rule = index.search("출장규정은 무엇을 정하는가", 5, None);
        assert!(by_rule
            .iter()
            .take(2)
            .any(|hit| hit.article_id == "출장규정#제12조"));

        let by_title = index.search("숙박비 기준", 5, None);
        assert_eq!(by_title[0].article_id, "출장규정#제13조");
    }

    #[test]
    fn pins_direct_article_reference_when_id_exists() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let hits = index.search("출장규정 제13조는 현재 어떤 상태인가?", 5, None);
        assert_eq!(hits[0].article_id, "출장규정#제13조");
    }

    #[test]
    fn reports_pin_and_retrieval_routes_separately() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let report = index.search_with_routes("출장규정 제13조 숙박비 기준", 5, None);

        assert_eq!(
            report.pin_hit.as_ref().map(|hit| hit.article_id.as_str()),
            Some("출장규정#제13조")
        );
        assert!(report
            .retrieval_hits
            .iter()
            .any(|hit| hit.article_id == "출장규정#제13조"));
        assert_eq!(report.hits[0].article_id, "출장규정#제13조");
    }

    #[test]
    fn merges_namespaced_multi_pack_route_reports() {
        let cni = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();
        let ctp = TantivyRulesIndex::from_articles(
            vec![article_fixture(
                "ctp",
                "인사규정",
                "제10조",
                "육아휴직",
                "① 직원은 자녀 양육을 위해 육아휴직을 신청할 수 있다.",
            )],
            default_pack_status("ctp", "2026-03-01"),
        )
        .unwrap();

        let reports = vec![
            namespace_search_route_report(cni.search_with_routes("육아휴직", 5, None), "cni", true),
            namespace_search_route_report(ctp.search_with_routes("육아휴직", 5, None), "ctp", true),
        ];
        let report = merge_search_route_reports(reports, 5);

        assert!(report
            .hits
            .iter()
            .any(|hit| hit.article_id == "ctp/인사규정#제10조" && hit.institution == "ctp"));
        assert!(report
            .retrieval_hits
            .iter()
            .all(|hit| hit.article_id.starts_with("cni/") || hit.article_id.starts_with("ctp/")));
    }

    #[test]
    fn direct_article_reference_uses_filter_rule_when_query_omits_rule_name() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let hits = index.search(
            "제18조는 무엇인가?",
            5,
            Some(RuleFilter {
                rule: Some("복무규정".to_string()),
                ..RuleFilter::default()
            }),
        );
        assert_eq!(hits[0].article_id, "복무규정#제18조");
    }

    #[test]
    fn direct_article_reference_parser_handles_sub_articles_and_suffix_particles() {
        assert_eq!(
            direct_article_ref("인사관리규정상 제19조의2는 무엇인가?", None),
            Some(("인사관리규정".to_string(), "제19조의2".to_string()))
        );
        assert_eq!(
            direct_article_ref("제18조는 무엇인가?", Some("복무규정")),
            Some(("복무규정".to_string(), "제18조".to_string()))
        );
        assert_eq!(direct_article_ref("인사관리규정 제19조의는?", None), None);
    }

    fn annex_fixture(institution: &str, rule: &str, annex: &str, title: &str, body: &str) -> Annex {
        parse_annex_markdown_str(&format!(
            "---\ntype: annex\ninstitution: {institution}\nrule: {rule}\nannex: {annex}\ntitle: {title}\neffective: 2026-03-01\nstatus: active\ntable_structured: true\n---\n{body}\n"
        ))
        .unwrap()
    }

    fn index_with_annexes(articles: Vec<Article>, annexes: Vec<Annex>) -> TantivyRulesIndex {
        let mut index =
            TantivyRulesIndex::from_articles(articles, default_pack_status("cni", "2026-03-01"))
                .unwrap();
        index.annexes = annexes
            .into_iter()
            .map(|annex| (annex.id.clone(), annex))
            .collect();
        (index.index, index.fields, index.tokenizer_status) = build_search_index(
            index.articles.values(),
            index.annexes.values(),
            index.search_options.ko_pos_filter,
            None,
            false,
        )
        .unwrap();
        index.reader = SearchReader(index.index.reader().unwrap());
        index
    }

    #[test]
    fn pins_direct_annex_reference_with_spaced_rule_name() {
        let index = index_with_annexes(
            vec![article_fixture(
                "cni",
                "인사 규정",
                "제20조",
                "자격",
                "자격 기준은 별표 1에 따른다.",
            )],
            vec![annex_fixture(
                "cni",
                "인사 규정",
                "별표1",
                "",
                "[별표 1]\n자격 기준\n| 구분 | A등급 |\n| --- | --- |\n| 기간 | 4년 |",
            )],
        );

        let report = index.search_with_routes("인사 규정 별표1에서 A등급 자격기준", 5, None);

        assert_eq!(
            report.pin_hit.as_ref().map(|hit| hit.article_id.as_str()),
            Some("인사규정#별표1")
        );
        assert_eq!(report.hits[0].kind, "annex");
    }

    #[test]
    fn boosts_unqualified_annex_reference_without_pin() {
        let index = index_with_annexes(
            vec![article_fixture(
                "cni",
                "지원규칙",
                "제15조",
                "지원",
                "출장 지원은 별표 1에 따른다.",
            )],
            vec![annex_fixture(
                "cni",
                "지원규칙",
                "별표1",
                "국내지원 지급표",
                "지원금 교통보조 단기 출장 지급 기준\n| 항목 | 금액 |\n| --- | --- |\n| 지원금 | 25000 |",
            )],
        );

        let report = index.search_with_routes("별표1 지원금 교통보조 기준", 5, None);

        assert!(report.pin_hit.is_none());
        assert_eq!(report.hits[0].article_id, "지원규칙#별표1");
        assert_eq!(report.hits[0].kind, "annex");
    }

    #[test]
    fn pins_unqualified_annex_reference_with_rule_filter() {
        let index = index_with_annexes(
            vec![article_fixture(
                "cni",
                "지원규칙",
                "제15조",
                "지원",
                "출장 지원은 별표 1에 따른다.",
            )],
            vec![annex_fixture(
                "cni",
                "지원규칙",
                "별표1",
                "국내지원 지급표",
                "지원금 교통보조 단기 출장 지급 기준",
            )],
        );

        let report = index.search_with_routes(
            "별표 1 지원금 기준",
            5,
            Some(RuleFilter {
                rule: Some("지원규칙".to_string()),
                ..RuleFilter::default()
            }),
        );

        assert_eq!(
            report.pin_hit.as_ref().map(|hit| hit.article_id.as_str()),
            Some("지원규칙#별표1")
        );
    }

    #[test]
    fn gets_article_neighbors_and_legal_basis() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let article = index.get_article("출장규정#제12조").unwrap();
        assert_eq!(article.next_id.as_deref(), Some("출장규정#제13조"));
        assert_eq!(index.related_laws("복무규정#제18조")[0].law, "근로기준법");
        assert_eq!(index.list_rules().len(), 2);
    }

    #[test]
    fn computes_impact_from_reverse_refs_and_delegation_chain() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        let impact = index.impact("복무규정#제18조");
        assert_eq!(impact.reverse_citations, vec!["출장규정#제12조"]);

        let travel_impact = index.impact("출장규정#제12조");
        assert_eq!(travel_impact.reverse_citations, vec!["출장규정#제13조"]);
        assert_eq!(travel_impact.delegation_chain, vec!["복무규정#제18조"]);
    }

    // --- 조문 파서 경계: 제N조의M ---

    fn article_md(rule: &str, article: &str, title: &str, body: &str) -> String {
        format!(
            "---\ninstitution: cni\nrule: {rule}\narticle: {article}\ntitle: {title}\neffective: 2026-02-27\namended: 2026-02-27\nstatus: active\nsupersedes: null\nlegal_basis: []\nrefs: []\n---\n{body}\n"
        )
    }

    #[test]
    fn parses_article_id_for_sub_numbered_article() {
        let article = parse_article_markdown_str(&article_md(
            "인사관리규정",
            "제19조의2",
            "결원보충",
            "① 본문",
        ))
        .unwrap();
        assert_eq!(article.id, "인사관리규정#제19조의2");
        assert_eq!(article.article, "제19조의2");
    }

    #[test]
    fn slugify_rule_matches_pipeline_canonical_golden_cases() {
        assert_eq!(slugify_rule(" 여비 지급 규칙 "), "여비지급규칙");
        assert_eq!(
            slugify_rule("안전\u{00a0}· 보건 관리규칙"),
            "안전·보건관리규칙"
        );
        assert_eq!(slugify_rule("A/B: CNI Rules!"), "ABCNIRules");
        assert_eq!(slugify_rule("e\u{301} 규정"), "é규정");
        assert_eq!(slugify_rule("!!!"), "e84c538e7fe2");
    }

    #[test]
    fn preserves_nested_hang_structure_verbatim_in_body() {
        let body = "① 첫째 항이다.\n② 둘째 항이다. 1. 첫째 호\n2. 둘째 호\n③ 셋째 항, 단서예외를 포함한다.";
        let article =
            parse_article_markdown_str(&article_md("여비지급규칙", "제12조", "항공운임", body))
                .unwrap();
        assert_eq!(article.body, format!("{body}\n"));
        assert!(article.body.contains("① 첫째"));
        assert!(article.body.contains("② 둘째"));
        assert!(article.body.contains("③ 셋째"));
    }

    #[test]
    fn orders_neighbors_so_sub_numbered_article_sits_between_base_articles() {
        let articles = vec![
            parse_article_markdown_str(&article_md("인사관리규정", "제19조", "채용", "① 본문"))
                .unwrap(),
            parse_article_markdown_str(&article_md(
                "인사관리규정",
                "제19조의2",
                "결원보충",
                "① 본문",
            ))
            .unwrap(),
            parse_article_markdown_str(&article_md("인사관리규정", "제20조", "임용", "① 본문"))
                .unwrap(),
        ];
        let index =
            TantivyRulesIndex::from_articles(articles, default_pack_status("cni", "2026-02-27"))
                .unwrap();

        let base = index.get_article("인사관리규정#제19조").unwrap();
        assert_eq!(base.next_id.as_deref(), Some("인사관리규정#제19조의2"));

        let sub = index.get_article("인사관리규정#제19조의2").unwrap();
        assert_eq!(sub.prev_id.as_deref(), Some("인사관리규정#제19조"));
        assert_eq!(sub.next_id.as_deref(), Some("인사관리규정#제20조"));

        let next = index.get_article("인사관리규정#제20조").unwrap();
        assert_eq!(next.prev_id.as_deref(), Some("인사관리규정#제19조의2"));
    }

    #[test]
    fn rejects_article_markdown_without_frontmatter() {
        let err = parse_article_markdown_str("본문만 있고 프론트매터가 없음").unwrap_err();
        assert!(matches!(err, RulesCoreError::InvalidArticle(_)));
    }

    #[test]
    fn rejects_article_markdown_with_unterminated_frontmatter() {
        let err = parse_article_markdown_str("---\nrule: 출장규정\n\n본문").unwrap_err();
        assert!(matches!(err, RulesCoreError::InvalidArticle(_)));
    }

    // --- 팩 sha256 위변조 (rules-core 자체 verify_manifest / from_pack_dir 경로) ---

    fn write_pack_fixture(root: &Path) -> PackManifest {
        let articles_dir = root.join("articles");
        fs::create_dir_all(&articles_dir).unwrap();
        let content = article_md("여비지급규칙", "제12조", "항공운임", "① 본문");
        fs::write(articles_dir.join("제12조.md"), &content).unwrap();

        let hash = sha256_file(&articles_dir.join("제12조.md")).unwrap();
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            source_url: None,
            quality: None,
            files: BTreeMap::from([("articles/제12조.md".to_string(), hash)]),
        };
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        manifest
    }

    #[test]
    fn verify_manifest_accepts_untampered_pack() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = write_pack_fixture(temp.path());
        verify_manifest(temp.path(), &manifest).unwrap();
    }

    #[test]
    fn verify_manifest_rejects_tampered_article_file() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = write_pack_fixture(temp.path());

        // Tamper the article after the manifest digest was computed.
        fs::write(
            temp.path().join("articles/제12조.md"),
            "attacker-modified content",
        )
        .unwrap();

        let err = verify_manifest(temp.path(), &manifest).unwrap_err();
        assert!(matches!(err, RulesCoreError::DigestMismatch { .. }));
    }

    #[test]
    fn verify_manifest_rejects_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let mut manifest = write_pack_fixture(temp.path());
        manifest
            .files
            .insert("articles/누락.md".to_string(), "0".repeat(64));

        let err = verify_manifest(temp.path(), &manifest).unwrap_err();
        assert!(matches!(err, RulesCoreError::MissingManifestEntry(_)));
    }

    #[test]
    fn from_pack_dir_rejects_tampered_pack_before_loading() {
        let temp = tempfile::tempdir().unwrap();
        write_pack_fixture(temp.path());
        fs::write(
            temp.path().join("articles/제12조.md"),
            "attacker-modified content",
        )
        .unwrap();

        let err = TantivyRulesIndex::from_pack_dir(temp.path()).unwrap_err();
        assert!(matches!(err, RulesCoreError::DigestMismatch { .. }));
    }

    #[test]
    fn from_pack_dir_loads_untampered_pack_and_is_searchable() {
        let temp = tempfile::tempdir().unwrap();
        write_pack_fixture(temp.path());

        let index = TantivyRulesIndex::from_pack_dir(temp.path()).unwrap();
        let hits = index.search("항공운임", 5, None);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].article_id, "여비지급규칙#제12조");
    }

    #[test]
    fn query_terms_keep_raw_and_particle_stripped_forms() {
        assert_eq!(
            query_terms("여비지급규칙은 어떤 목적으로 제정되었는가?"),
            vec![
                "여비지급규칙은".to_string(),
                "여비지급규칙".to_string(),
                "어떤".to_string(),
                "목적으로".to_string(),
                "목적".to_string(),
                "제정되었는가".to_string(),
                "제정되었".to_string()
            ]
        );
    }

    #[test]
    fn snippet_chooses_window_with_most_query_terms() {
        let body = "목적이라는 말만 먼저 나온다. 이 규칙은 충남연구원 여비 지급에 관한 사항을 정함을 목적으로 한다.";
        let result = snippet(
            body,
            "충남연구원 여비지급규칙은 어떤 목적으로 제정되었는가?",
        );

        assert!(result.contains("충남연구원"));
        assert!(result.contains("목적"));
    }

    #[test]
    fn rebuilding_same_rules_dir_produces_identical_search_orders() {
        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("crates/rules-core has a repository ancestor")
            .to_path_buf();
        let golden_path = repo.join("01_docs/eval/golden.jsonl");
        let rules_dir = repo.join("04_data/90_index-build/pack-cni-2026-02-27/articles");
        // 결정성 회귀 검증은 빌드 중간물(04_data/90_index-build/.../articles)에 의존한다.
        // 이 디렉터리는 파이프라인 산출물이며 git 미추적 — 배포 머신 등에는 없을 수 있다.
        // 픽스처가 없으면 패닉 대신 건너뛴다(이식성 확보). 픽스처가 있는 환경에서는 그대로 검증한다.
        if !golden_path.exists() || !rules_dir.exists() {
            eprintln!(
                "SKIP rebuilding_same_rules_dir_produces_identical_search_orders: fixture absent \
                 (golden={}, rules_dir={})",
                golden_path.display(),
                rules_dir.display()
            );
            return;
        }

        let queries = fs::read_to_string(&golden_path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .unwrap()
                    .get("q")
                    .and_then(|q| q.as_str())
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();

        let left = TantivyRulesIndex::from_articles_dir(
            &rules_dir,
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();
        let right = TantivyRulesIndex::from_articles_dir(
            &rules_dir,
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();

        for query in queries {
            let left_ids = left
                .search(&query, 5, None)
                .into_iter()
                .map(|hit| hit.article_id)
                .collect::<Vec<_>>();
            let right_ids = right
                .search(&query, 5, None)
                .into_iter()
                .map(|hit| hit.article_id)
                .collect::<Vec<_>>();
            assert_eq!(left_ids, right_ids, "query: {query}");
        }
    }

    /// Writes a synthetic pack (articles + annex + manifest) and returns its root.
    fn write_synthetic_pack(root: &Path) {
        let articles_dir = root.join("articles");
        let annexes_dir = root.join("annexes");
        fs::create_dir_all(&articles_dir).unwrap();
        fs::create_dir_all(&annexes_dir).unwrap();
        let mut files = BTreeMap::new();
        for (rule, no, title, body) in [
            (
                "시험규칙",
                "제1조",
                "목적",
                "① 이 규칙은 시험 운영에 필요한 사항을 정한다.",
            ),
            (
                "시험규칙",
                "제2조",
                "수당",
                "① 시험 위원에게 수당을 지급한다. 별표 1을 따른다.",
            ),
            (
                "가상법",
                "제3조",
                "위임",
                "① 시험 수당의 기준은 시험규칙으로 정한다.",
            ),
            (
                "가상법",
                "제4조",
                "보고",
                "① 시험 결과는 매년 보고한다. 시험규칙 제2조를 준용한다.",
            ),
        ] {
            let name = format!("{rule}_{no}.md");
            fs::write(articles_dir.join(&name), article_md(rule, no, title, body)).unwrap();
            files.insert(
                format!("articles/{name}"),
                sha256_file(&articles_dir.join(&name)).unwrap(),
            );
        }
        let annex = "---\ntype: annex\ninstitution: cni\nrule: 시험규칙\nannex: 별표1\ntitle: 시험 수당 기준\neffective: 2026-02-27\nstatus: active\ntable_structured: false\n---\n시험 위원 수당 기준표\n";
        fs::write(annexes_dir.join("시험규칙_별표1.md"), annex).unwrap();
        files.insert(
            "annexes/시험규칙_별표1.md".to_string(),
            sha256_file(&annexes_dir.join("시험규칙_별표1.md")).unwrap(),
        );
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            source_url: None,
            quality: None,
            files,
        };
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    /// Test-only reproduction of the pre-PH-04 loader: build once from articles,
    /// then rebuild the index with annexes.
    fn legacy_build(root: &Path) -> TantivyRulesIndex {
        let mut manifest: PackManifest =
            serde_json::from_reader(File::open(root.join("manifest.json")).unwrap()).unwrap();
        manifest.normalize_hashes();
        let articles = load_articles_dir(root.join("articles")).unwrap();
        let annexes = load_annexes_dir(root.join("annexes")).unwrap();
        let pages = load_pages_dir(root.join("pages")).unwrap();
        let nodes = load_jsonl::<GraphNode>(&root.join("graph/nodes.jsonl")).unwrap();
        let edges = load_jsonl::<GraphEdge>(&root.join("graph/edges.jsonl")).unwrap();
        let status = default_pack_status("cni", "2026-02-27");
        let mut index = TantivyRulesIndex::from_articles(articles, status).unwrap();
        index.annexes = annexes.into_iter().map(|a| (a.id.clone(), a)).collect();
        index.page_owners = build_page_owners(index.articles.values(), index.annexes.values());
        index.pages = pages;
        (index.index, index.fields, index.tokenizer_status) = build_search_index(
            index.articles.values(),
            index.annexes.values(),
            index.search_options.ko_pos_filter,
            None,
            false,
        )
        .unwrap();
        index.reader = SearchReader(index.index.reader().unwrap());
        let (graph, node_indices) = build_ref_graph(index.articles.values(), &nodes, &edges);
        index.graph = graph;
        index.node_indices = node_indices;
        index
    }

    #[test]
    fn pack_dir_load_builds_search_index_once() {
        let temp = tempfile::tempdir().unwrap();
        write_synthetic_pack(temp.path());
        SEARCH_INDEX_BUILDS.with(|c| c.set(0));
        let index = TantivyRulesIndex::from_pack_dir(temp.path()).unwrap();
        assert_eq!(SEARCH_INDEX_BUILDS.with(|c| c.get()), 1);
        assert!(!index.annexes.is_empty());
    }

    #[test]
    fn reader_is_reused_across_queries() {
        let index = TantivyRulesIndex::from_articles(
            fixture_articles(),
            default_pack_status("cni", "2026-02-27"),
        )
        .unwrap();
        let before = index.reader.0.searcher().generation().generation_id();
        for q in ["교통비", "연차휴가", "숙박비"] {
            let _ = index.search(q, 5, None);
            assert_eq!(
                index.reader.0.searcher().generation().generation_id(),
                before
            );
        }
    }

    #[test]
    fn precomputed_norm_cosine_is_bit_identical() {
        // Deterministic LCG so the test needs no rand dependency.
        let mut state: u64 = 0x2026_0929;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        };
        for _ in 0..1000 {
            let left: Vec<f32> = (0..384).map(|_| next()).collect();
            let right: Vec<f32> = (0..384).map(|_| next()).collect();
            let norm = squared_norm(&right).sqrt();
            let legacy = cosine_similarity(&left, &right).unwrap();
            let fast = cosine_similarity_with_norm(&left, &right, norm).unwrap();
            assert_eq!(legacy.to_bits(), fast.to_bits());
        }
        assert_eq!(cosine_similarity_with_norm(&[1.0], &[0.0], 0.0), None);
        assert_eq!(cosine_similarity_with_norm(&[1.0], &[1.0, 2.0], 1.0), None);
    }

    #[test]
    fn pack_dir_search_orders_identical_to_legacy_builder() {
        let temp = tempfile::tempdir().unwrap();
        write_synthetic_pack(temp.path());
        let new = TantivyRulesIndex::from_pack_dir(temp.path()).unwrap();
        let old = legacy_build(temp.path());
        let words = [
            "시험",
            "수당",
            "보고",
            "위원",
            "규칙",
            "별표 1",
            "시험규칙 제2조",
            "가상법",
            "목적",
            "위임",
            "기준표",
            "준용",
            "지급",
            "운영",
            "결과",
            "매년",
            "제3조",
            "시험규칙 별표 1",
            "수당 기준",
            "시험 위원",
            "보고 결과",
            "정한다",
            "따른다",
            "시험 운영",
            "위임 기준",
            "가상법 제4조",
            "수당 지급",
            "없는단어",
            "표",
            "사항",
        ];
        assert_eq!(words.len(), 30);
        for q in words {
            let a = new.search_with_routes(q, 20, None);
            let b = old.search_with_routes(q, 20, None);
            let key = |r: &SearchRouteReport| {
                r.hits
                    .iter()
                    .map(|h| (h.article_id.clone(), h.score.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(key(&a), key(&b), "query: {q}");
        }
    }

    fn pos_index(filter: KoPosFilter, bodies: &[&str]) -> TantivyRulesIndex {
        let articles = bodies
            .iter()
            .enumerate()
            .map(|(i, body)| {
                article_fixture("cni", "시험규칙", &format!("제{}조", i + 1), "목적", body)
            })
            .collect::<Vec<_>>();
        TantivyRulesIndex::from_articles_with_options(
            articles,
            default_pack_status("cni", "2026-03-01"),
            SearchOptions {
                ko_pos_filter: filter,
                ..SearchOptions::default()
            },
        )
        .unwrap()
    }

    fn ko_tokens(filter: KoPosFilter, text: &str) -> Vec<String> {
        use tantivy::tokenizer::TokenStream;
        let index = pos_index(filter, &["본문"]);
        let mut analyzer = index.index.tokenizers().get("ko").unwrap();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    fn without(tokens: &[String], removed: &[&str]) -> Vec<String> {
        tokens
            .iter()
            .filter(|t| !removed.iter().any(|r| **t == **r))
            .cloned()
            .collect()
    }

    const POS_SAMPLE: &str = "시험규칙에 따라 제3조 (적용 범위) 를 정하고 하는 시험을 끝나며 했다.";

    #[test]
    fn ko_pos_filter_tag_sets_are_exact() {
        assert!(KoPosFilter::None.stop_tags().is_empty());
        let mut v1 = KoPosFilter::V1.stop_tags();
        v1.sort_unstable();
        let mut expected_v1 = vec![
            "JKS", "JKC", "JKG", "JKO", "JKB", "JKV", "JKQ", "JX", "JC", "SF", "SE", "SSO", "SSC",
            "SC", "SY",
        ];
        expected_v1.sort_unstable();
        assert_eq!(v1, expected_v1);
        let mut v2 = KoPosFilter::V2.stop_tags();
        v2.sort_unstable();
        let mut expected_v2 = expected_v1.clone();
        expected_v2.extend(["EP", "EF", "EC", "ETN", "ETM"]);
        expected_v2.sort_unstable();
        assert_eq!(v2, expected_v2);
        for kept in [
            "SN", "SL", "SH", "NNG", "NNP", "VV", "VA", "MM", "MAG", "XR",
        ] {
            assert!(!expected_v2.contains(&kept), "{kept} must be preserved");
        }
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn pos_filter_v1_drops_particles_and_symbols_only() {
        let none = ko_tokens(KoPosFilter::None, POS_SAMPLE);
        let v1 = ko_tokens(KoPosFilter::V1, POS_SAMPLE);
        // particles (JKB/JKO), brackets and the sentence-final period are gone
        assert_eq!(v1, without(&none, &["에", "(", ")", "를", "을", "."]));
        // the number (SN) and the endings (EC/ETM/EF) survive v1
        for kept in ["3", "고", "는", "며", "다"] {
            assert!(v1.iter().any(|t| t == kept), "{kept} missing: {v1:?}");
        }
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn pos_filter_v2_also_drops_endings() {
        let v1 = ko_tokens(KoPosFilter::V1, POS_SAMPLE);
        let v2 = ko_tokens(KoPosFilter::V2, POS_SAMPLE);
        assert_eq!(v2, without(&v1, &["고", "는", "며", "다"]));
        assert!(v2.iter().any(|t| t == "3"));
        // compound tags such as VV+EC are not exact matches and stay (PH-00 probe)
        assert!(v2.iter().any(|t| t == "정하"));
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn pos_filter_never_drops_numbers_latin_hanja() {
        let text = "제12조 ABC test 法 version2 100";
        for filter in [KoPosFilter::V1, KoPosFilter::V2] {
            let tokens = ko_tokens(filter, text);
            for kept in ["12", "abc", "test", "法", "version", "2", "100"] {
                assert!(
                    tokens.iter().any(|t| t == kept),
                    "{filter:?}: {kept} missing in {tokens:?}"
                );
            }
        }
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn nfkc_and_lowercase_normalize_fullwidth_and_case() {
        let text = "ＡＢＣ Test １２";
        for filter in [KoPosFilter::V1, KoPosFilter::V2] {
            let tokens = ko_tokens(filter, text);
            for wanted in ["abc", "test", "12"] {
                assert!(
                    tokens.iter().any(|t| t == wanted),
                    "{filter:?}: {wanted} missing in {tokens:?}"
                );
            }
        }
        let legacy = ko_tokens(KoPosFilter::None, text);
        assert!(
            !legacy.iter().any(|t| t == "abc" || t == "12"),
            "{legacy:?}"
        );
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn query_and_document_share_analyzer() {
        use tantivy::collector::Count;
        // documents hold ASCII upper case / full-width; queries the other form.
        let bodies = ["ABC 시험 본문", "ＸＹＺ 시험 본문"];
        for (filter, expected) in [(KoPosFilter::V1, 1usize), (KoPosFilter::None, 0)] {
            let index = pos_index(filter, &bodies);
            let parser = QueryParser::for_index(&index.index, vec![index.fields.body]);
            let searcher = index.reader.0.searcher();
            for query in ["ＡＢＣ", "xyz"] {
                let parsed = parser.parse_query(query).unwrap();
                let hits = searcher.search(&parsed, &Count).unwrap();
                assert_eq!(hits, expected, "{filter:?} query {query}");
            }
        }
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn tokenizer_load_failure_is_reported_not_silent() {
        use tantivy::tokenizer::TokenStream;
        let index = Index::create_in_ram(Schema::builder().build());
        let status = register_ko_tokenizer_with(&index, KoPosFilter::V2, || {
            Err(lindera::error::LinderaErrorKind::NotFound.with_error(io::Error::other("no dict")))
        });
        assert_eq!(status.kind, "simple-fallback");
        assert!(status.degraded);
        assert_eq!(status.filter, "none");
        assert!(status.reason.as_deref().unwrap().contains("no dict"));
        // the fallback analyzer is registered so indexing keeps working
        let mut analyzer = index.tokenizers().get("ko").unwrap();
        let mut stream = analyzer.token_stream("가 나");
        assert!(stream.advance());

        let healthy = register_ko_tokenizer_with(&index, KoPosFilter::V1, || {
            lindera::dictionary::load_dictionary("embedded://ko-dic")
        });
        assert_eq!(healthy.kind, "lindera-ko-dic");
        assert!(!healthy.degraded);
        assert_eq!(healthy.filter, "v1");
        assert_eq!(healthy.reason, None);
    }

    #[cfg(feature = "korean-tokenizer")]
    #[test]
    fn default_filter_none_keeps_legacy_token_stream() {
        use lindera::mode::Mode;
        use lindera::segmenter::Segmenter;
        use tantivy::tokenizer::{TextAnalyzer, TokenStream};
        let legacy = lindera_tantivy::tokenizer::LinderaTokenizer::from_segmenter(Segmenter::new(
            Mode::Normal,
            lindera::dictionary::load_dictionary("embedded://ko-dic").unwrap(),
            None,
        ));
        let mut legacy = TextAnalyzer::builder(legacy).build();
        let index = TantivyRulesIndex::from_articles(
            vec![article_fixture("cni", "시험규칙", "제1조", "목적", "본문")],
            default_pack_status("cni", "2026-03-01"),
        )
        .unwrap();
        assert_eq!(SearchOptions::default().ko_pos_filter, KoPosFilter::None);
        assert_eq!(index.tokenizer_status().filter, "none");
        assert!(!index.tokenizer_status().degraded);
        let mut current = index.index.tokenizers().get("ko").unwrap();
        for text in [
            POS_SAMPLE,
            "ＡＢＣ Test １２ 시험을 하고",
            "가상법 제12조에 따른 위임",
        ] {
            let collect = |analyzer: &mut TextAnalyzer| {
                let mut stream = analyzer.token_stream(text);
                let mut out = Vec::new();
                while stream.advance() {
                    let t = stream.token();
                    out.push((t.text.clone(), t.offset_from, t.offset_to, t.position));
                }
                out
            };
            assert_eq!(collect(&mut legacy), collect(&mut current), "{text}");
        }
    }

    // --- PH-07 deterministic context injection ---

    fn ctx_node(id: &str, kind: NodeKind, label: &str) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            kind,
            label: label.to_string(),
            meta: BTreeMap::new(),
        }
    }

    fn ctx_edge(src: &str, dst: &str, kind: EdgeKind) -> GraphEdge {
        GraphEdge {
            src: src.to_string(),
            dst: dst.to_string(),
            kind,
            meta: BTreeMap::new(),
        }
    }

    fn ctx_options(context_prefix: ContextPrefix, e5_prefix: bool) -> SearchOptions {
        SearchOptions {
            context_prefix,
            e5_prefix,
            ..SearchOptions::default()
        }
    }

    fn ctx_index(
        articles: Vec<Article>,
        annexes: Vec<Annex>,
        nodes: &[GraphNode],
        edges: &[GraphEdge],
        options: SearchOptions,
    ) -> TantivyRulesIndex {
        TantivyRulesIndex::assemble(
            articles.into_iter().map(|a| (a.id.clone(), a)).collect(),
            annexes.into_iter().map(|a| (a.id.clone(), a)).collect(),
            None,
            nodes,
            edges,
            default_pack_status("cni", "2026-03-01"),
            &VectorSearchOptions::default(),
            options,
            false,
        )
        .unwrap()
    }

    fn bigram_options(weight: f32) -> SearchOptions {
        SearchOptions {
            bigram_weight: weight,
            ..SearchOptions::default()
        }
    }

    fn bigram_corpus() -> Vec<Article> {
        vec![
            article_fixture(
                "cni",
                "테스트규정",
                "제1조",
                "총칙",
                "쿼틱스람다파이 절차를 따른다.",
            ),
            article_fixture(
                "cni",
                "테스트규정",
                "제2조",
                "잡담",
                "무관한 내용의 조문이다.",
            ),
            article_fixture("cni", "시험규칙", "제1조", "보충", "다른 문장만 있다."),
        ]
    }

    fn bigram_index(weight: f32) -> TantivyRulesIndex {
        TantivyRulesIndex::from_articles_with_options(
            bigram_corpus(),
            default_pack_status("cni", "2026-03-01"),
            bigram_options(weight),
        )
        .unwrap()
    }

    #[test]
    fn bigram_list_recovers_spacing_variants() {
        let index = bigram_index(1.0);
        let target = "테스트규정#제1조";
        // The document has no space inside the compound; the query splits it
        // differently. The bigram list must still surface the document.
        let hits = index.bigram_rank("쿼틱 스람다 파이", 10, None);
        assert_eq!(hits.first().map(|h| h.article_id.as_str()), Some(target));
        assert!(hits.iter().all(|h| h.article_id != "테스트규정#제2조"));

        // A query in another spacing that shares no whole token with the body.
        let hits = index.bigram_rank("쿼틱스 람다파이", 10, None);
        assert_eq!(hits.first().map(|h| h.article_id.as_str()), Some(target));

        // Fused search: the document is returned with the bigram list on.
        let fused = index.search("쿼틱 스람다 파이", 5, None);
        assert!(fused.iter().any(|h| h.article_id == target));

        // Empty / whitespace-only queries produce no bigram list.
        assert!(index.bigram_rank("   ", 10, None).is_empty());
        // Disabled index computes nothing.
        assert!(bigram_index(0.0)
            .bigram_rank("쿼틱스 람다파이", 10, None)
            .is_empty());
    }

    #[test]
    fn bigram_weight_zero_is_bit_identical_to_baseline() {
        let baseline = TantivyRulesIndex::from_articles(
            bigram_corpus(),
            default_pack_status("cni", "2026-03-01"),
        )
        .unwrap();
        let zero = bigram_index(0.0);
        for q in [
            "쿼틱스 람다파이",
            "절차",
            "무관한 내용",
            "시험규칙 제1조",
            "없는말",
        ] {
            let a = baseline.search_with_routes(q, 10, None);
            let b = zero.search_with_routes(q, 10, None);
            let key = |r: &SearchRouteReport| {
                r.hits
                    .iter()
                    .map(|h| (h.article_id.clone(), h.score.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(key(&a), key(&b), "query {q}");
            let retrieval = |r: &SearchRouteReport| {
                r.retrieval_hits
                    .iter()
                    .map(|h| (h.article_id.clone(), h.score.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(retrieval(&a), retrieval(&b), "query {q}");
        }
        assert_eq!(SearchOptions::default().bigram_weight, 0.0);
    }

    #[test]
    fn bigram_field_not_indexed_when_disabled() {
        let off = bigram_index(0.0);
        assert!(off.fields.body_ng.is_none());
        assert!(off.index.schema().get_field("body_ng").is_err());
        assert!(off.index.tokenizers().get(BIGRAM_TOKENIZER).is_none());
        assert_eq!(off.index.schema().fields().count(), 5);

        let on = bigram_index(0.5);
        assert!(on.fields.body_ng.is_some());
        assert!(on.index.schema().get_field("body_ng").is_ok());
        assert_eq!(on.index.schema().fields().count(), 6);
        // Weight validation used by the config layer.
        assert!(validate_bigram_weight(0.0).is_ok());
        assert!(validate_bigram_weight(2.0).is_ok());
        assert!(validate_bigram_weight(-0.01).is_err());
        assert!(validate_bigram_weight(2.01).is_err());
        assert!(validate_bigram_weight(f32::NAN).is_err());
    }

    #[test]
    fn article_context_uses_institution_label_rule_and_number() {
        let nodes = [ctx_node("cni", NodeKind::Institution, "가상연구원")];
        let mut untitled = article_fixture("cni", "테스트규정", "제3조", "x", "본문");
        untitled.title = String::new();
        let articles = [
            article_fixture("cni", "테스트규정", "제1조", "목적", "본문"),
            untitled,
            // Institution without a node: the slug is the display name.
            article_fixture("zzz", "시험규칙", "제2조", "정의", "본문"),
        ];
        let map = articles.iter().map(|a| (a.id.clone(), a.clone())).collect();
        let (graph, indices) = build_ref_graph(articles.iter(), &nodes, &[]);
        let contexts = build_contexts(&map, &BTreeMap::new(), &graph, &indices, &nodes);
        assert_eq!(
            contexts["테스트규정#제1조"],
            "가상연구원 · 테스트규정 · 제1조(목적)"
        );
        // Empty title: no parentheses at all.
        assert_eq!(
            contexts["테스트규정#제3조"],
            "가상연구원 · 테스트규정 · 제3조"
        );
        // No Institution node: slug fallback.
        assert_eq!(contexts["시험규칙#제2조"], "zzz · 시험규칙 · 제2조(정의)");
    }

    #[test]
    fn annex_context_lists_referencing_article_titles_sorted_and_capped() {
        let rule = "테스트규정";
        // Seven referencing articles inserted out of order; 10 sorts after 9.
        let numbers = [10, 2, 1, 7, 3, 9, 5];
        let mut articles = numbers
            .iter()
            .map(|n| {
                article_fixture(
                    "cni",
                    rule,
                    &format!("제{n}조"),
                    &format!("제목{n}"),
                    "본문",
                )
            })
            .collect::<Vec<_>>();
        // Same rule name but another institution must not be listed.
        articles.push(article_fixture(
            "ctp",
            "외부규정",
            "제1조",
            "타기관",
            "본문",
        ));
        let annexes = vec![annex_fixture("cni", rule, "별표1", "시험표", "| a | b |")];
        let annex_id = annexes[0].id.clone();
        let mut edges = numbers
            .iter()
            .map(|n| ctx_edge(&format!("{rule}#제{n}조"), &annex_id, EdgeKind::Cites))
            .collect::<Vec<_>>();
        edges.push(ctx_edge("외부규정#제1조", &annex_id, EdgeKind::Cites));
        // Non-article sources and other edge kinds are ignored.
        edges.push(ctx_edge(
            &format!("rule:{rule}"),
            &annex_id,
            EdgeKind::AppliesTo,
        ));
        edges.push(ctx_edge(
            &format!("{rule}#제10조"),
            &annex_id,
            EdgeKind::Delegates,
        ));

        let index = ctx_index(
            articles.clone(),
            annexes.clone(),
            &[],
            &edges,
            ctx_options(ContextPrefix::Vector, false),
        );
        assert_eq!(
            index.contexts[&annex_id],
            format!("{rule} · 별표1 시험표 · 참조 조문: 제목1, 제목2, 제목3, 제목5, 제목7")
        );

        // No referencing article: the reference part is omitted.
        let lonely = ctx_index(
            vec![],
            annexes.clone(),
            &[],
            &[],
            ctx_options(ContextPrefix::Vector, false),
        );
        assert_eq!(lonely.contexts[&annex_id], format!("{rule} · 별표1 시험표"));

        // 200-character cap on multibyte titles, cut on a character boundary.
        let long_title = "가".repeat(60);
        let long_articles = (1..=5)
            .map(|n| article_fixture("cni", rule, &format!("제{n}조"), &long_title, "본문"))
            .collect::<Vec<_>>();
        let long_edges = (1..=5)
            .map(|n| ctx_edge(&format!("{rule}#제{n}조"), &annex_id, EdgeKind::AppliesTo))
            .collect::<Vec<_>>();
        let capped = ctx_index(
            long_articles,
            annexes,
            &[],
            &long_edges,
            ctx_options(ContextPrefix::Vector, false),
        );
        let context = &capped.contexts[&annex_id];
        let refs = context.split_once("참조 조문: ").unwrap().1;
        assert_eq!(refs.chars().count(), 200);
        assert!(
            refs.len() > 200,
            "multibyte titles: byte length exceeds 200"
        );
        assert!(refs.starts_with(&long_title));
    }

    fn ctx_articles() -> Vec<Article> {
        vec![article_fixture(
            "cni",
            "시험규칙",
            "제1조",
            "목적",
            "시험 본문",
        )]
    }

    fn ctx_nodes() -> Vec<GraphNode> {
        vec![ctx_node("cni", NodeKind::Institution, "구름연구소")]
    }

    #[test]
    fn context_prefix_vector_changes_only_passages() {
        let context = "구름연구소 · 시험규칙 · 제1조(목적)";
        for (e5, expected) in [
            (false, format!("{context}\n시험규칙 목적\n시험 본문\n")),
            (
                true,
                format!("passage: {context}\n시험규칙 목적\n시험 본문\n"),
            ),
        ] {
            let provider = RecordingProvider::default();
            let mut index = ctx_index(
                ctx_articles(),
                vec![],
                &ctx_nodes(),
                &[],
                ctx_options(ContextPrefix::Vector, e5),
            );
            index
                .enable_vectors_for_test(provider.clone(), "k", None, 60, 1.0)
                .unwrap();
            index.search("시험질의", 5, None);
            let texts = provider.texts.lock().unwrap().clone();
            assert_eq!(texts[0], expected, "e5={e5}");
            // The query side never carries the context.
            let query = if e5 {
                "query: 시험질의"
            } else {
                "시험질의"
            };
            assert_eq!(texts[1], query);
            assert_eq!(texts.len(), 2);
        }

        // BM25 side: results and the indexed body are those of `none`.
        let plain = ctx_index(
            ctx_articles(),
            vec![],
            &ctx_nodes(),
            &[],
            SearchOptions::default(),
        );
        let vector = ctx_index(
            ctx_articles(),
            vec![],
            &ctx_nodes(),
            &[],
            ctx_options(ContextPrefix::Vector, false),
        );
        for q in ["시험 본문", "구름연구소", "목적"] {
            assert_eq!(plain.search(q, 5, None), vector.search(q, 5, None), "{q}");
        }
        assert_eq!(body_field_hits(&vector, "구름연구소"), 0);
    }

    fn body_field_hits(index: &TantivyRulesIndex, term: &str) -> usize {
        let parser = QueryParser::for_index(&index.index, vec![index.fields.body]);
        let (query, _) = parser.parse_query_lenient(term);
        index
            .reader
            .0
            .searcher()
            .search(&query, &TopDocs::with_limit(10))
            .unwrap()
            .len()
    }

    fn title_field_hits(index: &TantivyRulesIndex, term: &str) -> usize {
        let parser = QueryParser::for_index(&index.index, vec![index.fields.title]);
        let (query, _) = parser.parse_query_lenient(term);
        index
            .reader
            .0
            .searcher()
            .search(&query, &TopDocs::with_limit(10))
            .unwrap()
            .len()
    }

    #[test]
    fn context_prefix_both_prefixes_bm25_body_only() {
        let label = "구름연구소";
        let none = ctx_index(
            ctx_articles(),
            vec![],
            &ctx_nodes(),
            &[],
            SearchOptions::default(),
        );
        let vector = ctx_index(
            ctx_articles(),
            vec![],
            &ctx_nodes(),
            &[],
            ctx_options(ContextPrefix::Vector, false),
        );
        let both = ctx_index(
            ctx_articles(),
            vec![],
            &ctx_nodes(),
            &[],
            ctx_options(ContextPrefix::Both, false),
        );
        assert_eq!(body_field_hits(&none, label), 0);
        assert_eq!(body_field_hits(&vector, label), 0);
        assert_eq!(body_field_hits(&both, label), 1);
        // title and rule fields are untouched; the stored article body too.
        assert_eq!(title_field_hits(&both, label), 0);
        assert_eq!(
            both.get_article("시험규칙#제1조").unwrap(),
            none.get_article("시험규칙#제1조").unwrap()
        );
        assert!(both
            .search(label, 5, None)
            .iter()
            .any(|h| h.article_id == "시험규칙#제1조"));
        assert!(none.search(label, 5, None).is_empty());

        // Vector passages equal the `vector` variant (same `ctx1` format).
        let record = |index: &mut TantivyRulesIndex| {
            let provider = RecordingProvider::default();
            index
                .enable_vectors_for_test(provider.clone(), "k", None, 60, 1.0)
                .unwrap();
            let texts = provider.texts.lock().unwrap().clone();
            texts
        };
        let mut vector = vector;
        let mut both = both;
        assert_eq!(record(&mut vector), record(&mut both));
    }

    #[test]
    fn context_strings_are_deterministic_across_rebuilds() {
        let rule = "테스트규정";
        let mut articles = (1..=6)
            .map(|n| {
                article_fixture(
                    "cni",
                    rule,
                    &format!("제{n}조"),
                    &format!("제목{n}"),
                    "본문",
                )
            })
            .collect::<Vec<_>>();
        let annexes = vec![annex_fixture("cni", rule, "별표1", "시험표", "| a |")];
        let annex_id = annexes[0].id.clone();
        let mut edges = (1..=6)
            .map(|n| ctx_edge(&format!("{rule}#제{n}조"), &annex_id, EdgeKind::AppliesTo))
            .collect::<Vec<_>>();
        let nodes = ctx_nodes();
        let first = ctx_index(
            articles.clone(),
            annexes.clone(),
            &nodes,
            &edges,
            ctx_options(ContextPrefix::Both, false),
        );
        // Rebuild with shuffled input order.
        articles.reverse();
        edges.reverse();
        let second = ctx_index(
            articles,
            annexes,
            &nodes,
            &edges,
            ctx_options(ContextPrefix::Both, false),
        );
        assert_eq!(first.contexts, second.contexts);
        assert_eq!(first.contexts.len(), 7);
        let texts = |index: &TantivyRulesIndex| {
            vector_source_entries(
                index.articles.values(),
                index.annexes.values(),
                PassageFormat::Ctx1,
                &index.contexts,
            )
        };
        assert_eq!(texts(&first), texts(&second));
    }

    #[test]
    fn context_prefix_uses_distinct_cache_key() {
        let manifest = test_manifest();
        let keys = [
            (SearchOptions::default(), None),
            (ctx_options(ContextPrefix::None, true), Some("e5p")),
            (ctx_options(ContextPrefix::Vector, false), Some("ctx1")),
            (ctx_options(ContextPrefix::Both, false), Some("ctx1")),
            (ctx_options(ContextPrefix::Vector, true), Some("e5p+ctx1")),
            (ctx_options(ContextPrefix::Both, true), Some("e5p+ctx1")),
        ];
        for (options, tag) in &keys {
            assert_eq!(options.passage_format().cache_tag(), *tag);
        }
        let key = |o: SearchOptions| pack_vector_cache_key(&manifest, o.passage_format());
        let legacy = key(SearchOptions::default());
        let e5 = key(ctx_options(ContextPrefix::None, true));
        let ctx1 = key(ctx_options(ContextPrefix::Vector, false));
        let both_e5 = key(ctx_options(ContextPrefix::Both, true));
        let all = [&legacy, &e5, &ctx1, &both_e5];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // `both` embeds exactly like `vector`, so it shares the vector cache file.
        assert_eq!(ctx1, key(ctx_options(ContextPrefix::Both, false)));
        // The cache file structure did not change, so the version stays put.
        assert_eq!(VECTOR_CACHE_VERSION, 1);

        // Separate files per format when built through the real cache path.
        let dir = tempfile::tempdir().unwrap();
        let index = ctx_index(
            ctx_articles(),
            vec![],
            &ctx_nodes(),
            &[],
            ctx_options(ContextPrefix::Vector, false),
        );
        for (k, fmt) in [
            (&legacy, PassageFormat::Legacy),
            (&ctx1, PassageFormat::Ctx1),
        ] {
            build_or_load_vector_corpus(
                index.articles.values(),
                index.annexes.values(),
                &RecordingProvider::default(),
                k,
                Some(dir.path()),
                fmt,
                &index.contexts,
            )
            .unwrap();
        }
        assert!(vector_cache_path(dir.path(), &legacy).is_file());
        assert!(vector_cache_path(dir.path(), &ctx1).is_file());
        // A legacy corpus never picks up context text and vice versa.
        let hash = |fmt| {
            vector_source_entries(
                index.articles.values(),
                index.annexes.values(),
                fmt,
                &index.contexts,
            )[0]
            .2
            .clone()
        };
        assert_ne!(hash(PassageFormat::Legacy), hash(PassageFormat::Ctx1));
    }
}
