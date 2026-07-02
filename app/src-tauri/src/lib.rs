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
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

pub const LABOR_MODE_NOTICE: &str =
    "이 도구는 내 상황이 무엇인지 최초 판단을 돕는 참고 도구입니다.";
pub const LABOR_DISCLAIMER: &str =
    "본 내용은 법률 자문이 아니며, 구체적인 사안은 노무사 등 전문가와 상담하시기 바랍니다.";

pub struct AppState {
    db: Mutex<Database>,
    engine: Mutex<EngineKind>,
    rules_index: Mutex<Option<TantivyRulesIndex>>,
    data_dir: PathBuf,
    rules_dir: PathBuf,
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
    pub institution: String,
    pub effective_date: String,
    pub source_commit: String,
    pub index_built_at: String,
    pub stale: bool,
}

impl From<PackStatus> for UpdateStatusDto {
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
            CREATE INDEX IF NOT EXISTS idx_conversations_deleted_updated
                ON conversations(deleted_at, updated_at);
            CREATE INDEX IF NOT EXISTS idx_messages_conversation_created
                ON messages(conversation_id, created_at);
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
        *index = Some(load_index_from_rules_dir(&state.rules_dir)?);
    }
    Ok(f(index.as_ref().expect("index initialized")))
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
    let mut citations = Vec::new();
    for block in context {
        let bracket = format!("[{}]", block.id);
        if content.contains(&bracket) || content.contains(&block.id) {
            citations.push(CitationRecord {
                article_id: Some(block.id.clone()),
                law_ref: None,
                kind: Some("answer".to_string()),
            });
        }
    }
    citations
}

async fn collect_engine_stream(
    mut stream: rules_engines::BoxStream<ChatDelta>,
    mode: &Mode,
    mut emit: impl FnMut(ChatDelta),
) -> String {
    let mut content = String::new();
    while let Some(delta) = stream.next().await {
        if !delta.content.is_empty() {
            content.push_str(&delta.content);
        }
        emit(delta);
    }
    append_labor_disclaimer(mode, &content)
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

fn dev_rules_dir() -> PathBuf {
    std::env::var_os("CNI_RULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../04_data/90_index-build/rules"))
}

fn app_pack_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("rules-pack")
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
    with_index(&state, |index| index.get_article(&id))?
        .map(|article| ArticleWithPagesDto {
            source_pages: source_pages_for_article(&state.rules_dir, &article),
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
    {
        let db = lock_or_recover(&state.db);
        db.add_message(&conversation_id, "user", &text)?;
        record_question_if_allowed(&db, &state.data_dir, &mode, &text)?;
    }
    let (engine_kind, hits, context) = {
        let engine_kind = lock_or_recover(&state.engine).clone();
        let (hits, context) = with_index(&state, |index| {
            let hits = index.search(&text, 5, None);
            let context = context_from_hits(index, &hits);
            (hits, context)
        })?;
        (engine_kind, hits, context)
    };
    let request = ChatRequest {
        mode,
        messages: vec![Msg {
            role: "user".to_string(),
            content: text.clone(),
        }],
        context,
    };
    let stream = selected_engine(&engine_kind).send(request);
    let conversation_for_emit = conversation_id.clone();
    let content = collect_engine_stream(stream, &mode, |delta| {
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
    let assistant_message = {
        let db = lock_or_recover(&state.db);
        let message = db.add_message(&conversation_id, "assistant", &content)?;
        let context_ids = hits
            .into_iter()
            .filter_map(|hit| {
                with_index(&state, |index| index.get_article(&hit.article_id)).ok()?
            })
            .map(|article| ContextBlock {
                id: article.id,
                title: article.title,
                body: article.body,
                source: article.effective,
            })
            .collect::<Vec<_>>();
        let citations = citations_from_content(&content, &context_ids);
        db.add_citations(&message.id, &citations)?;
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
    with_index(&state, |index| index.status().into())
}

#[tauri::command]
fn apply_update(app: AppHandle, state: State<'_, AppState>) -> CommandResult<UpdateStatusDto> {
    let pack_dir = state.pack_dir.clone();
    let report = pack_updater::update_from_latest_release(&pack_dir, &|event| {
        app.emit("update://progress", update_progress_dto(&event))
            .ok();
    })
    .map_err(|err| AppError::Update(err.to_string()))?;
    let index = TantivyRulesIndex::from_pack_dir(&report.target_dir)
        .map_err(|err| AppError::RulesIndex(err.to_string()))?;
    let status = UpdateStatusDto::from(index.status());
    *lock_or_recover(&state.rules_index) = Some(index);
    app.emit("update://done", &status).ok();
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
    load_rulebook(&state.rules_dir)
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

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Database::open(data_dir.join("conversations.sqlite3"))?;
            let pack_dir = app_pack_dir(&data_dir);
            let rules_dir = if pack_dir.join("articles").is_dir() {
                pack_dir.join("articles")
            } else {
                dev_rules_dir()
            };
            app.manage(AppState {
                db: Mutex::new(db),
                engine: Mutex::new(EngineKind::ChatGpt),
                rules_index: Mutex::new(None),
                data_dir: data_dir.clone(),
                rules_dir,
                pack_dir,
                rulebook: Mutex::new(RulebookState::default()),
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
            question_telemetry_set
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
                content: "출장 일비".to_string(),
            }],
            context: vec![ContextBlock {
                id: "여비규정#제12조".to_string(),
                title: "제12조 일비".to_string(),
                body: "일비를 지급한다.".to_string(),
                source: "fixture".to_string(),
            }],
        };
        let stream = MockEngine.send(request);
        let mut emitted = Vec::new();
        let content = tauri::async_runtime::block_on(collect_engine_stream(
            stream,
            &Mode::Interpret,
            |delta| emitted.push(delta),
        ));

        assert_eq!(content, "여비규정#제12조에 따릅니다.");
        assert_eq!(emitted.len(), 3);
        assert!(emitted.last().expect("done").done);
    }

    #[test]
    fn citations_are_extracted_from_streamed_content() {
        let context = vec![ContextBlock {
            id: "여비규정#제12조".to_string(),
            title: "제12조 일비".to_string(),
            body: "일비를 지급한다.".to_string(),
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

        record_question_if_allowed(&db, data_dir.path(), &Mode::Interpret, "출장 일비는?")
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
        assert_eq!(value["app_version"], "0.1.0-beta");
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

        record_question_if_allowed(&db, data_dir.path(), &Mode::Interpret, "출장 일비는?")
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
}
