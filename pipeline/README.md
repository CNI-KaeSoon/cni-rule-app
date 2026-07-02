# Pipeline

Build pipeline workspace for PDF-to-Markdown conversion, article normalization, graph generation, and release pack creation.

Pipeline tools are expected to run on macOS and produce the pack assets consumed by `rules-core`.

## PDF to Markdown

The M0 Lane D pipeline parses the CNI rule-book PDF with PyMuPDF, splits rule units from the table of contents, splits articles by `제N조` headings, and emits contract-compatible Markdown files.

```bash
UV_CACHE_DIR=../../99_tmp/uv-cache \
UV_PYTHON_INSTALL_DIR=../../99_tmp/uv-python \
uv run --python 3.13 cni-rule-pipeline
```

Outputs:

- `../../04_data/90_index-build/rules/<규정슬러그>/<조문키>.md`
- `../../04_data/90_index-build/qa-report.md`
