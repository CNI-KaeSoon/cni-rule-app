from __future__ import annotations

import argparse
import difflib
import json
import random
import re
import unicodedata
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

import pdfplumber


DEFAULT_SAMPLE = 40
DEFAULT_SEED = 20260708
MATCH_THRESHOLD = 0.9
PARTIAL_THRESHOLD = 0.6


@dataclass(frozen=True)
class ExtractedItem:
    path: Path
    item_id: str
    institution: str
    rule: str
    kind: str
    label: str
    pages: tuple[int, int]
    body: str


@dataclass(frozen=True)
class VerificationCase:
    item: ExtractedItem
    score: float
    status: str
    pdf_excerpt: str
    body_excerpt: str


def normalize_text(text: str) -> str:
    text = unicodedata.normalize("NFC", text)
    text = text.replace("\u00a0", " ")
    text = text.replace("", '"')
    text = re.sub(r"\s+", " ", text)
    return text.strip()


def content_tokens(text: str) -> list[str]:
    return re.findall(r"[0-9A-Za-z가-힣]+", normalize_text(text))


def similarity_score(expected: str, actual: str) -> float:
    expected_norm = normalize_text(expected)
    actual_norm = normalize_text(actual)
    if not expected_norm:
        return 1.0
    if expected_norm in actual_norm:
        return 1.0

    expected_tokens = [token for token in content_tokens(expected_norm) if len(token) > 1]
    actual_counts = Counter(token for token in content_tokens(actual_norm) if len(token) > 1)
    if not expected_tokens:
        return difflib.SequenceMatcher(None, expected_norm, actual_norm).ratio()

    expected_counts = Counter(expected_tokens)
    overlap = sum(min(count, actual_counts.get(token, 0)) for token, count in expected_counts.items())
    token_coverage = overlap / sum(expected_counts.values())
    if token_coverage >= PARTIAL_THRESHOLD:
        return token_coverage
    return max(token_coverage, difflib.SequenceMatcher(None, expected_norm, actual_norm).ratio())


def verdict(score: float) -> str:
    if score >= MATCH_THRESHOLD:
        return "match"
    if score >= PARTIAL_THRESHOLD:
        return "partial"
    return "mismatch"


def parse_markdown_file(path: Path) -> ExtractedItem:
    raw = path.read_text(encoding="utf-8")
    meta_text, body = split_frontmatter(raw)
    meta = parse_frontmatter(meta_text)
    kind = str(meta.get("type") or "article")
    label = str(meta.get("annex") if kind == "annex" else meta.get("article"))
    institution = str(meta.get("institution") or "unknown")
    rule = str(meta.get("rule") or path.parent.name)
    pages = parse_page_range(meta.get("source_pages") or meta.get("pages"))
    item_id = f"{path.parent.name}#{label}"
    clean_body = body.split("\n## Extracted tables", 1)[0].strip()
    return ExtractedItem(
        path=path,
        item_id=item_id,
        institution=institution,
        rule=rule,
        kind=kind,
        label=label,
        pages=pages,
        body=clean_body,
    )


def split_frontmatter(raw: str) -> tuple[str, str]:
    if not raw.startswith("---\n"):
        return "", raw
    _start, rest = raw.split("---\n", 1)
    if "\n---\n" not in rest:
        return "", raw
    meta, body = rest.split("\n---\n", 1)
    return meta, body


def parse_frontmatter(meta_text: str) -> dict[str, object]:
    meta: dict[str, object] = {}
    lines = meta_text.splitlines()
    index = 0
    while index < len(lines):
        line = lines[index]
        if not line.strip() or line.startswith("  "):
            index += 1
            continue
        if ":" not in line:
            index += 1
            continue
        key, value = line.split(":", 1)
        key = key.strip()
        value = value.strip()
        if value == "[]":
            meta[key] = []
        elif value.startswith("[") and value.endswith("]"):
            meta[key] = [part.strip() for part in value.strip("[]").split(",") if part.strip()]
        elif value.startswith('"') and value.endswith('"'):
            meta[key] = value[1:-1]
        elif value in {"null", "None"}:
            meta[key] = None
        else:
            meta[key] = value
        index += 1
    return meta


def parse_page_range(value: object) -> tuple[int, int]:
    if isinstance(value, list) and len(value) >= 2:
        start, end = int(value[0]), int(value[1])
    elif isinstance(value, str):
        nums = [int(num) for num in re.findall(r"\d+", value)]
        if len(nums) < 2:
            raise ValueError(f"cannot parse page range: {value!r}")
        start, end = nums[0], nums[1]
    else:
        raise ValueError(f"missing page range: {value!r}")
    if start <= 0 or end < start:
        raise ValueError(f"invalid page range: {value!r}")
    return start, end


def load_items(output_dir: Path) -> list[ExtractedItem]:
    rules_dir = output_dir / "rules"
    pages_dir = output_dir / "pages"
    if not rules_dir.is_dir():
        raise FileNotFoundError(f"rules directory not found: {rules_dir}")
    if not pages_dir.is_dir():
        raise FileNotFoundError(f"pages directory not found: {pages_dir}")
    items = [parse_markdown_file(path) for path in sorted(rules_dir.rglob("*.md"))]
    return [item for item in items if normalize_text(item.body)]


def balanced_sample(items: Iterable[ExtractedItem], sample_size: int, seed: int) -> list[ExtractedItem]:
    groups: dict[str, list[ExtractedItem]] = defaultdict(list)
    for item in items:
        groups[item.rule].append(item)
    rng = random.Random(seed)
    shuffled_groups = list(groups.values())
    rng.shuffle(shuffled_groups)
    for group in shuffled_groups:
        rng.shuffle(group)

    selected: list[ExtractedItem] = []
    while len(selected) < sample_size and any(shuffled_groups):
        for group in shuffled_groups:
            if not group:
                continue
            selected.append(group.pop())
            if len(selected) >= sample_size:
                break
        shuffled_groups = [group for group in shuffled_groups if group]
    return selected


def extract_pdf_text(pdf_path: Path, page_range: tuple[int, int]) -> str:
    start, end = page_range
    parts: list[str] = []
    with pdfplumber.open(pdf_path) as pdf:
        max_page = len(pdf.pages)
        if end > max_page:
            raise ValueError(f"page range {page_range} exceeds PDF page count {max_page}")
        for page_no in range(start, end + 1):
            text = pdf.pages[page_no - 1].extract_text(x_tolerance=1, y_tolerance=3) or ""
            parts.append(text)
    return "\n".join(parts)


def verify_items(items: list[ExtractedItem], pdf_path: Path) -> list[VerificationCase]:
    cases: list[VerificationCase] = []
    for item in items:
        pdf_text = extract_pdf_text(pdf_path, item.pages)
        score = similarity_score(item.body, pdf_text)
        cases.append(
            VerificationCase(
                item=item,
                score=score,
                status=verdict(score),
                pdf_excerpt=normalize_text(pdf_text)[:200],
                body_excerpt=normalize_text(item.body)[:200],
            )
        )
    return cases


def summarize(cases: list[VerificationCase]) -> dict[str, dict[str, int]]:
    summary: dict[str, dict[str, int]] = {}
    for case in cases:
        inst = case.item.institution
        summary.setdefault(inst, {"match": 0, "partial": 0, "mismatch": 0})
        summary[inst][case.status] += 1
    return summary


def case_to_json(case: VerificationCase) -> dict[str, object]:
    return {
        "id": case.item.item_id,
        "institution": case.item.institution,
        "rule": case.item.rule,
        "kind": case.item.kind,
        "label": case.item.label,
        "pages": list(case.item.pages),
        "score": round(case.score, 4),
        "status": case.status,
        "path": str(case.item.path),
        "body_excerpt": case.body_excerpt,
        "pdf_excerpt": case.pdf_excerpt,
    }


def write_json_report(
    output_dir: Path,
    pdf_path: Path,
    sample: int,
    seed: int,
    article_cases: list[VerificationCase],
    annex_cases: list[VerificationCase],
) -> None:
    cases = article_cases + annex_cases
    payload = {
        "pdf": str(pdf_path),
        "output_dir": str(output_dir),
        "sample": sample,
        "annex_sample": max(1, sample // 4),
        "seed": seed,
        "summary": summarize(cases),
        "cases": [case_to_json(case) for case in cases],
        "mismatches": [case_to_json(case) for case in cases if case.status == "mismatch"],
    }
    (output_dir / "verify.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def write_markdown_report(
    output_dir: Path,
    pdf_path: Path,
    article_cases: list[VerificationCase],
    annex_cases: list[VerificationCase],
) -> None:
    cases = article_cases + annex_cases
    lines = [
        "# Extraction Verification Report",
        "",
        f"- PDF: `{pdf_path}`",
        f"- Output: `{output_dir}`",
        f"- Cases: articles={len(article_cases)}, annexes={len(annex_cases)}, total={len(cases)}",
        "",
        "## Summary",
        "",
        "| Institution | Match | Partial | Mismatch |",
        "| --- | ---: | ---: | ---: |",
    ]
    for institution, counts in sorted(summarize(cases).items()):
        lines.append(
            f"| {institution} | {counts.get('match', 0)} | {counts.get('partial', 0)} | {counts.get('mismatch', 0)} |"
        )
    lines.extend(["", "## Mismatches", ""])
    mismatches = [case for case in cases if case.status == "mismatch"]
    if not mismatches:
        lines.append("No mismatches.")
    for case in mismatches:
        lines.extend(
            [
                f"### {case.item.item_id}",
                "",
                f"- Institution: {case.item.institution}",
                f"- Rule: {case.item.rule}",
                f"- Kind: {case.item.kind}",
                f"- Pages: {case.item.pages[0]}-{case.item.pages[1]}",
                f"- Score: {case.score:.4f}",
                f"- Markdown excerpt: {case.body_excerpt}",
                f"- pdfplumber excerpt: {case.pdf_excerpt}",
                "",
            ]
        )
    (output_dir / "verify-report.md").write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


def run(output_dir: Path, pdf_path: Path, sample: int, seed: int) -> tuple[list[VerificationCase], list[VerificationCase]]:
    items = load_items(output_dir)
    articles = [item for item in items if item.kind != "annex"]
    annexes = [item for item in items if item.kind == "annex"]
    article_sample = balanced_sample(articles, min(sample, len(articles)), seed)
    annex_sample_size = min(max(1, sample // 4), len(annexes)) if annexes else 0
    annex_sample = balanced_sample(annexes, annex_sample_size, seed + 1)
    article_cases = verify_items(article_sample, pdf_path)
    annex_cases = verify_items(annex_sample, pdf_path)
    write_json_report(output_dir, pdf_path, sample, seed, article_cases, annex_cases)
    write_markdown_report(output_dir, pdf_path, article_cases, annex_cases)
    return article_cases, annex_cases


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify pack Markdown extraction against independent pdfplumber text.")
    parser.add_argument("output_dir", type=Path, help="Pack output directory containing rules/ and pages/.")
    parser.add_argument("pdf", type=Path, help="Original source PDF.")
    parser.add_argument("--sample", type=int, default=DEFAULT_SAMPLE, help=f"Article sample size. Default: {DEFAULT_SAMPLE}.")
    parser.add_argument("--seed", type=int, default=DEFAULT_SEED, help=f"Fixed random seed. Default: {DEFAULT_SEED}.")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    article_cases, annex_cases = run(args.output_dir, args.pdf, args.sample, args.seed)
    cases = article_cases + annex_cases
    print(json.dumps({"summary": summarize(cases), "cases": len(cases)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
