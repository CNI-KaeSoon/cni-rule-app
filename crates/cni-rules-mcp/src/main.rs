const CNI_CONFIG: &str = r#"
institution = "cni"

[pack]
effective = "2026-02-27"
source_commit = "bundled-rules"
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config: public_rules_mcp::ServerConfig = toml::from_str(CNI_CONFIG)?;
    config.pack.path = Some(executable_rules_dir()?);
    public_rules_mcp::run_stdio_server(config).await
}

fn executable_rules_dir() -> anyhow::Result<std::path::PathBuf> {
    let executable = std::env::current_exe()?;
    let base = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current executable has no parent directory"))?;
    Ok(base.join("rules"))
}

#[cfg(test)]
mod tests {
    use super::{executable_rules_dir, CNI_CONFIG};
    use public_rules_mcp::ServerConfig;

    // §6 계약: cni-rules-mcp는 "동일 바이너리에 --config cni.toml 내장판" — 이 크레이트의
    // 유일한 책임은 올바른 내장 설정을 제공하는 것("로직 금지"). 그 설정 자체가
    // ServerConfig 스키마와 어긋나면 바이너리가 기동 즉시 깨지므로 최소 계약으로 고정한다.
    #[test]
    fn embedded_cni_config_parses_and_targets_cni_institution() {
        let config: ServerConfig = toml::from_str(CNI_CONFIG).expect("embedded config must parse");
        assert_eq!(config.institution, "cni");
        assert!(config.pack.path.is_none());
        assert_eq!(config.pack.effective.as_deref(), Some("2026-02-27"));
        assert_eq!(config.pack.source_commit.as_deref(), Some("bundled-rules"));
        assert!(executable_rules_dir().unwrap().is_absolute());
    }
}
