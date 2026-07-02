use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

use futures_util::StreamExt;
use rules_core::{default_pack_status, RulesIndex, TantivyRulesIndex};
use rules_engines::{
    codex_cli_engine, ChatEngine, ChatRequest, ContextBlock, Mode, Msg, PromptBuilder,
};

const DEFAULT_RULES_DIR: &str = "/Users/kaesoon/Projects/cni-rule/04_data/90_index-build/rules";
const DEFAULT_EFFECTIVE_DATE: &str = "2026-02-27";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let question = env::args().nth(1).ok_or(
        "usage: cargo run -p rules-engines --example e2e --features korean-tokenizer -- \"<질문>\"",
    )?;

    let started_at = Instant::now();
    let index = TantivyRulesIndex::from_articles_dir(
        PathBuf::from(DEFAULT_RULES_DIR),
        default_pack_status("cni", DEFAULT_EFFECTIVE_DATE),
    )?;

    let hits = index.search(&question, 5, None);
    let context = hits
        .iter()
        .filter_map(|hit| {
            index
                .get_article(&hit.article_id)
                .map(|article| ContextBlock {
                    id: article.id,
                    title: format!("{} {}", article.rule, article.article),
                    body: article.body,
                    source: format!("{}@{}", hit.rule, hit.effective),
                })
        })
        .collect::<Vec<_>>();

    let request = ChatRequest {
        mode: Mode::Interpret,
        messages: vec![Msg {
            role: "user".to_string(),
            content: question,
        }],
        context,
    };

    let prompt = PromptBuilder::build(&request);
    let engine = codex_cli_engine();
    eprintln!("engine_status={:?}", engine.probe());
    eprintln!("prompt_bytes={}", prompt.len());
    eprintln!(
        "searched_article_ids={}",
        hits.iter()
            .map(|hit| hit.article_id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut stream = engine.send(request);
    let mut delta_count = 0_usize;
    let mut final_answer = String::new();
    let mut stdout = io::stdout().lock();

    while let Some(delta) = stream.next().await {
        if delta.done {
            break;
        }
        delta_count += 1;
        final_answer.push_str(&delta.content);
        write!(stdout, "{}", delta.content)?;
        stdout.flush()?;
    }

    let cited_ids = hits
        .iter()
        .filter(|hit| final_answer.contains(&hit.article_id))
        .map(|hit| hit.article_id.as_str())
        .collect::<Vec<_>>();

    writeln!(stdout)?;
    writeln!(stdout, "\n--- e2e verification ---")?;
    writeln!(stdout, "stream_delta_count={delta_count}")?;
    writeln!(
        stdout,
        "searched_article_id_cited={}",
        if cited_ids.is_empty() { "no" } else { "yes" }
    )?;
    writeln!(
        stdout,
        "cited_article_ids={}",
        if cited_ids.is_empty() {
            "(none)".to_string()
        } else {
            cited_ids.join(",")
        }
    )?;
    writeln!(stdout, "elapsed_ms={}", started_at.elapsed().as_millis())?;

    Ok(())
}
