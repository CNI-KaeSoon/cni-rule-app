# cni-rule source workspace

This directory is the application monorepo for the CNI rules RAG desktop app.

## Layout

- `crates/rules-core`: shared rule index, ontology, graph, pack, and search API contracts.
- `crates/rules-engines`: chat engine contracts and future CLI/API adapters.
- `crates/public-rules-mcp`: generic public rules MCP server binary stub.
- `crates/cni-rules-mcp`: CNI preset MCP wrapper binary stub.
- `app`: future Tauri frontend shell. Lane A will initialize `src-tauri`.
- `pipeline`: future PDF-to-Markdown and pack build pipeline tools.

The PDF currently stored in this directory is source material and is intentionally left untouched by this scaffold.
