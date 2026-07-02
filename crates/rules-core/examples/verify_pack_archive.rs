use rules_core::{RulesIndex, TantivyRulesIndex};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let archive_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .expect("crates/rules-core has a repository ancestor")
                .join("04_data/90_index-build/pack-cni-2026-02-27.tar.zst")
        });
    let query = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "여비지급규칙 제1조".to_string());

    let index = TantivyRulesIndex::from_pack_archive(&archive_path)?;
    let hits = index.search(&query, 5, None);
    println!(
        "status institution={} effective={} source_commit={}",
        index.status().institution,
        index.status().effective_date,
        index.status().source_commit
    );
    println!("query={query}");
    println!("hits={}", hits.len());
    for hit in hits.iter().take(5) {
        println!(
            "{}\t{:.6}\t{}\t{}",
            hit.article_id, hit.score, hit.rule, hit.title
        );
    }
    if hits.is_empty() {
        anyhow::bail!("pack archive loaded but search returned no hits");
    }
    Ok(())
}
