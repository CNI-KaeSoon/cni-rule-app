use chrono::Utc;
use futures_util::StreamExt;
use pack_updater::ProgressEvent;
use rules_core::{
    default_pack_status, load_articles_dir, slugify_rule, Article, PackStatus, RuleFilter,
    RulesIndex, SearchHit, TantivyRulesIndex,
};
use rules_engines::{
    claude_cli_engine, codex_cli_engine, gemini_cli_engine, ApiKeyEngine, ChatDelta, ChatEngine,
    ChatRequest, ContextBlock, EngineKind, EngineStatus, Mode, Msg, Provider,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

pub const LABOR_MODE_NOTICE: &str =
    "이 도구는 내 상황이 무엇인지 최초 판단을 돕는 참고 도구입니다.";
pub const LABOR_DISCLAIMER: &str =
    "본 내용은 법률 자문이 아니며, 구체적인 사안은 노무사 등 전문가와 상담하시기 바랍니다.";
pub const RULES_DATA_MISSING_NOTICE: &str = "규정집 데이터가 아직 없습니다 — 지금 다운로드(1.2MB)";

pub struct AppState {
    db: Mutex<Database>,
    engine: Mutex<EngineKind>,
    rules_index: Mutex<Option<TantivyRulesIndex>>,
    data_dir: PathBuf,
    rules_dir: Mutex<Option<PathBuf>>,
    pack_dir: PathBuf,
    rulebook: Mutex<RulebookState>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rules index error: {0}")]
    RulesIndex(String),
    #[error("{0}")]
    DataMissing(String),
    #[error("engine error: {0}")]
    Engine(String),
    #[error("update error: {0}")]
    Update(String),
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

type CommandResult<T> = Result<T, AppError>;

fn engine_label(kind: &EngineKind) -> String {
    match kind {
        EngineKind::ChatGpt => "ChatGPT".to_string(),
        EngineKind::Claude => "Claude".to_string(),
        EngineKind::Gemini => "Gemini".to_string(),
        EngineKind::ApiKey(provider) => format!("API 키({provider:?})"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineStatusDto {
    pub kind: EngineKind,
    pub label: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationDto {
    pub id: String,
    pub title: String,
    pub mode: String,
    pub engine: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationDetailDto {
    pub conversation: ConversationDto,
    pub messages: Vec<MessageDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageDto {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatResponseDto {
    pub conversation_id: String,
    pub assistant_message_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatDeltaEvent {
    pub conversation_id: String,
    pub content: String,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatusDto {
    pub installed: bool,
    pub institution: String,
    pub effective_date: String,
    pub source_commit: String,
    pub index_built_at: String,
    pub stale: bool,
}

impl From<PackStatus> for UpdateStatusDto {
    fn from(status: PackStatus) -> Self {
        Self {
            installed: true,
            institution: status.institution,
            effective_date: status.effective_date,
            source_commit: status.source_commit,
            index_built_at: status.index_built_at,
            stale: status.stale,
        }
    }
}

impl UpdateStatusDto {
    fn missing() -> Self {
        Self {
            installed: false,
            institution: String::new(),
            effective_date: String::new(),
            source_commit: String::new(),
            index_built_at: String::new(),
            stale: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingDto {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionTelemetrySettingsDto {
    pub consent: Option<bool>,
    pub shared_dir: Option<String>,
    pub install_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionLogRecord {
    pub ts: String,
    pub question: String,
    pub mode: String,
    pub app_version: String,
    pub install_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuleFilterInput {
    pub institution: Option<String>,
    pub rule: Option<String>,
    pub status: Option<String>,
    pub effective: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArticleWithPagesDto {
    pub article: Article,
    pub source_pages: Vec<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulebookState {
    pub active: bool,
    pub page: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulebookOpenEvent {
    pub page: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RulebookDto {
    pub articles: Vec<ArticleWithPagesDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateProgressDto {
    pub stage: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchTraceHit {
    pub article_id: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerTraceDto {
    pub message_id: String,
    pub conversation_id: String,
    pub search_query: String,
    pub search_results: Vec<SearchTraceHit>,
    pub direct_routing: bool,
    pub search_ms: u128,
    pub context_article_ids: Vec<String>,
    pub prompt_bytes: usize,
    pub engine_kind: String,
    pub engine_delta_count: usize,
    pub engine_ms: u128,
    pub engine_exit_code: Option<i32>,
    pub engine_stderr_tail: String,
    pub extracted_citations: Vec<String>,
    pub citations_in_context: bool,
    pub total_ms: u128,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticExportDto {
    pub local_path: String,
    pub shared_path: Option<String>,
    pub shared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticReport {
    pub schema_version: u8,
    pub ts: String,
    pub app_version: String,
    pub install_id: String,
    pub reason: String,
    pub details: String,
    pub conversation_id: String,
    pub message_id: String,
    pub mode: String,
    pub question: String,
    pub answer: String,
    pub trace: Option<AnswerTraceDto>,
}

#[derive(Debug, Clone)]
pub struct NewAnswerTrace {
    pub message_id: String,
    pub conversation_id: String,
    pub search_query: String,
    pub search_results: Vec<SearchTraceHit>,
    pub direct_routing: bool,
    pub search_ms: u128,
    pub context_article_ids: Vec<String>,
    pub prompt_bytes: usize,
    pub engine_kind: String,
    pub engine_delta_count: usize,
    pub engine_ms: u128,
    pub engine_exit_code: Option<i32>,
    pub engine_stderr_tail: String,
    pub extracted_citations: Vec<String>,
    pub citations_in_context: bool,
    pub total_ms: u128,
}

#[derive(Debug, Clone)]
struct EngineCollection {
    content: String,
    delta_count: usize,
    error_tail: String,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn in_memory() -> Result<Self, AppError> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), AppError> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                mode TEXT NOT NULL,
                engine TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(conversation_id) REFERENCES conversations(id)
            );
            CREATE TABLE IF NOT EXISTS citations (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                article_id TEXT NULL,
                law_ref TEXT NULL,
                kind TEXT NULL,
                FOREIGN KEY(message_id) REFERENCES messages(id)
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS traces (
                message_id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                search_query TEXT NOT NULL,
                search_results_json TEXT NOT NULL,
                direct_routing INTEGER NOT NULL,
                search_ms INTEGER NOT NULL,
                context_article_ids_json TEXT NOT NULL,
                prompt_bytes INTEGER NOT NULL,
                engine_kind TEXT NOT NULL,
                engine_delta_count INTEGER NOT NULL,
                engine_ms INTEGER NOT NULL,
                engine_exit_code INTEGER NULL,
                engine_stderr_tail TEXT NOT NULL,
                extracted_citations_json TEXT NOT NULL,
                citations_in_context INTEGER NOT NULL,
                total_ms INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(message_id) REFERENCES messages(id),
                FOREIGN KEY(conversation_id) REFERENCES conversations(id)
            );
            CREATE INDEX IF NOT EXISTS idx_conversations_deleted_updated
                ON conversations(deleted_at, updated_at);
            CREATE INDEX IF NOT EXISTS idx_messages_conversation_created
                ON messages(conversation_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_traces_created
                ON traces(created_at DESC);
            ",
        )?;
        Ok(())
    }

    fn create_conversation(
        &self,
        title: Option<String>,
        mode: Mode,
        engine: String,
    ) -> Result<ConversationDto, AppError> {
        let now = now_iso();
        let id = Uuid::new_v4().to_string();
        let title = title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "새 대화".to_string());
        self.conn.execute(
            "INSERT INTO conversations(id, title, mode, engine, created_at, updated_at, deleted_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5, NULL)",
            params![id, title, mode_as_str(&mode), engine, now],
        )?;
        self.get_conversation(&id)?
            .ok_or_else(|| AppError::NotFound(id))
    }

    fn list_conversations(&self, include_deleted: bool) -> Result<Vec<ConversationDto>, AppError> {
        let sql = if include_deleted {
            "SELECT id, title, mode, engine, created_at, updated_at, deleted_at
             FROM conversations ORDER BY updated_at DESC"
        } else {
            "SELECT id, title, mode, engine, created_at, updated_at, deleted_at
             FROM conversations WHERE deleted_at IS NULL ORDER BY updated_at DESC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], map_conversation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AppError::from)
    }

    fn get_conversation(&self, id: &str) -> Result<Option<ConversationDto>, AppError> {
        self.conn
            .query_row(
                "SELECT id, title, mode, engine, created_at, updated_at, deleted_at
                 FROM conversations WHERE id = ?1",
                [id],
                map_conversation,
            )
            .optional()
            .map_err(AppError::from)
    }

    fn get_conversation_detail(&self, id: &str) -> Result<ConversationDetailDto, AppError> {
        let conversation = self
            .get_conversation(id)?
            .ok_or_else(|| AppError::NotFound(id.to_string()))?;
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, role, content, created_at
             FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC",
        )?;
        let messages = stmt
            .query_map([id], |row| {
                Ok(MessageDto {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ConversationDetailDto {
            conversation,
            messages,
        })
    }

    fn rename_conversation(&self, id: &str, title: String) -> Result<ConversationDto, AppError> {
        let now = now_iso();
        let updated = self.conn.execute(
            "UPDATE conversations SET title = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
            params![title, now, id],
        )?;
        if updated == 0 {
            return Err(AppError::NotFound(id.to_string()));
        }
        self.get_conversation(id)?
            .ok_or_else(|| AppError::NotFound(id.to_string()))
    }

    fn delete_to_trash(&self, id: &str) -> Result<ConversationDto, AppError> {
        let now = now_iso();
        let updated = self.conn.execute(
            "UPDATE conversations SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![now, id],
        )?;
        if updated == 0 {
            return Err(AppError::NotFound(id.to_string()));
        }
        self.get_conversation(id)?
            .ok_or_else(|| AppError::NotFound(id.to_string()))
    }

    fn add_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
    ) -> Result<MessageDto, AppError> {
        let exists = self
            .get_conversation(conversation_id)?
            .filter(|conversation| conversation.deleted_at.is_none())
            .is_some();
        if !exists {
            return Err(AppError::NotFound(conversation_id.to_string()));
        }
        let now = now_iso();
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO messages(id, conversation_id, role, content, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id, conversation_id, role, content, now],
        )?;
        self.conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            params![now, conversation_id],
        )?;
        Ok(MessageDto {
            id,
            conversation_id: conversation_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: now,
        })
    }

    fn add_citations(
        &self,
        message_id: &str,
        citations: &[CitationRecord],
    ) -> Result<(), AppError> {
        for citation in citations {
            self.conn.execute(
                "INSERT INTO citations(id, message_id, article_id, law_ref, kind)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    Uuid::new_v4().to_string(),
                    message_id,
                    citation.article_id,
                    citation.law_ref,
                    citation.kind
                ],
            )?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn citations_for_message(&self, message_id: &str) -> Result<Vec<CitationRecord>, AppError> {
        let mut stmt = self.conn.prepare(
            "SELECT article_id, law_ref, kind FROM citations
             WHERE message_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([message_id], |row| {
            Ok(CitationRecord {
                article_id: row.get(0)?,
                law_ref: row.get(1)?,
                kind: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AppError::from)
    }

    fn export_md(&self, id: &str) -> Result<String, AppError> {
        let detail = self.get_conversation_detail(id)?;
        let mut out = format!("# {}\n\n", detail.conversation.title);
        for message in detail.messages {
            out.push_str(&format!("## {}\n\n{}\n\n", message.role, message.content));
        }
        Ok(out)
    }

    fn setting_get(&self, key: &str) -> Result<Option<String>, AppError> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(AppError::from)
    }

    fn setting_set(&self, key: String, value: String) -> Result<SettingDto, AppError> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(SettingDto { key, value })
    }

    fn setting_list(&self) -> Result<Vec<SettingDto>, AppError> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM settings ORDER BY key")?;
        let rows = stmt.query_map([], |row| {
            Ok(SettingDto {
                key: row.get(0)?,
                value: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AppError::from)
    }

    fn add_trace(&self, trace: &NewAnswerTrace) -> Result<AnswerTraceDto, AppError> {
        let created_at = now_iso();
        self.conn.execute(
            "INSERT INTO traces(
                message_id, conversation_id, search_query, search_results_json, direct_routing,
                search_ms, context_article_ids_json, prompt_bytes, engine_kind, engine_delta_count,
                engine_ms, engine_exit_code, engine_stderr_tail, extracted_citations_json,
                citations_in_context, total_ms, created_at
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                trace.message_id,
                trace.conversation_id,
                trace.search_query,
                serde_json::to_string(&trace.search_results)?,
                trace.direct_routing,
                trace.search_ms.to_string(),
                serde_json::to_string(&trace.context_article_ids)?,
                trace.prompt_bytes.to_string(),
                trace.engine_kind,
                trace.engine_delta_count.to_string(),
                trace.engine_ms.to_string(),
                trace.engine_exit_code,
                trace.engine_stderr_tail,
                serde_json::to_string(&trace.extracted_citations)?,
                trace.citations_in_context,
                trace.total_ms.to_string(),
                created_at,
            ],
        )?;
        self.get_trace(&trace.message_id)?
            .ok_or_else(|| AppError::NotFound(trace.message_id.clone()))
    }

    fn recent_traces(&self, limit: usize) -> Result<Vec<AnswerTraceDto>, AppError> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, conversation_id, search_query, search_results_json, direct_routing,
                    search_ms, context_article_ids_json, prompt_bytes, engine_kind, engine_delta_count,
                    engine_ms, engine_exit_code, engine_stderr_tail, extracted_citations_json,
                    citations_in_context, total_ms, created_at
             FROM traces ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit.to_string()], map_trace)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AppError::from)
    }

    fn get_trace(&self, message_id: &str) -> Result<Option<AnswerTraceDto>, AppError> {
        self.conn
            .query_row(
                "SELECT message_id, conversation_id, search_query, search_results_json, direct_routing,
                        search_ms, context_article_ids_json, prompt_bytes, engine_kind, engine_delta_count,
                        engine_ms, engine_exit_code, engine_stderr_tail, extracted_citations_json,
                        citations_in_context, total_ms, created_at
                 FROM traces WHERE message_id = ?1",
                [message_id],
                map_trace,
            )
            .optional()
            .map_err(AppError::from)
    }
}

fn map_conversation(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationDto> {
    Ok(ConversationDto {
        id: row.get(0)?,
        title: row.get(1)?,
        mode: row.get(2)?,
        engine: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        deleted_at: row.get(6)?,
    })
}

fn parse_json_column<T: serde::de::DeserializeOwned>(raw: String) -> rusqlite::Result<T> {
    serde_json::from_str(&raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn map_trace(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnswerTraceDto> {
    let search_results: String = row.get(3)?;
    let context_article_ids: String = row.get(6)?;
    let extracted_citations: String = row.get(13)?;
    let search_ms: i64 = row.get(5)?;
    let prompt_bytes: i64 = row.get(7)?;
    let engine_delta_count: i64 = row.get(9)?;
    let engine_ms: i64 = row.get(10)?;
    let total_ms: i64 = row.get(15)?;
    Ok(AnswerTraceDto {
        message_id: row.get(0)?,
        conversation_id: row.get(1)?,
        search_query: row.get(2)?,
        search_results: parse_json_column(search_results)?,
        direct_routing: row.get(4)?,
        search_ms: search_ms.max(0) as u128,
        context_article_ids: parse_json_column(context_article_ids)?,
        prompt_bytes: prompt_bytes.max(0) as usize,
        engine_kind: row.get(8)?,
        engine_delta_count: engine_delta_count.max(0) as usize,
        engine_ms: engine_ms.max(0) as u128,
        engine_exit_code: row.get(11)?,
        engine_stderr_tail: row.get(12)?,
        extracted_citations: parse_json_column(extracted_citations)?,
        citations_in_context: row.get(14)?,
        total_ms: total_ms.max(0) as u128,
        created_at: row.get(16)?,
    })
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationRecord {
    pub article_id: Option<String>,
    pub law_ref: Option<String>,
    pub kind: Option<String>,
}

fn mode_as_str(mode: &Mode) -> &'static str {
    match mode {
        Mode::Interpret => "Interpret",
        Mode::Labor => "Labor",
        Mode::Compare => "Compare",
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub fn append_labor_disclaimer(mode: &Mode, content: &str) -> String {
    if !matches!(mode, Mode::Labor) || content.contains(LABOR_DISCLAIMER) {
        return content.to_string();
    }
    format!("{content}\n\n{LABOR_DISCLAIMER}")
}

fn load_index_from_rules_dir(rules_dir: &Path) -> Result<TantivyRulesIndex, AppError> {
    TantivyRulesIndex::from_articles_dir(rules_dir, default_pack_status("cni", "dev"))
        .map_err(|err| AppError::RulesIndex(err.to_string()))
}

fn with_index<T>(state: &AppState, f: impl FnOnce(&TantivyRulesIndex) -> T) -> Result<T, AppError> {
    let mut index = lock_or_recover(&state.rules_index);
    if index.is_none() {
        let rules_dir = active_rules_dir(state)?;
        *index = Some(load_index_from_rules_dir(&rules_dir)?);
    }
    Ok(f(index.as_ref().expect("index initialized")))
}

fn active_rules_dir(state: &AppState) -> Result<PathBuf, AppError> {
    lock_or_recover(&state.rules_dir)
        .clone()
        .ok_or_else(|| AppError::DataMissing(RULES_DATA_MISSING_NOTICE.to_string()))
}

fn install_index_from_pack_dir(
    state: &AppState,
    pack_dir: &Path,
) -> Result<UpdateStatusDto, AppError> {
    let index = TantivyRulesIndex::from_pack_dir(pack_dir)
        .map_err(|err| AppError::RulesIndex(err.to_string()))?;
    let status = UpdateStatusDto::from(index.status());
    *lock_or_recover(&state.rules_dir) = Some(pack_dir.join("articles"));
    *lock_or_recover(&state.rules_index) = Some(index);
    Ok(status)
}

fn filter_from_dto(filter: Option<RuleFilterInput>) -> Option<RuleFilter> {
    filter.map(|value| RuleFilter {
        institution: value.institution.filter(|v| !v.trim().is_empty()),
        rule: value.rule.filter(|v| !v.trim().is_empty()),
        status: value.status.filter(|v| !v.trim().is_empty()),
        effective: value.effective.filter(|v| !v.trim().is_empty()),
    })
}

fn selected_engine(kind: &EngineKind) -> Box<dyn ChatEngine> {
    match kind {
        EngineKind::ChatGpt => Box::new(codex_cli_engine()),
        EngineKind::Claude => Box::new(claude_cli_engine()),
        EngineKind::Gemini => Box::new(gemini_cli_engine()),
        EngineKind::ApiKey(Provider::OpenAi) => Box::new(ApiKeyEngine::openai("gpt-4.1-mini")),
        EngineKind::ApiKey(provider) => Box::new(ApiKeyEngine::openai_compatible(
            provider.clone(),
            "default",
            "",
            "rules-engines-custom-api-key",
            "default",
        )),
    }
}

fn status_label(status: EngineStatus) -> String {
    match status {
        EngineStatus::Installed => "Installed",
        EngineStatus::NeedsLogin => "NeedsLogin",
        EngineStatus::Ready => "Ready",
        EngineStatus::Missing => "Missing",
    }
    .to_string()
}

fn context_from_hits(index: &TantivyRulesIndex, hits: &[SearchHit]) -> Vec<ContextBlock> {
    hits.iter()
        .filter_map(|hit| index.get_article(&hit.article_id))
        .map(|article| ContextBlock {
            id: article.id,
            title: format!("{} {}", article.rule, article.article),
            body: article.body,
            source: article.effective,
        })
        .collect()
}

fn citations_from_content(content: &str, context: &[ContextBlock]) -> Vec<CitationRecord> {
    let context_ids = context
        .iter()
        .map(|block| block.id.as_str())
        .collect::<Vec<_>>();
    extract_article_refs(content)
        .into_iter()
        .filter(|id| context_ids.iter().any(|context_id| context_id == id))
        .map(|id| CitationRecord {
            article_id: Some(id),
            law_ref: None,
            kind: Some("answer".to_string()),
        })
        .collect()
}

fn extract_article_refs(content: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for candidate in content.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '[' | ']' | '(' | ')' | ',' | '.' | ';' | ':' | '"' | '\'' | '“' | '”'
            )
    }) {
        let trimmed = candidate.trim();
        if trimmed.contains("#제") && trimmed.contains('조') && !refs.iter().any(|id| id == trimmed)
        {
            refs.push(trimmed.to_string());
        }
    }
    refs
}

fn citations_are_in_context(citations: &[String], context_ids: &[String]) -> bool {
    citations
        .iter()
        .all(|citation| context_ids.iter().any(|id| id == citation))
}

fn prompt_bytes_for_trace(text: &str, mode: &Mode, context: &[ContextBlock]) -> usize {
    let context_bytes = context
        .iter()
        .map(|block| block.id.len() + block.title.len() + block.body.len() + block.source.len())
        .sum::<usize>();
    mode_as_str(mode).len() + text.len() + context_bytes
}

fn last_chars(input: &str, max_chars: usize) -> String {
    let len = input.chars().count();
    input
        .chars()
        .skip(len.saturating_sub(max_chars))
        .collect::<String>()
}

fn engine_error_tail(content: &str) -> String {
    let markers = [
        "engine process failed to start",
        "engine process exited",
        "engine stdin was unavailable",
        "engine stdin write failed",
        "engine stdout was unavailable",
        "engine stdout read failed",
        "engine process timed out",
    ];
    content
        .lines()
        .rfind(|line| markers.iter().any(|marker| line.contains(marker)))
        .map(|line| last_chars(line, 500))
        .unwrap_or_default()
}

fn trace_hits_from_search(hits: &[SearchHit]) -> Vec<SearchTraceHit> {
    hits.iter()
        .map(|hit| SearchTraceHit {
            article_id: hit.article_id.clone(),
            score: hit.score,
        })
        .collect()
}

fn engine_kind_for_trace(kind: &EngineKind) -> String {
    serde_json::to_string(kind).unwrap_or_else(|_| engine_label(kind))
}

async fn collect_engine_stream(
    mut stream: rules_engines::BoxStream<ChatDelta>,
    mode: &Mode,
    mut emit: impl FnMut(ChatDelta),
) -> EngineCollection {
    let mut content = String::new();
    let mut delta_count = 0_usize;
    while let Some(delta) = stream.next().await {
        if !delta.content.is_empty() {
            delta_count += 1;
            content.push_str(&delta.content);
        }
        emit(delta);
    }
    let content = append_labor_disclaimer(mode, &content);
    let error_tail = engine_error_tail(&content);
    EngineCollection {
        content,
        delta_count,
        error_tail,
    }
}

fn source_pages_for_article(rules_dir: &Path, article: &Article) -> Vec<u32> {
    let path = rules_dir
        .join(slugify_rule(&article.rule))
        .join(format!("{}.md", article.article));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|input| parse_source_pages(&input))
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct SourcePagesFrontmatter {
    #[serde(default)]
    source_pages: Vec<u32>,
}

fn parse_source_pages(input: &str) -> Option<Vec<u32>> {
    let rest = input.strip_prefix("---\n")?;
    let (frontmatter, _) = rest.split_once("\n---")?;
    serde_yaml::from_str::<SourcePagesFrontmatter>(frontmatter)
        .ok()
        .map(|parsed| parsed.source_pages)
}

fn update_rulebook_state(state: &mut RulebookState, page: u32) {
    state.active = true;
    state.page = Some(page);
}

fn update_progress_dto(event: &ProgressEvent) -> UpdateProgressDto {
    match event {
        ProgressEvent::CheckingLatestRelease => UpdateProgressDto {
            stage: "checking".to_string(),
            message: "최신 규정팩 확인 중".to_string(),
        },
        ProgressEvent::DownloadStarted { asset_name, .. } => UpdateProgressDto {
            stage: "download-started".to_string(),
            message: format!("{asset_name} 다운로드 시작"),
        },
        ProgressEvent::Downloaded { bytes, total_bytes } => UpdateProgressDto {
            stage: "downloading".to_string(),
            message: total_bytes
                .map(|total| format!("{bytes}/{total} bytes"))
                .unwrap_or_else(|| format!("{bytes} bytes")),
        },
        ProgressEvent::DownloadFinished { bytes } => UpdateProgressDto {
            stage: "download-finished".to_string(),
            message: format!("{bytes} bytes 다운로드 완료"),
        },
        ProgressEvent::Unpacking => UpdateProgressDto {
            stage: "unpacking".to_string(),
            message: "규정팩 압축 해제 중".to_string(),
        },
        ProgressEvent::Verifying { path } => UpdateProgressDto {
            stage: "verifying".to_string(),
            message: format!("{path} 검증 중"),
        },
        ProgressEvent::Installing => UpdateProgressDto {
            stage: "installing".to_string(),
            message: "규정팩 설치 중".to_string(),
        },
        ProgressEvent::Installed { target_dir } => UpdateProgressDto {
            stage: "installed".to_string(),
            message: target_dir.display().to_string(),
        },
    }
}

#[cfg(debug_assertions)]
fn dev_rules_dir() -> PathBuf {
    std::env::var_os("CNI_RULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../04_data/90_index-build/rules"))
}

fn app_pack_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("rules-pack")
}

fn initial_rules_dir(pack_dir: &Path) -> Option<PathBuf> {
    let packed_articles = pack_dir.join("articles");
    if packed_articles.is_dir() {
        return Some(packed_articles);
    }

    #[cfg(debug_assertions)]
    {
        let dev_dir = dev_rules_dir();
        if dev_dir.is_dir() {
            return Some(dev_dir);
        }
    }

    None
}

fn initial_update_status(pack_dir: &Path, rules_dir: Option<&Path>) -> UpdateStatusDto {
    if pack_dir.join("articles").is_dir() {
        return TantivyRulesIndex::from_pack_dir(pack_dir)
            .map(|index| index.status().into())
            .unwrap_or_else(|_| UpdateStatusDto::missing());
    }
    rules_dir
        .and_then(|path| load_index_from_rules_dir(path).ok())
        .map(|index| index.status().into())
        .unwrap_or_else(UpdateStatusDto::missing)
}

const QUESTION_LOG_CONSENT_KEY: &str = "question_log_consent";
const QUESTION_LOG_SHARED_DIR_KEY: &str = "question_log_shared_dir";
const QUESTION_LOG_INSTALL_ID_KEY: &str = "question_log_install_id";
const QUESTION_LOG_DIR: &str = "question-logs";
const QUESTION_LOG_FILE: &str = "questions.jsonl";

fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn bool_setting(value: Option<String>) -> Option<bool> {
    value.and_then(|raw| match raw.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    })
}

fn ensure_install_id(db: &Database) -> Result<String, AppError> {
    if let Some(existing) = db.setting_get(QUESTION_LOG_INSTALL_ID_KEY)? {
        if !existing.trim().is_empty() {
            return Ok(existing);
        }
    }
    let install_id = Uuid::new_v4().to_string();
    db.setting_set(QUESTION_LOG_INSTALL_ID_KEY.to_string(), install_id.clone())?;
    Ok(install_id)
}

fn question_telemetry_settings(db: &Database) -> Result<QuestionTelemetrySettingsDto, AppError> {
    Ok(QuestionTelemetrySettingsDto {
        consent: bool_setting(db.setting_get(QUESTION_LOG_CONSENT_KEY)?),
        shared_dir: db
            .setting_get(QUESTION_LOG_SHARED_DIR_KEY)?
            .filter(|value| !value.trim().is_empty()),
        install_id: ensure_install_id(db)?,
    })
}

fn set_question_telemetry_settings(
    db: &Database,
    consent: bool,
    shared_dir: Option<String>,
) -> Result<QuestionTelemetrySettingsDto, AppError> {
    db.setting_set(QUESTION_LOG_CONSENT_KEY.to_string(), consent.to_string())?;
    db.setting_set(
        QUESTION_LOG_SHARED_DIR_KEY.to_string(),
        shared_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("")
            .to_string(),
    )?;
    question_telemetry_settings(db)
}

fn local_question_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(QUESTION_LOG_DIR).join(QUESTION_LOG_FILE)
}

fn local_diagnostics_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("diagnostics")
}

fn local_error_log_path(data_dir: &Path) -> PathBuf {
    local_diagnostics_dir(data_dir).join("errors.jsonl")
}

fn append_technical_error(
    data_dir: &Path,
    shared_dir: Option<&Path>,
    error_type: &str,
    message: &str,
) -> Result<Option<PathBuf>, AppError> {
    let local_path = local_error_log_path(data_dir);
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&local_path)?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "ts": now_iso(),
            "app_version": app_version(),
            "type": error_type,
            "message": message,
        }),
    )?;
    file.write_all(b"\n")?;
    file.flush()?;

    if let Some(shared_dir) = shared_dir {
        let target_dir = shared_dir.join("diagnostics");
        fs::create_dir_all(&target_dir)?;
        let target = target_dir.join("errors.jsonl");
        fs::copy(&local_path, &target)?;
        return Ok(Some(target));
    }
    Ok(None)
}

fn record_technical_error_if_allowed(
    db: &Database,
    data_dir: &Path,
    error_type: &str,
    message: &str,
) -> Result<(), AppError> {
    let settings = question_telemetry_settings(db)?;
    let shared_dir = if settings.consent == Some(true) {
        settings.shared_dir.as_deref().map(Path::new)
    } else {
        None
    };
    append_technical_error(data_dir, shared_dir, error_type, message)?;
    Ok(())
}

fn previous_user_message(
    messages: &[MessageDto],
    assistant_message_id: &str,
) -> Option<MessageDto> {
    let assistant_index = messages
        .iter()
        .position(|message| message.id == assistant_message_id)?;
    messages[..assistant_index]
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .cloned()
}

fn diagnostic_file_name(message_id: &str) -> String {
    format!(
        "diagnostic-{}-{}.json",
        now_iso().replace(':', ""),
        message_id
    )
}

fn export_diagnostic_report_file(
    data_dir: &Path,
    shared_dir: Option<&Path>,
    report: &DiagnosticReport,
) -> Result<DiagnosticExportDto, AppError> {
    let local_dir = local_diagnostics_dir(data_dir);
    fs::create_dir_all(&local_dir)?;
    let file_name = diagnostic_file_name(&report.message_id);
    let local_path = local_dir.join(&file_name);
    fs::write(&local_path, serde_json::to_vec_pretty(report)?)?;

    let shared_path = if let Some(shared_dir) = shared_dir {
        let target_dir = shared_dir.join("diagnostics");
        fs::create_dir_all(&target_dir)?;
        let target = target_dir.join(file_name);
        fs::copy(&local_path, &target)?;
        Some(target)
    } else {
        None
    };

    Ok(DiagnosticExportDto {
        local_path: local_path.display().to_string(),
        shared: shared_path.is_some(),
        shared_path: shared_path.map(|path| path.display().to_string()),
    })
}

fn diagnostic_report_for_message(
    db: &Database,
    message_id: &str,
    reason: &str,
    details: &str,
) -> Result<DiagnosticReport, AppError> {
    let trace = db.get_trace(message_id)?;
    let conversation_id = trace
        .as_ref()
        .map(|trace| trace.conversation_id.clone())
        .or_else(|| {
            db.conn
                .query_row(
                    "SELECT conversation_id FROM messages WHERE id = ?1",
                    [message_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .ok()
                .flatten()
        })
        .ok_or_else(|| AppError::NotFound(message_id.to_string()))?;
    let detail = db.get_conversation_detail(&conversation_id)?;
    let answer = detail
        .messages
        .iter()
        .find(|message| message.id == message_id && message.role == "assistant")
        .cloned()
        .ok_or_else(|| AppError::NotFound(message_id.to_string()))?;
    let question = previous_user_message(&detail.messages, message_id)
        .map(|message| message.content)
        .unwrap_or_default();
    Ok(DiagnosticReport {
        schema_version: 1,
        ts: now_iso(),
        app_version: app_version().to_string(),
        install_id: ensure_install_id(db)?,
        reason: reason.to_string(),
        details: details.to_string(),
        conversation_id,
        message_id: message_id.to_string(),
        mode: detail.conversation.mode,
        question,
        answer: answer.content,
        trace,
    })
}

fn append_question_log(
    data_dir: &Path,
    shared_dir: Option<&Path>,
    record: &QuestionLogRecord,
) -> Result<Option<PathBuf>, AppError> {
    let local_path = local_question_log_path(data_dir);
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&local_path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.flush()?;

    if let Some(shared_dir) = shared_dir {
        fs::create_dir_all(shared_dir)?;
        let target = shared_dir.join(format!("qlog-{}.jsonl", record.install_id));
        let temp = shared_dir.join(format!(
            ".qlog-{}.{}.tmp",
            record.install_id,
            Uuid::new_v4()
        ));
        fs::copy(&local_path, &temp)?;
        fs::rename(&temp, &target)?;
        return Ok(Some(target));
    }

    Ok(None)
}

fn record_question_if_allowed(
    db: &Database,
    data_dir: &Path,
    mode: &Mode,
    question: &str,
) -> Result<(), AppError> {
    if matches!(mode, Mode::Labor) {
        return Ok(());
    }
    let settings = question_telemetry_settings(db)?;
    if settings.consent != Some(true) {
        return Ok(());
    }
    let record = QuestionLogRecord {
        ts: now_iso(),
        question: question.to_string(),
        mode: mode_as_str(mode).to_string(),
        app_version: app_version().to_string(),
        install_id: settings.install_id,
    };
    let shared_dir = settings.shared_dir.as_deref().map(Path::new);
    append_question_log(data_dir, shared_dir, &record)?;
    Ok(())
}

fn load_rulebook(rules_dir: &Path) -> Result<RulebookDto, AppError> {
    let articles =
        load_articles_dir(rules_dir).map_err(|err| AppError::RulesIndex(err.to_string()))?;
    Ok(RulebookDto {
        articles: articles
            .into_iter()
            .map(|article| ArticleWithPagesDto {
                source_pages: source_pages_for_article(rules_dir, &article),
                article,
            })
            .collect(),
    })
}

#[tauri::command]
fn search(
    state: State<'_, AppState>,
    q: String,
    filter: Option<RuleFilterInput>,
) -> CommandResult<Vec<SearchHit>> {
    with_index(&state, |index| index.search(&q, 5, filter_from_dto(filter)))
}

#[tauri::command]
fn get_article(state: State<'_, AppState>, id: String) -> CommandResult<ArticleWithPagesDto> {
    let rules_dir = active_rules_dir(&state)?;
    with_index(&state, |index| index.get_article(&id))?
        .map(|article| ArticleWithPagesDto {
            source_pages: source_pages_for_article(&rules_dir, &article),
            article,
        })
        .ok_or(AppError::NotFound(id))
}

#[tauri::command]
async fn send_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    mode: Mode,
    text: String,
) -> CommandResult<ChatResponseDto> {
    let total_start = Instant::now();
    {
        let db = lock_or_recover(&state.db);
        db.add_message(&conversation_id, "user", &text)?;
        record_question_if_allowed(&db, &state.data_dir, &mode, &text)?;
    }
    if active_rules_dir(&state).is_err() {
        let db = lock_or_recover(&state.db);
        let message = db.add_message(&conversation_id, "assistant", RULES_DATA_MISSING_NOTICE)?;
        return Ok(ChatResponseDto {
            conversation_id,
            assistant_message_id: message.id,
            content: RULES_DATA_MISSING_NOTICE.to_string(),
        });
    }
    let (engine_kind, hits, context, search_ms) = {
        let engine_kind = lock_or_recover(&state.engine).clone();
        let search_start = Instant::now();
        let (hits, context) = with_index(&state, |index| {
            let hits = index.search(&text, 5, None);
            let context = context_from_hits(index, &hits);
            (hits, context)
        })?;
        (
            engine_kind,
            hits,
            context,
            search_start.elapsed().as_millis(),
        )
    };
    let search_results = trace_hits_from_search(&hits);
    let context_article_ids = context
        .iter()
        .map(|block| block.id.clone())
        .collect::<Vec<_>>();
    let prompt_bytes = prompt_bytes_for_trace(&text, &mode, &context);
    let request = ChatRequest {
        mode,
        messages: vec![Msg {
            role: "user".to_string(),
            content: text.clone(),
        }],
        context: context.clone(),
    };
    let stream = selected_engine(&engine_kind).send(request);
    let conversation_for_emit = conversation_id.clone();
    let engine_start = Instant::now();
    let engine_collection = collect_engine_stream(stream, &mode, |delta| {
        let emitted = if delta.done {
            ChatDeltaEvent {
                conversation_id: conversation_for_emit.clone(),
                content: String::new(),
                done: true,
            }
        } else {
            ChatDeltaEvent {
                conversation_id: conversation_for_emit.clone(),
                content: delta.content,
                done: false,
            }
        };
        app.emit("chat://delta", emitted).ok();
    })
    .await;
    let engine_ms = engine_start.elapsed().as_millis();
    let content = engine_collection.content;
    if !engine_collection.error_tail.is_empty() {
        let db = lock_or_recover(&state.db);
        record_technical_error_if_allowed(
            &db,
            &state.data_dir,
            "engine",
            &engine_collection.error_tail,
        )?;
    }
    let assistant_message = {
        let db = lock_or_recover(&state.db);
        let message = db.add_message(&conversation_id, "assistant", &content)?;
        let citations = citations_from_content(&content, &context);
        db.add_citations(&message.id, &citations)?;
        let extracted_citations = extract_article_refs(&content);
        db.add_trace(&NewAnswerTrace {
            message_id: message.id.clone(),
            conversation_id: conversation_id.clone(),
            search_query: text.clone(),
            search_results,
            direct_routing: false,
            search_ms,
            context_article_ids: context_article_ids.clone(),
            prompt_bytes,
            engine_kind: engine_kind_for_trace(&engine_kind),
            engine_delta_count: engine_collection.delta_count,
            engine_ms,
            engine_exit_code: None,
            engine_stderr_tail: engine_collection.error_tail,
            extracted_citations: extracted_citations.clone(),
            citations_in_context: citations_are_in_context(
                &extracted_citations,
                &context_article_ids,
            ),
            total_ms: total_start.elapsed().as_millis(),
        })?;
        message
    };
    Ok(ChatResponseDto {
        conversation_id,
        assistant_message_id: assistant_message.id,
        content,
    })
}

#[tauri::command]
fn list_engines(state: State<'_, AppState>) -> CommandResult<Vec<EngineStatusDto>> {
    let selected = lock_or_recover(&state.engine).clone();
    let mut engines = vec![
        EngineStatusDto {
            kind: EngineKind::ChatGpt,
            label: "ChatGPT".to_string(),
            status: status_label(codex_cli_engine().probe()),
        },
        EngineStatusDto {
            kind: EngineKind::Claude,
            label: "Claude".to_string(),
            status: status_label(claude_cli_engine().probe()),
        },
        EngineStatusDto {
            kind: EngineKind::Gemini,
            label: "Gemini".to_string(),
            status: status_label(gemini_cli_engine().probe()),
        },
        EngineStatusDto {
            kind: EngineKind::ApiKey(Provider::OpenAi),
            label: "API 키 직접 입력".to_string(),
            status: "Missing".to_string(),
        },
    ];
    for engine in &mut engines {
        if engine.kind == selected {
            engine.status = "Ready".to_string();
        }
    }
    Ok(engines)
}

#[tauri::command]
fn set_engine(state: State<'_, AppState>, kind: EngineKind) -> CommandResult<EngineStatusDto> {
    *lock_or_recover(&state.engine) = kind.clone();
    Ok(EngineStatusDto {
        label: engine_label(&kind),
        kind,
        status: "Ready".to_string(),
    })
}

#[tauri::command]
fn engine_status(state: State<'_, AppState>) -> CommandResult<EngineStatusDto> {
    let kind = lock_or_recover(&state.engine).clone();
    let engine = selected_engine(&kind);
    Ok(EngineStatusDto {
        label: engine_label(&kind),
        kind,
        status: status_label(engine.probe()),
    })
}

#[tauri::command]
fn check_update(state: State<'_, AppState>) -> CommandResult<UpdateStatusDto> {
    match with_index(&state, |index| index.status().into()) {
        Ok(status) => Ok(status),
        Err(AppError::DataMissing(_)) => Ok(UpdateStatusDto::missing()),
        Err(err) => Err(err),
    }
}

#[tauri::command]
fn apply_update(app: AppHandle, state: State<'_, AppState>) -> CommandResult<UpdateStatusDto> {
    let pack_dir = state.pack_dir.clone();
    let report = pack_updater::update_from_latest_release(&pack_dir, &|event| {
        app.emit("update://progress", update_progress_dto(&event))
            .ok();
    })
    .map_err(|err| {
        let message = err.to_string();
        let db = lock_or_recover(&state.db);
        record_technical_error_if_allowed(&db, &state.data_dir, "update", &message).ok();
        AppError::Update(message)
    })?;
    let status = install_index_from_pack_dir(&state, &report.target_dir)?;
    app.emit("update://done", &status).ok();
    app.emit("rules://status", &status).ok();
    Ok(status)
}

#[tauri::command]
fn open_rulebook(
    app: AppHandle,
    state: State<'_, AppState>,
    page: u32,
) -> CommandResult<RulebookState> {
    let mut rulebook = lock_or_recover(&state.rulebook);
    update_rulebook_state(&mut rulebook, page);
    app.emit("rulebook://open", RulebookOpenEvent { page }).ok();
    Ok(rulebook.clone())
}

#[tauri::command]
fn rulebook_state(state: State<'_, AppState>) -> CommandResult<RulebookState> {
    Ok(lock_or_recover(&state.rulebook).clone())
}

#[tauri::command]
fn get_rulebook(state: State<'_, AppState>) -> CommandResult<RulebookDto> {
    let rules_dir = active_rules_dir(&state)?;
    load_rulebook(&rules_dir)
}

#[tauri::command]
fn conversations_list(
    state: State<'_, AppState>,
    include_deleted: Option<bool>,
) -> CommandResult<Vec<ConversationDto>> {
    let db = lock_or_recover(&state.db);
    db.list_conversations(include_deleted.unwrap_or(false))
}

#[tauri::command]
fn conversations_create(
    state: State<'_, AppState>,
    title: Option<String>,
    mode: Mode,
) -> CommandResult<ConversationDto> {
    let engine = engine_label(&lock_or_recover(&state.engine));
    let db = lock_or_recover(&state.db);
    db.create_conversation(title, mode, engine)
}

#[tauri::command]
fn conversations_get(
    state: State<'_, AppState>,
    id: String,
) -> CommandResult<ConversationDetailDto> {
    let db = lock_or_recover(&state.db);
    db.get_conversation_detail(&id)
}

#[tauri::command]
fn conversations_rename(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> CommandResult<ConversationDto> {
    let db = lock_or_recover(&state.db);
    db.rename_conversation(&id, title)
}

#[tauri::command]
fn conversations_delete_to_trash(
    state: State<'_, AppState>,
    id: String,
) -> CommandResult<ConversationDto> {
    let db = lock_or_recover(&state.db);
    db.delete_to_trash(&id)
}

#[tauri::command]
fn conversations_export_md(state: State<'_, AppState>, id: String) -> CommandResult<String> {
    let db = lock_or_recover(&state.db);
    db.export_md(&id)
}

#[tauri::command]
fn settings_get(state: State<'_, AppState>, key: String) -> CommandResult<Option<String>> {
    let db = lock_or_recover(&state.db);
    db.setting_get(&key)
}

#[tauri::command]
fn settings_set(
    state: State<'_, AppState>,
    key: String,
    value: String,
) -> CommandResult<SettingDto> {
    let db = lock_or_recover(&state.db);
    db.setting_set(key, value)
}

#[tauri::command]
fn settings_list(state: State<'_, AppState>) -> CommandResult<Vec<SettingDto>> {
    let db = lock_or_recover(&state.db);
    db.setting_list()
}

#[tauri::command]
fn question_telemetry_get(
    state: State<'_, AppState>,
) -> CommandResult<QuestionTelemetrySettingsDto> {
    let db = lock_or_recover(&state.db);
    question_telemetry_settings(&db)
}

#[tauri::command]
fn question_telemetry_set(
    state: State<'_, AppState>,
    consent: bool,
    shared_dir: Option<String>,
) -> CommandResult<QuestionTelemetrySettingsDto> {
    let db = lock_or_recover(&state.db);
    set_question_telemetry_settings(&db, consent, shared_dir)
}

#[tauri::command]
fn traces_recent(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> CommandResult<Vec<AnswerTraceDto>> {
    let db = lock_or_recover(&state.db);
    db.recent_traces(limit.unwrap_or(20).min(100))
}

#[tauri::command]
fn traces_get(
    state: State<'_, AppState>,
    message_id: String,
) -> CommandResult<Option<AnswerTraceDto>> {
    let db = lock_or_recover(&state.db);
    db.get_trace(&message_id)
}

#[tauri::command]
fn diagnostics_export(
    state: State<'_, AppState>,
    message_id: String,
    reason: String,
    details: Option<String>,
    labor_share_confirmed: Option<bool>,
) -> CommandResult<DiagnosticExportDto> {
    let db = lock_or_recover(&state.db);
    let report =
        diagnostic_report_for_message(&db, &message_id, &reason, details.as_deref().unwrap_or(""))?;
    let settings = question_telemetry_settings(&db)?;
    let shared_dir = if settings.consent == Some(true)
        && settings
            .shared_dir
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        && (report.mode != "Labor" || labor_share_confirmed == Some(true))
    {
        settings.shared_dir.as_deref().map(Path::new)
    } else {
        None
    };
    export_diagnostic_report_file(&state.data_dir, shared_dir, &report)
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Database::open(data_dir.join("conversations.sqlite3"))?;
            let pack_dir = app_pack_dir(&data_dir);
            let rules_dir = initial_rules_dir(&pack_dir);
            let startup_status = initial_update_status(&pack_dir, rules_dir.as_deref());
            app.manage(AppState {
                db: Mutex::new(db),
                engine: Mutex::new(EngineKind::ChatGpt),
                rules_index: Mutex::new(None),
                data_dir: data_dir.clone(),
                rules_dir: Mutex::new(rules_dir),
                pack_dir,
                rulebook: Mutex::new(RulebookState::default()),
            });
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                handle.emit("rules://status", startup_status).ok();
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search,
            get_article,
            send_chat,
            list_engines,
            set_engine,
            engine_status,
            check_update,
            apply_update,
            open_rulebook,
            rulebook_state,
            get_rulebook,
            conversations_list,
            conversations_create,
            conversations_get,
            conversations_rename,
            conversations_delete_to_trash,
            conversations_export_md,
            settings_get,
            settings_set,
            settings_list,
            question_telemetry_get,
            question_telemetry_set,
            traces_recent,
            traces_get,
            diagnostics_export
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    struct MockEngine;

    impl ChatEngine for MockEngine {
        fn kind(&self) -> EngineKind {
            EngineKind::ChatGpt
        }

        fn probe(&self) -> EngineStatus {
            EngineStatus::Ready
        }

        fn send(&self, _req: ChatRequest) -> rules_engines::BoxStream<ChatDelta> {
            Box::pin(stream::iter([
                ChatDelta::content("여비규정#제12조"),
                ChatDelta::content("에 따릅니다."),
                ChatDelta::done(),
            ]))
        }
    }

    #[test]
    fn labor_mode_appends_disclaimer_in_rust_layer() {
        let answer = append_labor_disclaimer(&Mode::Labor, "복직 절차를 확인하세요.");
        assert!(answer.ends_with(LABOR_DISCLAIMER));
        assert_eq!(answer.matches(LABOR_DISCLAIMER).count(), 1);
    }

    #[test]
    fn non_labor_modes_do_not_append_labor_disclaimer() {
        let answer = append_labor_disclaimer(&Mode::Interpret, "여비규정을 확인하세요.");
        assert!(!answer.contains(LABOR_DISCLAIMER));
    }

    #[test]
    fn conversation_delete_is_soft_delete() {
        let db = Database::in_memory().expect("db");
        let conversation = db
            .create_conversation(
                Some("테스트".to_string()),
                Mode::Interpret,
                "ChatGPT".to_string(),
            )
            .expect("conversation");
        db.add_message(&conversation.id, "user", "질문")
            .expect("message");

        let trashed = db.delete_to_trash(&conversation.id).expect("trash");
        assert!(trashed.deleted_at.is_some());
        assert!(db.list_conversations(false).expect("active").is_empty());
        assert_eq!(db.list_conversations(true).expect("all").len(), 1);
        assert!(db
            .export_md(&conversation.id)
            .expect("export")
            .contains("질문"));
    }

    // --- soft-delete 우회 방지 ---

    fn seeded_conversation(db: &Database) -> ConversationDto {
        db.create_conversation(
            Some("테스트".to_string()),
            Mode::Interpret,
            "ChatGPT".to_string(),
        )
        .expect("conversation")
    }

    fn test_state_with_rules_dir(data_dir: &Path, rules_dir: Option<PathBuf>) -> AppState {
        AppState {
            db: Mutex::new(Database::in_memory().expect("db")),
            engine: Mutex::new(EngineKind::ChatGpt),
            rules_index: Mutex::new(None),
            data_dir: data_dir.to_path_buf(),
            rules_dir: Mutex::new(rules_dir),
            pack_dir: app_pack_dir(data_dir),
            rulebook: Mutex::new(RulebookState::default()),
        }
    }

    fn write_pack_fixture(root: &Path) {
        let articles_dir = root.join("articles");
        let graph_dir = root.join("graph");
        fs::create_dir_all(&articles_dir).expect("articles dir");
        fs::create_dir_all(&graph_dir).expect("graph dir");
        let article = "---\ninstitution: cni\nrule: 출장지급규칙\narticle: 제12조\ntitle: 항공운임\neffective: 2026-02-27\namended: 2026-02-27\nstatus: active\nsupersedes: null\nlegal_basis: []\nrefs: []\n---\n① 항공운임과 출장 교통비를 지급한다.\n";
        fs::write(articles_dir.join("제12조.md"), article).expect("article");
        fs::write(graph_dir.join("nodes.jsonl"), "").expect("nodes");
        fs::write(graph_dir.join("edges.jsonl"), "").expect("edges");
        fs::write(
            root.join("manifest.json"),
            r#"{"schema_version":1,"institution":"cni","effective_date":"2026-02-27","source_commit":"fixture123","created_at":"2026-07-02T00:00:00Z","files":{"articles/제12조.md":"73e95310650ceacd0f1d38b1afcc5ad06abbf5762a3f2cb023bef1adabcf8afa","graph/nodes.jsonl":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","graph/edges.jsonl":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}}"#,
        )
        .expect("manifest");
    }

    #[test]
    fn missing_pack_state_returns_explicit_no_data_error() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let state = test_state_with_rules_dir(data_dir.path(), None);

        let err = with_index(&state, |index| index.search("출장", 5, None))
            .expect_err("missing data must not silently return an empty result");

        assert!(matches!(err, AppError::DataMissing(_)));
        assert_eq!(err.to_string(), RULES_DATA_MISSING_NOTICE);
    }

    #[test]
    fn fixture_pack_bootstrap_swaps_index_without_restart() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let pack_dir = tempfile::tempdir().expect("pack dir");
        write_pack_fixture(pack_dir.path());
        let state = test_state_with_rules_dir(data_dir.path(), None);

        let status = install_index_from_pack_dir(&state, pack_dir.path()).expect("install pack");
        let hits =
            with_index(&state, |index| index.search("출장 교통비", 5, None)).expect("search");

        assert!(status.installed);
        assert_eq!(status.source_commit, "fixture123");
        assert_eq!(
            active_rules_dir(&state).expect("rules dir"),
            pack_dir.path().join("articles")
        );
        assert!(!hits.is_empty());
        assert_eq!(hits[0].article_id, "출장지급규칙#제12조");
    }

    #[test]
    fn deleting_an_already_trashed_conversation_is_rejected_not_silently_reset() {
        let db = Database::in_memory().expect("db");
        let conversation = seeded_conversation(&db);
        let first = db.delete_to_trash(&conversation.id).expect("first trash");

        let err = db
            .delete_to_trash(&conversation.id)
            .expect_err("double delete must fail");
        assert!(matches!(err, AppError::NotFound(_)));

        // The original deleted_at timestamp must be untouched by the rejected
        // second call (no silent "undelete-then-redelete" side effect).
        let still_trashed = db
            .get_conversation(&conversation.id)
            .expect("query")
            .expect("conversation still exists (soft delete, not hard delete)");
        assert_eq!(still_trashed.deleted_at, first.deleted_at);
    }

    #[test]
    fn renaming_a_trashed_conversation_is_rejected() {
        let db = Database::in_memory().expect("db");
        let conversation = seeded_conversation(&db);
        db.delete_to_trash(&conversation.id).expect("trash");

        let err = db
            .rename_conversation(&conversation.id, "우회 시도".to_string())
            .expect_err("rename on trashed conversation must fail");
        assert!(matches!(err, AppError::NotFound(_)));

        let unchanged = db
            .get_conversation(&conversation.id)
            .expect("query")
            .expect("still present");
        assert_eq!(unchanged.title, "테스트");
    }

    #[test]
    fn adding_a_message_to_a_trashed_conversation_is_rejected() {
        let db = Database::in_memory().expect("db");
        let conversation = seeded_conversation(&db);
        db.delete_to_trash(&conversation.id).expect("trash");

        let err = db
            .add_message(&conversation.id, "user", "휴지통 우회 메시지")
            .expect_err("adding a message to a trashed conversation must fail");
        assert!(matches!(err, AppError::NotFound(_)));

        let detail = db
            .get_conversation_detail(&conversation.id)
            .expect("detail still readable for export/audit");
        assert!(detail.messages.is_empty());
    }

    #[test]
    fn hard_delete_command_does_not_exist_only_soft_delete_is_wired() {
        // §8 계약: "삭제는 soft-delete(휴지통 테이블) — 영구삭제 커맨드 만들지 않음."
        // Static-scan guard: build the forbidden SQL fragment via
        // concatenation (not as a contiguous literal) so this assertion
        // doesn't trivially match its own source text through include_str!,
        // while still catching a real hard-delete statement added later to
        // this file (which would naturally appear as a contiguous literal).
        let forbidden = format!("{}{}", "DELETE FR", "OM conversations");
        let source = include_str!("lib.rs");
        assert!(
            !source.contains(&forbidden),
            "found a hard-delete SQL statement against conversations; soft-delete-only contract violated"
        );
    }

    #[test]
    fn delete_to_trash_keeps_the_row_count_unchanged() {
        // Behavioral counterpart to the static-scan guard above: prove via
        // observable behavior (not just source text) that trashing a
        // conversation is a row UPDATE, not a row DELETE.
        let db = Database::in_memory().expect("db");
        let keep = seeded_conversation(&db);
        let trash = seeded_conversation(&db);
        let before = db.list_conversations(true).expect("all").len();

        db.delete_to_trash(&trash.id).expect("trash");

        let after = db.list_conversations(true).expect("all").len();
        assert_eq!(before, after, "row count must be unchanged by soft delete");
        assert!(db
            .list_conversations(true)
            .expect("all")
            .iter()
            .any(|c| c.id == keep.id && c.deleted_at.is_none()));
    }

    // --- 고지문(LABOR_DISCLAIMER) 누락 경로 ---

    #[test]
    fn compare_mode_does_not_append_labor_disclaimer() {
        let answer = append_labor_disclaimer(&Mode::Compare, "두 규정을 비교한 결과입니다.");
        assert!(!answer.contains(LABOR_DISCLAIMER));
    }

    #[test]
    fn appending_disclaimer_twice_does_not_duplicate_it() {
        let once = append_labor_disclaimer(&Mode::Labor, "1차 답변");
        let twice = append_labor_disclaimer(&Mode::Labor, &once);
        assert_eq!(twice, once);
        assert_eq!(twice.matches(LABOR_DISCLAIMER).count(), 1);
    }

    #[test]
    fn open_rulebook_state_switches_to_rulebook_page() {
        let mut state = RulebookState::default();
        update_rulebook_state(&mut state, 176);

        assert!(state.active);
        assert_eq!(state.page, Some(176));
    }

    #[test]
    fn citations_are_inserted_for_assistant_message() {
        let db = Database::in_memory().expect("db");
        let conversation = seeded_conversation(&db);
        let message = db
            .add_message(&conversation.id, "assistant", "여비규정#제12조에 따릅니다.")
            .expect("assistant");
        db.add_citations(
            &message.id,
            &[CitationRecord {
                article_id: Some("여비규정#제12조".to_string()),
                law_ref: None,
                kind: Some("answer".to_string()),
            }],
        )
        .expect("citations");

        let citations = db.citations_for_message(&message.id).expect("query");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].article_id.as_deref(), Some("여비규정#제12조"));
    }

    #[test]
    fn streaming_engine_wiring_collects_mock_deltas_and_emits_chunks() {
        let request = ChatRequest {
            mode: Mode::Interpret,
            messages: vec![Msg {
                role: "user".to_string(),
                content: "출장 교통비".to_string(),
            }],
            context: vec![ContextBlock {
                id: "여비규정#제12조".to_string(),
                title: "제12조 교통비".to_string(),
                body: "교통비를 지급한다.".to_string(),
                source: "fixture".to_string(),
            }],
        };
        let stream = MockEngine.send(request);
        let mut emitted = Vec::new();
        let collection = tauri::async_runtime::block_on(collect_engine_stream(
            stream,
            &Mode::Interpret,
            |delta| emitted.push(delta),
        ));

        assert_eq!(collection.content, "여비규정#제12조에 따릅니다.");
        assert_eq!(collection.delta_count, 2);
        assert_eq!(emitted.len(), 3);
        assert!(emitted.last().expect("done").done);
    }

    #[test]
    fn citations_are_extracted_from_streamed_content() {
        let context = vec![ContextBlock {
            id: "여비규정#제12조".to_string(),
            title: "제12조 교통비".to_string(),
            body: "교통비를 지급한다.".to_string(),
            source: "fixture".to_string(),
        }];

        let citations = citations_from_content("[여비규정#제12조] 기준입니다.", &context);

        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].article_id.as_deref(), Some("여비규정#제12조"));
    }

    #[test]
    fn question_log_opt_out_does_not_write_local_file() {
        let db = Database::in_memory().expect("db");
        let data_dir = tempfile::tempdir().expect("data dir");
        set_question_telemetry_settings(&db, false, None).expect("settings");

        record_question_if_allowed(&db, data_dir.path(), &Mode::Interpret, "출장 교통비는?")
            .expect("record");

        assert!(!local_question_log_path(data_dir.path()).exists());
    }

    #[test]
    fn question_log_never_records_labor_mode_even_when_opted_in() {
        let db = Database::in_memory().expect("db");
        let data_dir = tempfile::tempdir().expect("data dir");
        set_question_telemetry_settings(&db, true, None).expect("settings");

        record_question_if_allowed(&db, data_dir.path(), &Mode::Labor, "복직 상담입니다")
            .expect("record");

        assert!(!local_question_log_path(data_dir.path()).exists());
    }

    #[test]
    fn question_log_jsonl_schema_contains_contract_fields_only() {
        let db = Database::in_memory().expect("db");
        let data_dir = tempfile::tempdir().expect("data dir");
        let settings = set_question_telemetry_settings(&db, true, None).expect("settings");

        record_question_if_allowed(&db, data_dir.path(), &Mode::Compare, "육아휴직 비교")
            .expect("record");

        let line = std::fs::read_to_string(local_question_log_path(data_dir.path()))
            .expect("jsonl")
            .lines()
            .next()
            .expect("line")
            .to_string();
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        let object = value.as_object().expect("object");
        let keys = object.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec!["app_version", "install_id", "mode", "question", "ts"]
        );
        assert_eq!(value["question"], "육아휴직 비교");
        assert_eq!(value["mode"], "Compare");
        assert_eq!(value["app_version"], app_version());
        assert_eq!(value["install_id"], settings.install_id);
    }

    #[test]
    fn question_log_syncs_to_shared_qlog_file_via_temp_rename() {
        let db = Database::in_memory().expect("db");
        let data_dir = tempfile::tempdir().expect("data dir");
        let shared_dir = tempfile::tempdir().expect("shared dir");
        let settings = set_question_telemetry_settings(
            &db,
            true,
            Some(shared_dir.path().display().to_string()),
        )
        .expect("settings");

        record_question_if_allowed(&db, data_dir.path(), &Mode::Interpret, "출장 교통비는?")
            .expect("record");

        let synced = shared_dir
            .path()
            .join(format!("qlog-{}.jsonl", settings.install_id));
        assert!(synced.exists());
        assert_eq!(
            std::fs::read_to_string(local_question_log_path(data_dir.path())).expect("local"),
            std::fs::read_to_string(synced).expect("synced")
        );
        let temp_files = std::fs::read_dir(shared_dir.path())
            .expect("read shared")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temp_files, 0);
    }

    fn sample_trace(message_id: String, conversation_id: String) -> NewAnswerTrace {
        NewAnswerTrace {
            message_id,
            conversation_id,
            search_query: "출장 교통비".to_string(),
            search_results: vec![SearchTraceHit {
                article_id: "여비규정#제12조".to_string(),
                score: 3.5,
            }],
            direct_routing: false,
            search_ms: 7,
            context_article_ids: vec!["여비규정#제12조".to_string()],
            prompt_bytes: 128,
            engine_kind: "\"ChatGpt\"".to_string(),
            engine_delta_count: 2,
            engine_ms: 31,
            engine_exit_code: None,
            engine_stderr_tail: String::new(),
            extracted_citations: vec!["여비규정#제12조".to_string()],
            citations_in_context: true,
            total_ms: 42,
        }
    }

    #[test]
    fn trace_record_round_trips_with_message_id_and_stage_metrics() {
        let db = Database::in_memory().expect("db");
        let conversation = seeded_conversation(&db);
        let message = db
            .add_message(
                &conversation.id,
                "assistant",
                "[여비규정#제12조] 기준입니다.",
            )
            .expect("message");

        let trace = db
            .add_trace(&sample_trace(message.id.clone(), conversation.id.clone()))
            .expect("trace");

        assert_eq!(trace.message_id, message.id);
        assert_eq!(trace.search_results[0].article_id, "여비규정#제12조");
        assert_eq!(trace.context_article_ids, vec!["여비규정#제12조"]);
        assert_eq!(trace.engine_delta_count, 2);
        assert_eq!(trace.total_ms, 42);
        assert_eq!(db.recent_traces(20).expect("recent").len(), 1);
    }

    #[test]
    fn csr_false_when_extracted_citation_is_not_in_injected_context() {
        let extracted = vec!["인사관리규정#제35조".to_string()];
        let context = vec!["여비규정#제12조".to_string()];

        assert!(!citations_are_in_context(&extracted, &context));
    }

    #[test]
    fn diagnostic_export_writes_local_and_shared_json_for_non_labor_report() {
        let db = Database::in_memory().expect("db");
        let data_dir = tempfile::tempdir().expect("data dir");
        let shared_dir = tempfile::tempdir().expect("shared dir");
        set_question_telemetry_settings(&db, true, Some(shared_dir.path().display().to_string()))
            .expect("settings");
        let conversation = seeded_conversation(&db);
        db.add_message(&conversation.id, "user", "출장 교통비는?")
            .expect("question");
        let answer = db
            .add_message(
                &conversation.id,
                "assistant",
                "[여비규정#제12조] 기준입니다.",
            )
            .expect("answer");
        db.add_trace(&sample_trace(answer.id.clone(), conversation.id.clone()))
            .expect("trace");
        let report =
            diagnostic_report_for_message(&db, &answer.id, "답변이 틀림", "메모").expect("report");

        let export =
            export_diagnostic_report_file(data_dir.path(), Some(shared_dir.path()), &report)
                .expect("export");

        assert!(Path::new(&export.local_path).exists());
        assert!(export.shared);
        assert!(Path::new(export.shared_path.as_deref().expect("shared")).exists());
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(export.local_path).expect("json"))
                .expect("parse");
        assert_eq!(saved["reason"], "답변이 틀림");
        assert_eq!(saved["question"], "출장 교통비는?");
    }

    #[test]
    fn labor_diagnostic_requires_explicit_share_confirmation() {
        let db = Database::in_memory().expect("db");
        let data_dir = tempfile::tempdir().expect("data dir");
        let shared_dir = tempfile::tempdir().expect("shared dir");
        set_question_telemetry_settings(&db, true, Some(shared_dir.path().display().to_string()))
            .expect("settings");
        let conversation = db
            .create_conversation(Some("노무".to_string()), Mode::Labor, "ChatGPT".to_string())
            .expect("conversation");
        db.add_message(&conversation.id, "user", "복직 상담")
            .expect("question");
        let answer = db
            .add_message(&conversation.id, "assistant", "답변")
            .expect("answer");
        let report = diagnostic_report_for_message(&db, &answer.id, "기타", "").expect("report");

        let denied = export_diagnostic_report_file(data_dir.path(), None, &report).expect("local");
        assert!(!denied.shared);
        assert!(!shared_dir.path().join("diagnostics").exists());

        let confirmed =
            export_diagnostic_report_file(data_dir.path(), Some(shared_dir.path()), &report)
                .expect("shared");
        assert!(confirmed.shared);
        assert!(shared_dir.path().join("diagnostics").exists());
    }
}
