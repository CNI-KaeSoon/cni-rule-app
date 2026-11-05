//! stdio 전송에서 표준출력은 JSON-RPC 메시지만 담아야 한다.
//! 진단 로그는 표준오류로 나가야 stdio 클라이언트가 스트림을 파싱할 수 있다.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn stdio_stdout_carries_only_json_rpc_messages() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join(format!(
        "public-rules-mcp-stdio-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let rule_dir = root.join("여비지급규칙");
    fs::create_dir_all(&rule_dir)?;
    write_article(&rule_dir.join("제12조.md"))?;
    let config_path = root.join("server.toml");
    fs::write(
        &config_path,
        format!(
            "institution = \"cni\"\n\n[pack]\npath = \"{}\"\neffective = \"2026-02-27\"\nsource_commit = \"stdio-fixture\"\n",
            root.display()
        ),
    )?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_public-rules-mcp"))
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout");
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stdout_reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().expect("stderr");
    let stderr_reader = thread::spawn(move || {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        err
    });

    let mut stdin = child.stdin.take().expect("stdin");
    for request in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"stdio-test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    ] {
        writeln!(stdin, "{request}")?;
    }
    stdin.flush()?;

    // tools/list 응답(id 2)까지 받은 뒤 stdin 을 닫아 서버를 끝낸다.
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(remaining) {
            Ok(line) => {
                let done = serde_json::from_str::<serde_json::Value>(&line)
                    .map(|message| message["id"] == 2)
                    .unwrap_or(false);
                lines.push(line);
                if done {
                    break;
                }
            }
            Err(_) => {
                let _ = child.kill();
                panic!("tools/list 응답을 20초 안에 받지 못했다. stdout: {lines:?}");
            }
        }
    }
    drop(stdin);
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            panic!("stdin 을 닫았는데 10초 안에 끝나지 않았다");
        }
        thread::sleep(Duration::from_millis(50));
    };
    stdout_reader.join().expect("stdout reader");
    lines.extend(line_rx.try_iter());
    let err = stderr_reader.join().expect("stderr reader");

    let lines: Vec<&String> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    for line in &lines {
        let message: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout 에 JSON 이 아닌 줄: {line:?} ({e})"));
        assert_eq!(
            message["jsonrpc"], "2.0",
            "JSON-RPC 메시지가 아니다: {line}"
        );
    }
    let initialize: serde_json::Value = serde_json::from_str(lines[0])?;
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");
    assert!(
        err.contains("mcp_initialize requested_protocol_version=2025-06-18"),
        "진단 로그는 stderr 로 가야 한다. stderr: {err}"
    );
    assert!(status.success(), "stdin 종료 후 비정상 종료: {status}");
    let _ = fs::remove_dir_all(&root);
    Ok(())
}

fn write_article(path: &Path) -> anyhow::Result<()> {
    fs::write(
        path,
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
① 원장은 출장업무가 시급을 요할 때 항공운임 지급 여부를 결정한다.
"#,
    )?;
    Ok(())
}
