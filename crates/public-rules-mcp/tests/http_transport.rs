use public_rules_mcp::{
    FreshnessMeta, PackConfig, SearchRulesResult, ServerConfig, ServerTransport, TransportArgs,
    GET_ARTICLE_TOOL, GET_LEGAL_BASIS_TOOL, LIST_RULES_TOOL, SEARCH_RULES_TOOL, STATUS_TOOL,
};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo, ContentBlock},
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
        },
    ));
    let url = format!("http://{addr}/mcp");

    let client = connect_with_retry(&url).await?;
    let tool_names = client
        .list_all_tools()
        .await?
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        tool_names,
        BTreeSet::from([
            SEARCH_RULES_TOOL.to_string(),
            GET_ARTICLE_TOOL.to_string(),
            LIST_RULES_TOOL.to_string(),
            GET_LEGAL_BASIS_TOOL.to_string(),
            STATUS_TOOL.to_string(),
        ])
    );

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
        .map(|line| serde_json::from_str::<serde_json::Value>(line))
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

async fn connect_with_retry(
    url: &str,
) -> anyhow::Result<
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::InitializeRequestParams>,
> {
    let mut last_error = None;
    for _ in 0..20 {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url.to_string()),
        );
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
    let mut last_error = None;
    for _ in 0..20 {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(mut stream) => {
                let body = "{}";
                let request = format!(
                    "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(request.as_bytes()).await?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await?;
                let response = String::from_utf8(response)?;
                let status = response
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .ok_or_else(|| anyhow::anyhow!("HTTP response status line missing"))?
                    .parse::<u16>()?;
                return Ok(status);
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

fn fixture_config(fixture_root: std::path::PathBuf) -> ServerConfig {
    ServerConfig {
        institution: "cni".to_string(),
        pack: PackConfig {
            path: Some(fixture_root),
            url: None,
            effective: Some("2026-02-27".to_string()),
            source_commit: Some("http-fixture".to_string()),
        },
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

fn write_article(path: &Path, article: &str, title: &str, body: &str) -> anyhow::Result<()> {
    fs::write(
        path,
        format!(
            r#"---
institution: cni
rule: 여비지급규칙
article: {article}
title: {title}
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
{body}
"#
        ),
    )?;
    Ok(())
}
