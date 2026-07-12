use std::fmt;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::Duration;

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use keyring::Entry;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::time::timeout;

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineKind {
    ChatGpt,
    Claude,
    Gemini,
    ApiKey(Provider),
}

pub trait ChatEngine {
    fn kind(&self) -> EngineKind;
    fn probe(&self) -> EngineStatus;
    fn send(&self, req: ChatRequest) -> BoxStream<ChatDelta>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    OpenAi,
    Anthropic,
    Google,
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineStatus {
    Installed,
    NeedsLogin,
    Ready,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Interpret,
    Labor,
    Compare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    pub id: String,
    pub title: String,
    pub body: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub mode: Mode,
    pub messages: Vec<Msg>,
    pub context: Vec<ContextBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDelta {
    pub content: String,
    pub done: bool,
}

impl ChatDelta {
    pub fn content(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            done: false,
        }
    }

    pub fn done() -> Self {
        Self {
            content: String::new(),
            done: true,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            content: error.into(),
            done: false,
        }
    }
}

#[derive(Debug)]
pub enum RulesEngineError {
    MissingApiKey,
    InvalidApiKey,
    Http(reqwest::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl fmt::Display for RulesEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingApiKey => write!(f, "API key is missing from the OS credential store"),
            Self::InvalidApiKey => write!(f, "API key cannot be used in an Authorization header"),
            Self::Http(err) => write!(f, "HTTP request failed: {err}"),
            Self::Json(err) => write!(f, "JSON parsing failed: {err}"),
            Self::Io(err) => write!(f, "process I/O failed: {err}"),
        }
    }
}

impl std::error::Error for RulesEngineError {}

pub struct PromptBuilder;

impl PromptBuilder {
    pub fn build(req: &ChatRequest) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are a Korean institutional rules interpretation assistant.\n");
        prompt.push_str(
            "Use only the supplied regulation and law context when answering factual questions.\n",
        );
        prompt.push_str("Cite every legal or regulation claim with its source id in square brackets, e.g. [여비규정#제12조].\n");
        prompt
            .push_str("If the context is insufficient, say what is missing instead of guessing.\n");

        match req.mode {
            Mode::Interpret => {
                prompt
                    .push_str("Mode: interpret the selected internal rule and its legal basis.\n");
            }
            Mode::Labor => {
                prompt.push_str("Mode: labor intake. First classify the user's situation, then identify required facts, applicable rule/law context, and next questions.\n");
                prompt.push_str("Do not generate or paraphrase any legal-advice disclaimer; the app layer owns fixed notices.\n");
            }
            Mode::Compare => {
                prompt.push_str("Mode: compare provisions. Explain similarities, conflicts, exceptions, and effective-date implications.\n");
            }
        }

        if !req.context.is_empty() {
            prompt.push_str("\n<context>\n");
            for block in &req.context {
                prompt.push('[');
                prompt.push_str(&block.id);
                prompt.push_str("] ");
                prompt.push_str(&block.title);
                prompt.push_str(" (source: ");
                prompt.push_str(&block.source);
                prompt.push_str(")\n");
                prompt.push_str(&block.body);
                prompt.push_str("\n\n");
            }
            prompt.push_str("</context>\n");
        }

        prompt.push_str("\n<conversation>\n");
        for msg in &req.messages {
            prompt.push_str(&msg.role);
            prompt.push_str(": ");
            prompt.push_str(&msg.content);
            prompt.push('\n');
        }
        prompt.push_str("</conversation>\n");
        prompt
    }
}

#[derive(Debug, Clone)]
struct CliCommandSpec {
    executable: String,
    args: Vec<String>,
    login_args: Vec<String>,
    prompt_input: CliPromptInput,
    envs: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
enum CliPromptInput {
    Stdin,
    Arg,
}

impl CliCommandSpec {
    fn codex() -> Self {
        Self {
            executable: "codex".to_string(),
            args: vec![
                "exec".to_string(),
                "--skip-git-repo-check".to_string(),
                "-".to_string(),
            ],
            login_args: vec!["auth".to_string(), "status".to_string()],
            prompt_input: CliPromptInput::Stdin,
            envs: Vec::new(),
        }
    }

    fn claude() -> Self {
        Self {
            executable: "claude".to_string(),
            args: vec!["--print".to_string()],
            login_args: vec!["--version".to_string()],
            prompt_input: CliPromptInput::Stdin,
            envs: Vec::new(),
        }
    }

    fn gemini() -> Self {
        Self {
            executable: "gemini".to_string(),
            args: vec!["-p".to_string()],
            login_args: vec!["--version".to_string()],
            prompt_input: CliPromptInput::Arg,
            // headless 실행은 신뢰 디렉터리 검사에 걸림 — 명시 신뢰 필요
            envs: vec![("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string())],
        }
    }

    #[cfg(test)]
    fn test(executable: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            executable: executable.into(),
            args,
            login_args: vec!["--version".to_string()],
            prompt_input: CliPromptInput::Stdin,
            envs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliSidecarEngine {
    kind: EngineKind,
    command: CliCommandSpec,
    read_timeout: Duration,
}

impl CliSidecarEngine {
    fn new(kind: EngineKind, command: CliCommandSpec) -> Self {
        Self {
            kind,
            command,
            read_timeout: Duration::from_secs(30),
        }
    }

    fn stream_process(&self, req: ChatRequest) -> BoxStream<ChatDelta> {
        let command = self.command.clone();
        let read_timeout = self.read_timeout;
        Box::pin(stream! {
            let prompt = PromptBuilder::build(&req);
            let mut args = command.args.clone();
            if matches!(command.prompt_input, CliPromptInput::Arg) {
                args.push(prompt.clone());
            }

            let mut child = match TokioCommand::new(&command.executable)
                .args(&args)
                .envs(command.envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(err) => {
                    yield ChatDelta::content(format!("engine process failed to start: {err}"));
                    yield ChatDelta::done();
                    return;
                }
            };

            if matches!(command.prompt_input, CliPromptInput::Stdin) {
                let Some(mut stdin) = child.stdin.take() else {
                    yield ChatDelta::content("engine stdin was unavailable");
                    yield ChatDelta::done();
                    return;
                };

                if let Err(err) = stdin.write_all(prompt.as_bytes()).await {
                    yield ChatDelta::content(format!("engine stdin write failed: {err}"));
                    yield ChatDelta::done();
                    return;
                }
            } else {
                drop(child.stdin.take());
            }

            let Some(stdout) = child.stdout.take() else {
                yield ChatDelta::content("engine stdout was unavailable");
                yield ChatDelta::done();
                return;
            };
            let stderr_task = child.stderr.take().map(|mut stderr| {
                tokio::spawn(async move {
                    let mut captured = String::new();
                    let _ = stderr.read_to_string(&mut captured).await;
                    captured
                })
            });

            let mut lines = BufReader::new(stdout).lines();
            let mut content_delta_count = 0_usize;
            loop {
                match timeout(read_timeout, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        for content in parse_cli_stdout_line(&line) {
                            if !content.is_empty() {
                                content_delta_count += 1;
                                yield ChatDelta::content(content);
                            }
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(err)) => {
                        yield ChatDelta::content(format!("engine stdout read failed: {err}"));
                        break;
                    }
                    Err(_) => {
                        let _ = child.kill().await;
                        yield ChatDelta::content("engine process timed out");
                        yield ChatDelta::done();
                        return;
                    }
                }
            }

            let status = child.wait().await;
            let stderr = match stderr_task {
                Some(task) => task.await.unwrap_or_default(),
                None => String::new(),
            };
            let failed = status.as_ref().map_or(true, |status| !status.success());
            if failed || content_delta_count == 0 {
                yield ChatDelta::error(sidecar_error_message(status, &stderr, content_delta_count == 0));
            }
            yield ChatDelta::done();
        })
    }

    #[cfg(test)]
    fn for_test(executable: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            kind: EngineKind::ChatGpt,
            command: CliCommandSpec::test(executable, args),
            read_timeout: Duration::from_millis(200),
        }
    }
}

impl ChatEngine for CliSidecarEngine {
    fn kind(&self) -> EngineKind {
        self.kind.clone()
    }

    fn probe(&self) -> EngineStatus {
        probe_cli(&self.command)
    }

    fn send(&self, req: ChatRequest) -> BoxStream<ChatDelta> {
        self.stream_process(req)
    }
}

pub type CodexCliEngine = CliSidecarEngine;
pub type ClaudeCliEngine = CliSidecarEngine;
pub type GeminiCliEngine = CliSidecarEngine;

pub fn codex_cli_engine() -> CodexCliEngine {
    CliSidecarEngine::new(EngineKind::ChatGpt, CliCommandSpec::codex())
}

pub fn claude_cli_engine() -> ClaudeCliEngine {
    CliSidecarEngine::new(EngineKind::Claude, CliCommandSpec::claude())
}

pub fn gemini_cli_engine() -> GeminiCliEngine {
    CliSidecarEngine::new(EngineKind::Gemini, CliCommandSpec::gemini())
}

fn probe_cli(spec: &CliCommandSpec) -> EngineStatus {
    if which::which(&spec.executable).is_err() {
        return EngineStatus::Missing;
    }

    let output = Command::new(&spec.executable)
        .args(&spec.login_args)
        .envs(spec.envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let Ok(output) = output else {
        return EngineStatus::Installed;
    };

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();

    if looks_like_cli_auth_error(&combined) {
        EngineStatus::NeedsLogin
    } else if output.status.success() {
        EngineStatus::Ready
    } else {
        EngineStatus::Installed
    }
}

fn looks_like_cli_auth_error(output: &str) -> bool {
    let output = output.to_lowercase();
    output.contains("login")
        || output.contains("not authenticated")
        || output.contains("unauthorized")
        || output.contains("auth required")
        || output.contains("authentication")
        || output.contains("forbidden")
        || output.contains("ineligibletiererror")
        || output.contains("ineligible tier")
}

fn sidecar_error_message(
    status: Result<std::process::ExitStatus, std::io::Error>,
    stderr: &str,
    no_stdout_content: bool,
) -> String {
    let reason = match status {
        Ok(status) if status.success() && no_stdout_content => {
            "engine process finished without output".to_string()
        }
        Ok(status) => format!("engine process exited with {status}"),
        Err(err) => format!("engine process wait failed: {err}"),
    };
    let stderr = last_chars(stderr.trim(), 500);
    if stderr.is_empty() {
        reason
    } else {
        format!("{reason}: {stderr}")
    }
}

fn last_chars(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

pub fn parse_cli_stdout_line(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.get("done").and_then(Value::as_bool) == Some(true) {
            return Vec::new();
        }

        for path in [
            &["delta", "content"][..],
            &["delta"][..],
            &["content"][..],
            &["message", "content"][..],
            &["text"][..],
        ] {
            if let Some(text) = json_path_string(&value, path) {
                return vec![text.to_string()];
            }
        }

        if let Some(array) = value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            return vec![array.to_string()];
        }
        return Vec::new();
    }

    vec![line.to_string()]
}

fn json_path_string<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

#[derive(Debug, Clone)]
pub struct ApiKeyEngine {
    provider: Provider,
    model: String,
    endpoint: String,
    credential_service: String,
    credential_user: String,
    client: reqwest::Client,
}

impl ApiKeyEngine {
    pub fn openai_compatible(
        provider: Provider,
        model: impl Into<String>,
        endpoint: impl Into<String>,
        credential_service: impl Into<String>,
        credential_user: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            endpoint: endpoint.into(),
            credential_service: credential_service.into(),
            credential_user: credential_user.into(),
            client: reqwest::Client::new(),
        }
    }

    pub fn openai(model: impl Into<String>) -> Self {
        Self::openai_compatible(
            Provider::OpenAi,
            model,
            "https://api.openai.com/v1/chat/completions",
            "rules-engines-openai-api-key",
            "default",
        )
    }

    pub fn store_api_key(&self, api_key: &str) -> Result<(), keyring::Error> {
        Entry::new(&self.credential_service, &self.credential_user)?.set_password(api_key)
    }

    fn load_api_key(&self) -> Result<String, RulesEngineError> {
        Entry::new(&self.credential_service, &self.credential_user)
            .and_then(|entry| entry.get_password())
            .map_err(|_| RulesEngineError::MissingApiKey)
    }

    fn send_api_stream(&self, req: ChatRequest) -> BoxStream<ChatDelta> {
        let engine = self.clone();
        Box::pin(stream! {
            match engine.request_stream(req).await {
                Ok(mut stream) => {
                    while let Some(next) = stream.next().await {
                        match next {
                            Ok(bytes) => {
                                for content in parse_sse_bytes(&bytes) {
                                    yield ChatDelta::content(content);
                                }
                            }
                            Err(err) => {
                                yield ChatDelta::content(format!("api stream failed: {err}"));
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    yield ChatDelta::content(err.to_string());
                }
            }
            yield ChatDelta::done();
        })
    }

    async fn request_stream(
        &self,
        req: ChatRequest,
    ) -> Result<impl Stream<Item = Result<bytes::Bytes, reqwest::Error>>, RulesEngineError> {
        let api_key = self.load_api_key()?;
        let bearer = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| RulesEngineError::InvalidApiKey)?;
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, bearer);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let mut messages = vec![json!({
            "role": "system",
            "content": PromptBuilder::build(&ChatRequest {
                mode: req.mode,
                messages: Vec::new(),
                context: req.context.clone(),
            }),
        })];
        messages.extend(req.messages.iter().map(|msg| {
            json!({
                "role": msg.role,
                "content": msg.content,
            })
        }));

        let response = self
            .client
            .post(&self.endpoint)
            .headers(headers)
            .json(&json!({
                "model": self.model,
                "messages": messages,
                "stream": true,
            }))
            .send()
            .await
            .map_err(RulesEngineError::Http)?
            .error_for_status()
            .map_err(RulesEngineError::Http)?;

        Ok(response.bytes_stream())
    }
}

impl ChatEngine for ApiKeyEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::ApiKey(self.provider.clone())
    }

    fn probe(&self) -> EngineStatus {
        match self.load_api_key() {
            Ok(key) if !key.trim().is_empty() => EngineStatus::Ready,
            _ => EngineStatus::NeedsLogin,
        }
    }

    fn send(&self, req: ChatRequest) -> BoxStream<ChatDelta> {
        self.send_api_stream(req)
    }
}

pub fn parse_sse_bytes(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if let Some(content) = value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
            {
                out.push(content.to_string());
            }
        }
    }
    out
}

pub fn wait_for_probe(engine: &dyn ChatEngine) -> EngineStatus {
    engine.probe()
}

pub async fn collect_stream(mut stream: BoxStream<ChatDelta>) -> Vec<ChatDelta> {
    let mut out = Vec::new();
    while let Some(delta) = stream.next().await {
        out.push(delta);
    }
    out
}

pub async fn collect_stream_with_timeout(
    stream: BoxStream<ChatDelta>,
    duration: Duration,
) -> Vec<ChatDelta> {
    timeout(duration, collect_stream(stream))
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: Mode) -> ChatRequest {
        ChatRequest {
            mode,
            messages: vec![Msg {
                role: "user".to_string(),
                content: "출장 교통비 기준은?".to_string(),
            }],
            context: vec![ContextBlock {
                id: "출장규정#제12조".to_string(),
                title: "교통비".to_string(),
                body: "① 국내 출장자에게는 교통비를 지급한다.".to_string(),
                source: "cni-rules@abc123".to_string(),
            }],
        }
    }

    #[test]
    fn prompt_builder_injects_context_and_citation_rule() {
        let prompt = PromptBuilder::build(&request(Mode::Interpret));

        assert!(prompt.contains("[출장규정#제12조] 교통비"));
        assert!(prompt.contains("Cite every legal or regulation claim"));
        assert!(prompt.contains("출장 교통비 기준은?"));
    }

    #[test]
    fn prompt_builder_labor_mode_has_intake_without_disclaimer() {
        let prompt = PromptBuilder::build(&request(Mode::Labor));

        assert!(prompt.contains("Mode: labor intake"));
        assert!(prompt.contains("required facts"));
        assert!(prompt.contains("Do not generate or paraphrase any legal-advice disclaimer"));
        assert!(!prompt.contains("본 내용은 법률 자문이 아니며"));
    }

    #[test]
    fn parses_cli_json_and_plain_stdout() {
        assert_eq!(
            parse_cli_stdout_line(r#"{"delta":{"content":"안녕"}}"#),
            vec!["안녕".to_string()]
        );
        assert_eq!(
            parse_cli_stdout_line(r#"{"choices":[{"delta":{"content":"하세요"}}]}"#),
            vec!["하세요".to_string()]
        );
        assert_eq!(
            parse_cli_stdout_line("plain output"),
            vec!["plain output".to_string()]
        );
    }

    #[test]
    fn parses_openai_sse_chunks() {
        let chunk = br#"data: {"choices":[{"delta":{"content":"A"}}]}
data: {"choices":[{"delta":{"content":"B"}}]}
data: [DONE]
"#;
        assert_eq!(
            parse_sse_bytes(chunk),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    #[test]
    fn probe_reports_missing_for_nonexistent_executable() {
        let engine = CliSidecarEngine::for_test(
            "cni-rule-definitely-not-a-real-binary-xyz",
            vec!["--print".to_string()],
        );
        assert_eq!(engine.probe(), EngineStatus::Missing);
    }

    #[test]
    fn probe_auth_error_detection_includes_cli_eligibility_failures() {
        assert!(looks_like_cli_auth_error(
            "IneligibleTierError: This account cannot use Gemini CLI"
        ));
        assert!(looks_like_cli_auth_error("not authenticated"));
        assert!(!looks_like_cli_auth_error("gemini version 0.40.1"));
    }

    #[tokio::test]
    async fn stream_terminates_with_done_when_process_exits_immediately_without_output() {
        // Simulates a sidecar CLI that crashes/exits before writing anything —
        // the stream must surface an error delta and terminate with `done`
        // instead of hanging or silently dropping the conversation turn.
        let engine = CliSidecarEngine::for_test("sh", vec!["-c".to_string(), "exit 1".to_string()]);

        let deltas = collect_stream_with_timeout(
            engine.send(request(Mode::Interpret)),
            Duration::from_secs(2),
        )
        .await;

        assert!(!deltas.is_empty(), "must emit at least the done delta");
        assert!(deltas
            .iter()
            .any(|delta| delta.content.contains("engine process exited")));
        assert!(deltas.last().is_some_and(|delta| delta.done));
    }

    #[tokio::test]
    async fn stream_reports_stderr_when_process_exits_without_stdout() {
        let engine = CliSidecarEngine::for_test(
            "sh",
            vec![
                "-c".to_string(),
                "cat >/dev/null; printf '%s' 'IneligibleTierError: Gemini auth failed' >&2; exit 1"
                    .to_string(),
            ],
        );

        let deltas = collect_stream_with_timeout(
            engine.send(request(Mode::Interpret)),
            Duration::from_secs(2),
        )
        .await;

        let error_delta = deltas
            .iter()
            .find(|delta| delta.content.contains("IneligibleTierError"))
            .expect("stderr must be surfaced as an error delta");
        assert!(!error_delta.done);
        assert!(error_delta.content.contains("engine process exited"));
        assert!(deltas.last().is_some_and(|delta| delta.done));
    }

    #[tokio::test]
    async fn stream_flushes_trailing_unterminated_line_when_process_dies_at_eof() {
        // Simulates a sidecar CLI that is interrupted mid-answer: it writes a
        // partial chunk with NO trailing newline and then the process exits
        // (pipe closes / EOF), without ever emitting a final `{"done":true}`
        // marker. The reader must still surface the buffered partial content
        // at EOF instead of silently dropping the user's partial answer.
        let engine = CliSidecarEngine::for_test(
            "sh",
            vec![
                "-c".to_string(),
                "cat >/dev/null; printf '%s' 'partial-no-newline'".to_string(),
            ],
        );

        let deltas = collect_stream_with_timeout(
            engine.send(request(Mode::Interpret)),
            Duration::from_secs(2),
        )
        .await;

        let content = deltas
            .iter()
            .filter(|delta| !delta.done)
            .map(|delta| delta.content.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(content, "partial-no-newline");
        assert!(deltas.last().is_some_and(|delta| delta.done));
    }

    #[tokio::test]
    async fn stream_reports_timeout_when_process_hangs_past_the_read_timeout() {
        // A genuinely stuck/interrupted process: it writes a partial chunk
        // with no trailing newline and then hangs (neither closes stdout nor
        // exits). tokio's line-buffered reader cannot know the line is
        // "finished" without either a newline or EOF, so — unlike the
        // EOF-flush case above — no content is delivered before the caller's
        // timeout elapses. This documents the current interruption behavior:
        // a wedged sidecar process yields nothing until it is killed/reaped,
        // rather than surfacing a partial answer immediately.
        let engine = CliSidecarEngine::for_test(
            "sh",
            vec![
                "-c".to_string(),
                "cat >/dev/null; printf '%s' 'stuck-no-newline'; sleep 5".to_string(),
            ],
        );

        let deltas = collect_stream_with_timeout(
            engine.send(request(Mode::Interpret)),
            Duration::from_secs(2),
        )
        .await;

        assert!(deltas
            .iter()
            .any(|delta| delta.content.contains("engine process timed out")));
        assert!(deltas.last().is_some_and(|delta| delta.done));
    }

    #[test]
    fn engine_kind_and_status_are_stable_for_sidecar_engines() {
        assert_eq!(codex_cli_engine().kind(), EngineKind::ChatGpt);
        assert_eq!(claude_cli_engine().kind(), EngineKind::Claude);
        assert_eq!(gemini_cli_engine().kind(), EngineKind::Gemini);
    }

    #[tokio::test]
    async fn streams_mock_process_stdout() {
        let engine = CliSidecarEngine::for_test(
            "sh",
            vec![
                "-c".to_string(),
                "cat >/dev/null; printf '%s\n' '{\"delta\":{\"content\":\"첫\"}}' '{\"choices\":[{\"delta\":{\"content\":\"째\"}}]}' 'plain'".to_string(),
            ],
        );

        let deltas = collect_stream_with_timeout(
            engine.send(request(Mode::Interpret)),
            Duration::from_secs(2),
        )
        .await;
        let content = deltas
            .iter()
            .filter(|delta| !delta.done)
            .map(|delta| delta.content.as_str())
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(content, "첫째plain");
        assert!(deltas.last().is_some_and(|delta| delta.done));
    }
}
