use public_rules_mcp::{
    AuthToken, CompareRulesResult, FreshnessMeta, PackConfig, SearchRulesResult, ServerConfig,
    ServerTransport, TransportArgs, VectorConfig, COMPARE_RULES_TOOL, GET_ANNEX_TOOL,
    GET_ARTICLE_TOOL, GET_LEGAL_BASIS_TOOL, GET_SOURCE_PAGE_TOOL, LABOR_COMPARE_PROMPT,
    LIST_RULES_TOOL, SEARCH_RULES_TOOL, STATUS_TOOL,
};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo, ContentBlock, GetPromptRequestParams},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};
use std::{
    collections::BTreeSet,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn streamable_http_round_trips_tools_and_search_results() -> anyhow::Result<()> {
    let fixture_root = make_fixture_pack()?;
    let config = fixture_config(fixture_root.clone());
    let query_log_path = fixture_root.join("query-log.jsonl");

    let addr = unused_loopback_addr().await?;
    let server_handle = tokio::spawn(public_rules_mcp::run_server_with_transport_args(
        config,
        TransportArgs {
            transport: ServerTransport::Http,
            bind_addr: addr,
            allowed_hosts: Vec::new(),
            query_log_path: Some(query_log_path.clone()),
            auth_token: None,
        },
    ));
    let url = format!("http://{addr}/mcp");

    let client = connect_with_retry(&url).await?;
    let peer_info = client
        .peer_info()
        .ok_or_else(|| anyhow::anyhow!("server handshake info missing"))?;
    assert!(peer_info.capabilities.tools.is_some());
    assert!(peer_info.capabilities.prompts.is_some());

    let tool_names = client
        .list_all_tools()
        .await?
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        tool_names,
        BTreeSet::from([
            COMPARE_RULES_TOOL.to_string(),
            SEARCH_RULES_TOOL.to_string(),
            GET_ARTICLE_TOOL.to_string(),
            LIST_RULES_TOOL.to_string(),
            GET_LEGAL_BASIS_TOOL.to_string(),
            STATUS_TOOL.to_string(),
            GET_ANNEX_TOOL.to_string(),
            GET_SOURCE_PAGE_TOOL.to_string(),
        ])
    );

    let prompts = client.list_all_prompts().await?;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name.as_str(), LABOR_COMPARE_PROMPT);

    let prompt_arguments = serde_json::json!({
        "topic": "육아휴직",
        "target_institution": "cni",
        "institutions": "cni,ctp",
        "query_variants": "육아 휴직,부모 휴직"
    })
    .as_object()
    .expect("prompt arguments must be an object")
    .clone();
    let prompt_result = client
        .get_prompt(
            GetPromptRequestParams::new(LABOR_COMPARE_PROMPT).with_arguments(prompt_arguments),
        )
        .await?;
    let prompt_text = prompt_result
        .messages
        .iter()
        .find_map(|message| match &message.content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("labor_compare prompt did not include text content"))?;
    assert!(prompt_text.contains("주제: 육아휴직"));
    assert!(prompt_text.contains("우리 기관(target_institution): cni"));
    assert!(prompt_text.contains("대조 기관(institutions, 쉼표 구분): cni,ctp"));
    assert!(prompt_text.contains("검색 변형(query_variants, 쉼표 구분): 육아 휴직,부모 휴직"));
    let missing_required = serde_json::json!({ "topic": "육아휴직" })
        .as_object()
        .expect("prompt arguments must be an object")
        .clone();
    assert!(client
        .get_prompt(
            GetPromptRequestParams::new(LABOR_COMPARE_PROMPT).with_arguments(missing_required),
        )
        .await
        .is_err());

    let arguments = serde_json::from_value(serde_json::json!({
        "query": "항공운임",
        "top_k": 5
    }))?;
    let result = client
        .call_tool(CallToolRequestParams::new(SEARCH_RULES_TOOL).with_arguments(arguments))
        .await?;
    assert_ne!(result.is_error, Some(true));

    let payload = tool_result_json(result)?;
    let search_result: SearchRulesResult = serde_json::from_value(payload)?;
    assert!(!search_result.hits.is_empty());
    assert_eq!(search_result.hits[0].article_id, "여비지급규칙#제12조");
    assert_freshness_meta(search_result.meta);

    let arguments = serde_json::from_value(serde_json::json!({
        "topic": "항공운임",
        "institutions": ["cni"]
    }))?;
    let result = client
        .call_tool(CallToolRequestParams::new(COMPARE_RULES_TOOL).with_arguments(arguments))
        .await?;
    assert_ne!(result.is_error, Some(true));

    let payload = tool_result_json(result)?;
    let compare_result: CompareRulesResult = serde_json::from_value(payload)?;
    assert_eq!(compare_result.topic, "항공운임");
    assert_eq!(compare_result.institutions.len(), 1);
    assert_eq!(compare_result.institutions[0].institution, "cni");
    assert!(compare_result.institutions[0]
        .provisions
        .iter()
        .any(|provision| provision.id == "여비지급규칙#제12조"));

    let result = client
        .call_tool(CallToolRequestParams::new(STATUS_TOOL))
        .await?;
    assert_ne!(result.is_error, Some(true));
    let payload = tool_result_json(result)?;
    let status: public_rules_mcp::StatusResult = serde_json::from_value(payload)?;
    assert_eq!(status.institution, "cni");
    assert_eq!(status.source_commit, "http-fixture");

    client.cancel().await?;
    server_handle.abort();
    let _ = server_handle.await;

    let log_text = fs::read_to_string(query_log_path)?;
    let events = log_text
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let search_event = events
        .iter()
        .find(|event| {
            event.get("tool").and_then(serde_json::Value::as_str) == Some(SEARCH_RULES_TOOL)
        })
        .ok_or_else(|| anyhow::anyhow!("search_rules query log event missing"))?;
    assert_eq!(
        search_event
            .pointer("/params/query")
            .and_then(serde_json::Value::as_str),
        Some("항공운임")
    );
    assert_eq!(
        search_event
            .pointer("/result/article_ids/0")
            .and_then(serde_json::Value::as_str),
        Some("여비지급규칙#제12조")
    );
    assert!(search_event
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64)
        .is_some());
    Ok(())
}

#[tokio::test]
async fn streamable_http_searches_multiple_packs_with_institution_labels() -> anyhow::Result<()> {
    let cni_root = make_institution_pack("cni", "2026-02-27", "직원은 육아휴직을 신청할 수 있다.")?;
    let ctp_root =
        make_institution_pack("ctp", "2026-03-01", "임직원 육아휴직 기간은 별도로 정한다.")?;
    let mut config = fixture_config(cni_root.clone());
    config.extra_packs.push(public_rules_mcp::ExtraPackConfig {
        institution: "ctp".to_string(),
        pack: PackConfig {
            path: Some(ctp_root),
            url: None,
            effective: Some("2026-03-01".to_string()),
            source_commit: Some("http-fixture-ctp".to_string()),
        },
    });

    let addr = unused_loopback_addr().await?;
    let server_handle = tokio::spawn(public_rules_mcp::run_server_with_transport_args(
        config,
        TransportArgs {
            transport: ServerTransport::Http,
            bind_addr: addr,
            allowed_hosts: Vec::new(),
            query_log_path: None,
            auth_token: None,
        },
    ));
    let url = format!("http://{addr}/mcp");
    let client = connect_with_retry(&url).await?;

    let arguments = serde_json::from_value(serde_json::json!({
        "query": "육아휴직",
        "top_k": 5
    }))?;
    let result = client
        .call_tool(CallToolRequestParams::new(SEARCH_RULES_TOOL).with_arguments(arguments))
        .await?;
    assert_ne!(result.is_error, Some(true));

    let payload = tool_result_json(result)?;
    let search_result: SearchRulesResult = serde_json::from_value(payload)?;
    let hits = search_result
        .hits
        .iter()
        .map(|hit| (hit.institution.as_str(), hit.article_id.as_str()))
        .collect::<Vec<_>>();
    assert!(hits.contains(&("cni", "cni/인사규정#제10조")));
    assert!(hits.contains(&("ctp", "ctp/인사규정#제10조")));

    client.cancel().await?;
    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn streamable_http_rejects_untrusted_host_header() -> anyhow::Result<()> {
    let fixture_root = make_fixture_pack()?;
    let addr = unused_loopback_addr().await?;
    let server_handle = tokio::spawn(public_rules_mcp::run_http_server(
        fixture_config(fixture_root),
        addr,
    ));

    let status = raw_mcp_post_status(addr, "evil.example.com").await?;

    server_handle.abort();
    let _ = server_handle.await;
    assert_eq!(status, 403);
    Ok(())
}

#[tokio::test]
async fn streamable_http_allows_configured_host_header() -> anyhow::Result<()> {
    let fixture_root = make_fixture_pack()?;
    let addr = unused_loopback_addr().await?;
    let server_handle = tokio::spawn(public_rules_mcp::run_http_server_with_allowed_hosts(
        fixture_config(fixture_root),
        addr,
        vec!["allowed.example.test".to_string()],
    ));

    let status = raw_mcp_post_status(addr, "allowed.example.test").await?;

    server_handle.abort();
    let _ = server_handle.await;
    assert_ne!(status, 403);
    Ok(())
}

#[tokio::test]
async fn streamable_http_negotiates_known_version_across_full_handshake() -> anyhow::Result<()> {
    let fixture_root = make_fixture_pack()?;
    let addr = unused_loopback_addr().await?;
    let server_handle = tokio::spawn(public_rules_mcp::run_http_server(
        fixture_config(fixture_root),
        addr,
    ));

    let info = raw_http_get_response(addr, "127.0.0.1").await?;
    assert_eq!(http_status(&info)?, 200);
    assert_eq!(
        header_value(&info, "content-type"),
        Some("application/json")
    );
    assert_eq!(
        response_json(&info)?.get("protocol"),
        Some(&serde_json::Value::String("2025-03-26".to_string()))
    );

    let initialize = raw_http_post_response(
        addr,
        "127.0.0.1",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-compatible-test","version":"1"}}}"#,
        None,
        None,
    )
    .await?;
    assert_eq!(http_status(&initialize)?, 200);
    assert_eq!(
        header_value(&initialize, "content-type"),
        Some("application/json")
    );
    let initialize_json = response_json(&initialize)?;
    assert_eq!(
        initialize_json.pointer("/result/protocolVersion"),
        Some(&serde_json::Value::String("2025-06-18".to_string()))
    );

    let initialized = raw_http_post_response(
        addr,
        "127.0.0.1",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        Some("2025-06-18"),
        None,
    )
    .await?;
    assert_eq!(http_status(&initialized)?, 202);

    let tools = raw_http_post_response(
        addr,
        "127.0.0.1",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        Some("2025-06-18"),
        None,
    )
    .await?;
    assert_eq!(http_status(&tools)?, 200);
    assert_eq!(
        header_value(&tools, "content-type"),
        Some("application/json")
    );
    let tools_json = response_json(&tools)?;
    let tool_names = tools_json
        .pointer("/result/tools")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("tools/list response did not contain a tools array"))?
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .collect::<BTreeSet<_>>();
    assert!(tool_names.contains(STATUS_TOOL));
    assert!(tool_names.contains(COMPARE_RULES_TOOL));

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn streamable_http_enforces_bearer_auth_and_preserves_host_validation() -> anyhow::Result<()>
{
    let fixture_root = make_fixture_pack()?;
    let query_log_path = fixture_root.join("auth-query-log.jsonl");
    let addr = unused_loopback_addr().await?;
    let token = format!(
        "fixture-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let server_handle = tokio::spawn(public_rules_mcp::run_server_with_transport_args(
        fixture_config(fixture_root),
        TransportArgs {
            transport: ServerTransport::Http,
            bind_addr: addr,
            allowed_hosts: Vec::new(),
            query_log_path: Some(query_log_path.clone()),
            auth_token: Some(AuthToken::new(token.clone())?),
        },
    ));

    assert_eq!(
        http_status(&raw_http_get_response(addr, "127.0.0.1").await?)?,
        200
    );
    assert_eq!(
        http_status(&raw_http_get_response(addr, "evil.example.com").await?)?,
        403
    );
    assert_eq!(
        raw_mcp_initialize_status(addr, "127.0.0.1", None).await?,
        401
    );
    assert_eq!(
        raw_mcp_initialize_status(addr, "127.0.0.1", Some("wrong-token")).await?,
        401
    );
    assert_eq!(
        raw_mcp_initialize_status(addr, "127.0.0.1", Some(&token)).await?,
        200
    );
    assert_eq!(
        raw_mcp_initialize_status(addr, "evil.example.com", Some(&token)).await?,
        403
    );
    assert_eq!(
        raw_mcp_initialize_status(addr, "evil.example.com", None).await?,
        403
    );

    let url = format!("http://{addr}/mcp");
    let client = connect_with_retry_auth(&url, Some(&token)).await?;
    let result = client
        .call_tool(CallToolRequestParams::new(STATUS_TOOL))
        .await?;
    assert_ne!(result.is_error, Some(true));
    client.cancel().await?;

    server_handle.abort();
    let _ = server_handle.await;
    let query_log = fs::read_to_string(query_log_path)?;
    assert!(!query_log.contains(&token));
    Ok(())
}

#[tokio::test]
async fn streamable_http_rejects_non_loopback_bind_without_auth() -> anyhow::Result<()> {
    let fixture_root = make_fixture_pack()?;
    let error = public_rules_mcp::run_server_with_transport_args(
        fixture_config(fixture_root),
        TransportArgs {
            transport: ServerTransport::Http,
            bind_addr: "0.0.0.0:0".parse()?,
            allowed_hosts: Vec::new(),
            query_log_path: None,
            auth_token: None,
        },
    )
    .await
    .expect_err("non-loopback HTTP must fail closed without authentication");

    assert!(error
        .to_string()
        .contains("authentication is required for a non-loopback"));
    Ok(())
}

fn sort_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .map(|(key, value)| (key, sort_json(value)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort_json).collect())
        }
        other => other,
    }
}

/// 스냅샷 비교. `UPDATE_SNAPSHOTS=1`일 때만 파일을 갱신한다.
fn assert_snapshot(name: &str, value: serde_json::Value) -> anyhow::Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(name);
    let actual = format!("{}\n", serde_json::to_string_pretty(&sort_json(value))?);
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        fs::create_dir_all(path.parent().expect("snapshot dir"))?;
        fs::write(&path, &actual)?;
        return Ok(());
    }
    let expected = fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "snapshot {} unreadable ({error}); run with UPDATE_SNAPSHOTS=1",
            path.display()
        )
    })?;
    assert_eq!(
        actual, expected,
        "snapshot {name} differs; rerun with UPDATE_SNAPSHOTS=1 and review the diff"
    );
    Ok(())
}

async fn snapshot_client() -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::InitializeRequestParams>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
)> {
    let fixture_root = make_fixture_pack()?;
    let addr = unused_loopback_addr().await?;
    let server_handle = tokio::spawn(public_rules_mcp::run_server_with_transport_args(
        fixture_config(fixture_root),
        TransportArgs {
            transport: ServerTransport::Http,
            bind_addr: addr,
            allowed_hosts: Vec::new(),
            query_log_path: None,
            auth_token: None,
        },
    ));
    let client = connect_with_retry(&format!("http://{addr}/mcp")).await?;
    Ok((client, server_handle))
}

#[tokio::test]
async fn tools_list_matches_snapshot() -> anyhow::Result<()> {
    let (client, server_handle) = snapshot_client().await?;
    let mut tools = client.list_all_tools().await?;
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let outcome = assert_snapshot("tools_list.json", serde_json::to_value(&tools)?);
    client.cancel().await?;
    server_handle.abort();
    let _ = server_handle.await;
    outcome
}

#[tokio::test]
async fn prompts_list_matches_snapshot() -> anyhow::Result<()> {
    let (client, server_handle) = snapshot_client().await?;
    let mut prompts = client.list_all_prompts().await?;
    prompts.sort_by(|left, right| left.name.cmp(&right.name));
    let outcome = assert_snapshot("prompts_list.json", serde_json::to_value(&prompts)?);
    client.cancel().await?;
    server_handle.abort();
    let _ = server_handle.await;
    outcome
}

async fn connect_with_retry(
    url: &str,
) -> anyhow::Result<
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::InitializeRequestParams>,
> {
    connect_with_retry_auth(url, None).await
}

async fn connect_with_retry_auth(
    url: &str,
    bearer_token: Option<&str>,
) -> anyhow::Result<
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::InitializeRequestParams>,
> {
    let mut last_error = None;
    for _ in 0..20 {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        if let Some(token) = bearer_token {
            config = config.auth_header(token.to_string());
        }
        let transport = StreamableHttpClientTransport::from_config(config);
        match ClientInfo::default().serve(transport).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("HTTP MCP server did not start")))
}

fn tool_result_json(result: rmcp::model::CallToolResult) -> anyhow::Result<serde_json::Value> {
    if let Some(structured) = result.structured_content {
        return Ok(structured);
    }
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("tool result did not include JSON text content"))?;
    Ok(serde_json::from_str(text)?)
}

fn assert_freshness_meta(meta: FreshnessMeta) {
    assert_eq!(meta.effective, "2026-02-27");
    assert_eq!(meta.amended, "2026-02-27");
    assert_eq!(meta.source_commit, "http-fixture");
}

async fn unused_loopback_addr() -> anyhow::Result<SocketAddr> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr)
}

async fn raw_mcp_post_status(addr: SocketAddr, host: &str) -> anyhow::Result<u16> {
    raw_http_post_status(addr, host, "{}", None).await
}

async fn raw_mcp_initialize_status(
    addr: SocketAddr,
    host: &str,
    bearer_token: Option<&str>,
) -> anyhow::Result<u16> {
    raw_http_post_status(
        addr,
        host,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"auth-test","version":"1"}}}"#,
        bearer_token,
    )
    .await
}

async fn raw_http_post_status(
    addr: SocketAddr,
    host: &str,
    body: &str,
    bearer_token: Option<&str>,
) -> anyhow::Result<u16> {
    let response = raw_http_post_response(addr, host, body, None, bearer_token).await?;
    http_status(&response)
}

async fn raw_http_post_response(
    addr: SocketAddr,
    host: &str,
    body: &str,
    protocol_version: Option<&str>,
    bearer_token: Option<&str>,
) -> anyhow::Result<String> {
    let mut last_error = None;
    for _ in 0..20 {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(mut stream) => {
                let authorization = bearer_token
                    .map(|token| format!("Authorization: Bearer {token}\r\n"))
                    .unwrap_or_default();
                let protocol_version = protocol_version
                    .map(|version| format!("MCP-Protocol-Version: {version}\r\n"))
                    .unwrap_or_default();
                let request = format!(
                    "POST /mcp HTTP/1.1\r\nHost: {host}\r\n{authorization}{protocol_version}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(request.as_bytes()).await?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await?;
                let response = String::from_utf8(response)?;
                return Ok(response);
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("HTTP MCP server did not start")))
}

async fn raw_http_get_response(addr: SocketAddr, host: &str) -> anyhow::Result<String> {
    let mut last_error = None;
    for _ in 0..20 {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(mut stream) => {
                let request = format!(
                    "GET /mcp HTTP/1.1\r\nHost: {host}\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(request.as_bytes()).await?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await?;
                return Ok(String::from_utf8(response)?);
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("HTTP MCP server did not start")))
}

fn http_status(response: &str) -> anyhow::Result<u16> {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("HTTP response status line missing"))?
        .parse::<u16>()
        .map_err(anyhow::Error::from)
}

fn header_value<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn response_json(response: &str) -> anyhow::Result<serde_json::Value> {
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| anyhow::anyhow!("HTTP response body missing"))?;
    if let Some(data) = body.lines().find_map(|line| line.strip_prefix("data: ")) {
        return Ok(serde_json::from_str(data)?);
    }
    Ok(serde_json::from_str(body.trim())?)
}

fn fixture_config(fixture_root: std::path::PathBuf) -> ServerConfig {
    ServerConfig {
        institution: "cni".to_string(),
        pack: PackConfig {
            path: Some(fixture_root),
            url: None,
            effective: Some("2026-02-27".to_string()),
            source_commit: Some("http-fixture".to_string()),
        },
        extra_packs: Vec::new(),
        vectors: VectorConfig::default(),
    }
}

fn make_fixture_pack() -> anyhow::Result<std::path::PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "public-rules-mcp-http-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let rule_dir = root.join("여비지급규칙");
    fs::create_dir_all(&rule_dir)?;
    write_article(
        &rule_dir.join("제12조.md"),
        "제12조",
        "항공운임의 지급",
        "① 원장은 출장업무가 시급을 요할 때 항공운임 지급 여부를 결정한다.",
    )?;
    write_article(
        &rule_dir.join("제13조.md"),
        "제13조",
        "숙박비 지급",
        "① 숙박비는 별표 기준에 따라 지급한다.",
    )?;
    Ok(root)
}

fn make_institution_pack(
    institution: &str,
    effective: &str,
    body: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "public-rules-mcp-http-{institution}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let rule_dir = root.join("인사규정");
    fs::create_dir_all(&rule_dir)?;
    write_article_with_institution(
        &rule_dir.join("제10조.md"),
        institution,
        effective,
        "인사규정",
        "제10조",
        "육아휴직",
        body,
    )?;
    Ok(root)
}

fn write_article(path: &Path, article: &str, title: &str, body: &str) -> anyhow::Result<()> {
    write_article_with_institution(
        path,
        "cni",
        "2026-02-27",
        "여비지급규칙",
        article,
        title,
        body,
    )
}

fn write_article_with_institution(
    path: &Path,
    institution: &str,
    effective: &str,
    rule: &str,
    article: &str,
    title: &str,
    body: &str,
) -> anyhow::Result<()> {
    fs::write(
        path,
        format!(
            r#"---
institution: {institution}
rule: {rule}
article: {article}
title: {title}
effective: {effective}
amended: {effective}
status: active
supersedes: null
legal_basis:
  - law: 근로기준법
    article: 제60조
    mst: "265959"
refs: []
---
{body}
"#
        ),
    )?;
    Ok(())
}
