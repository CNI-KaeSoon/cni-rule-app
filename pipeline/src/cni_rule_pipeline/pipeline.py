from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import re
import sys
import unicodedata
from collections import Counter
from pathlib import Path
from typing import Iterable

import fitz

SOURCE_EFFECTIVE_DATE = "2026-02-27"
INSTITUTION = "cni"

TOC_ENTRY_RE = re.compile(
    r"^(?P<code>[IVX]+\s*-\s*\d+)\s+"
    r"(?P<title>.+?)"
    r"[·.]{3,}\s*"
    r"(?P<page>\d+)\s*$"
)
ARTICLE_RE = re.compile(
    r"(?m)^제\s*(?P<num>\d+)\s*조(?:\s*의\s*(?P<sub>\d+))?\s*(?:\((?P<title>[^)\n]+)\))?"
)
LAW_QUOTE_REF_RE = re.compile(
    r"「(?P<law>[^」]+)」\s*제\s*(?P<num>\d+)\s*조(?:\s*의\s*(?P<sub>\d+))?"
)
INTERNAL_RULE_REF_RE = re.compile(
    r"(?P<rule>[가-힣A-Za-z0-9·ㆍ․\s]+?(?:규정|규칙|정관|조례|강령))\s*"
    r"제\s*(?P<num>\d+)\s*조(?:\s*의\s*(?P<sub>\d+))?"
)
SAME_RULE_REF_RE = re.compile(r"(?<![가-힣A-Za-z0-9])제\s*(?P<num>\d+)\s*조(?:\s*의\s*(?P<sub>\d+))?")
DATE_RE = re.compile(r"(?P<y>19\d{2}|20\d{2})\.\s*(?P<m>\d{1,2})\s*\.\s*(?P<d>\d{1,2})\.?")
AMENDMENT_MARKER_RE = re.compile(
    r"(?:\[(?:전문\s*개정|개정)[^\]]*?(?P<bracket_date>19\d{2}|20\d{2})\.\s*\d{1,2}\s*\.\s*\d{1,2}\.?\]"
    r"|<개정[^>]*?(?P<angle_date>19\d{2}|20\d{2})\.\s*\d{1,2}\s*\.\s*\d{1,2}\.?>)"
)


@dataclasses.dataclass(frozen=True)
class TocEntry:
    code: str
    rule: str
    start_page: int
    end_page: int

    @property
    def slug(self) -> str:
        return slugify(self.rule)


@dataclasses.dataclass(frozen=True)
class Article:
    rule: TocEntry
    article: str
    title: str
    body: str
    amended: str
    refs: tuple[dict[str, str], ...]
    legal_basis: tuple[dict[str, str], ...]
    source_pages: tuple[int, int]

    @property
    def article_id(self) -> str:
        return f"{self.rule.slug}#{self.article}"

    @property
    def relative_path(self) -> Path:
        return Path("rules") / self.rule.slug / f"{self.article}.md"


@dataclasses.dataclass
class BuildResult:
    rules: list[TocEntry]
    articles: list[Article]
    no_article_rules: list[TocEntry]
    ambiguous_basis: list[Article]
    table_review: list[Article]
    skipped_duplicate_articles: list[Article]
    output_dir: Path


@dataclasses.dataclass(frozen=True)
class PageSpan:
    page_no: int
    start: int
    end: int


def project_root() -> Path:
    return Path(__file__).resolve().parents[4]


def default_pdf_path(root: Path) -> Path:
    pdfs = sorted((root / "00_src").glob("*.pdf"))
    if not pdfs:
        raise FileNotFoundError("no PDF files found under 00_src")
    return pdfs[0]


def normalize_spaces(text: str) -> str:
    text = unicodedata.normalize("NFC", text)
    text = text.replace("\u00a0", " ")
    text = text.replace("", '"')
    text = re.sub(r"[ \t]+", " ", text)
    return text


def slugify(rule_name: str) -> str:
    slug = normalize_spaces(rule_name)
    slug = re.sub(r"\s+", "", slug)
    slug = re.sub(r"[^\w가-힣·ㆍ․.-]+", "", slug)
    return slug or hashlib.sha1(rule_name.encode("utf-8")).hexdigest()[:12]


def article_key(num: str, sub: str | None) -> str:
    key = f"제{int(num)}조"
    if sub:
        key += f"의{int(sub)}"
    return key


def read_pages(doc: fitz.Document) -> dict[int, str]:
    return {
        page_no: normalize_spaces(doc.load_page(page_no - 1).get_text("text"))
        for page_no in range(1, doc.page_count + 1)
    }


def parse_toc(pages: dict[int, str], toc_pages: Iterable[int] = (3, 4, 5)) -> list[TocEntry]:
    raw: list[tuple[str, str, int]] = []
    for page_no in toc_pages:
        for line in pages.get(page_no, "").splitlines():
            line = normalize_spaces(line).strip()
            match = TOC_ENTRY_RE.match(line)
            if not match:
                continue
            raw.append(
                (
                    re.sub(r"\s+", "", match.group("code")),
                    normalize_rule_title(match.group("title")),
                    int(match.group("page")),
                )
            )

    entries: list[TocEntry] = []
    for index, (code, title, start_page) in enumerate(raw):
        next_start = raw[index + 1][2] if index + 1 < len(raw) else max(pages)
        entries.append(TocEntry(code=code, rule=title, start_page=start_page, end_page=max(start_page, next_start - 1)))
    return entries


def normalize_rule_title(title: str) -> str:
    title = normalize_spaces(title)
    title = title.replace("․", "·")
    title = re.sub(r"\s+", " ", title).strip()
    return title


def clean_page_text(text: str) -> str:
    cleaned: list[str] = []
    for line in text.splitlines():
        line = normalize_spaces(line).rstrip()
        stripped = line.strip()
        if re.fullmatch(r"-\s*\d+\s*-", stripped):
            continue
        if stripped == "충남연구원":
            continue
        cleaned.append(line)
    return "\n".join(cleaned).strip()


def rule_text(entry: TocEntry, pages: dict[int, str]) -> str:
    text, _ = rule_text_with_page_spans(entry, pages)
    return text


def rule_text_with_page_spans(entry: TocEntry, pages: dict[int, str]) -> tuple[str, list[PageSpan]]:
    parts: list[str] = []
    spans: list[PageSpan] = []
    cursor = 0
    for page_no in range(entry.start_page, entry.end_page + 1):
        part = clean_page_text(pages.get(page_no, ""))
        if not part:
            continue
        if parts:
            parts.append("\n")
            cursor += 1
        start = cursor
        parts.append(part)
        cursor += len(part)
        spans.append(PageSpan(page_no=page_no, start=start, end=cursor))
    text = "".join(parts)
    return re.sub(r"\n{3,}", "\n\n", text).strip(), spans


def parse_articles(
    entry: TocEntry,
    text: str,
    known_rule_names: set[str] | None = None,
    page_spans: list[PageSpan] | None = None,
) -> list[Article]:
    matches = list(ARTICLE_RE.finditer(text))
    articles: list[Article] = []
    if not matches:
        return articles

    prefix = text[: matches[0].start()]
    amended = latest_date(prefix) or SOURCE_EFFECTIVE_DATE

    article_page_offsets = map_article_pages(entry, text, matches, page_spans)
    seen_keys: set[str] = set()
    for idx, match in enumerate(matches):
        start = match.start()
        end = matches[idx + 1].start() if idx + 1 < len(matches) else len(text)
        body = text[start:end].strip()
        key = article_key(match.group("num"), match.group("sub"))
        if key in seen_keys:
            continue
        seen_keys.add(key)
        title = (match.group("title") or "").strip()
        refs = tuple(extract_refs(body, entry.slug, key, known_rule_names=known_rule_names))
        legal_basis = tuple(extract_legal_basis(body, is_basis_candidate=is_legal_basis_candidate(idx, title, body)))
        article_amended = extract_article_amended(body) or amended
        pages = article_page_offsets.get(key, (entry.start_page, entry.end_page))
        articles.append(
            Article(
                rule=entry,
                article=key,
                title=title,
                body=body,
                amended=article_amended,
                refs=refs,
                legal_basis=legal_basis,
                source_pages=pages,
            )
        )
    return articles


def map_article_pages(
    entry: TocEntry,
    text: str,
    matches: list[re.Match[str]],
    page_spans: list[PageSpan] | None = None,
) -> dict[str, tuple[int, int]]:
    if page_spans:
        return article_pages_from_spans(entry, text, matches, page_spans)
    return estimate_article_pages(entry, text, matches)


def article_pages_from_spans(
    entry: TocEntry,
    text: str,
    matches: list[re.Match[str]],
    page_spans: list[PageSpan],
) -> dict[str, tuple[int, int]]:
    result: dict[str, tuple[int, int]] = {}
    for idx, match in enumerate(matches):
        key = article_key(match.group("num"), match.group("sub"))
        start = match.start()
        end = matches[idx + 1].start() if idx + 1 < len(matches) else len(text)
        overlapping = [span.page_no for span in page_spans if span.start < end and span.end > start]
        if overlapping:
            result[key] = (min(overlapping), max(overlapping))
        else:
            result[key] = (entry.start_page, entry.end_page)
    return result


def estimate_article_pages(entry: TocEntry, text: str, matches: list[re.Match[str]]) -> dict[str, tuple[int, int]]:
    if len(matches) == 1:
        match = matches[0]
        return {article_key(match.group("num"), match.group("sub")): (entry.start_page, entry.end_page)}
    lines_by_page = max(1, text.count("\n") // max(1, entry.end_page - entry.start_page + 1))
    result: dict[str, tuple[int, int]] = {}
    for idx, match in enumerate(matches):
        key = article_key(match.group("num"), match.group("sub"))
        start_line = text[: match.start()].count("\n")
        end_pos = matches[idx + 1].start() if idx + 1 < len(matches) else len(text)
        end_line = text[:end_pos].count("\n")
        start_page = min(entry.end_page, entry.start_page + start_line // lines_by_page)
        end_page = min(entry.end_page, entry.start_page + max(start_line, end_line) // lines_by_page)
        result[key] = (start_page, max(start_page, end_page))
    return result


def latest_date(text: str) -> str | None:
    dates: list[dt.date] = []
    for match in DATE_RE.finditer(text):
        try:
            dates.append(dt.date(int(match.group("y")), int(match.group("m")), int(match.group("d"))))
        except ValueError:
            continue
    if not dates:
        return None
    return max(dates).isoformat()


def extract_article_amended(article_text: str) -> str | None:
    marker_texts: list[str] = []
    marker_source = re.sub(r"\s*\n\s*", " ", article_text)
    for match in AMENDMENT_MARKER_RE.finditer(marker_source):
        marker_texts.append(match.group(0))
    return latest_date("\n".join(marker_texts))


def ref_type(window: str) -> str:
    if "준용" in window:
        return "준용"
    if "위임" in window:
        return "위임"
    if "단서" in window or "예외" in window:
        return "단서예외"
    return "인용"


def extract_refs(
    text: str,
    current_slug: str,
    current_article: str,
    *,
    known_rule_names: set[str] | None = None,
) -> list[dict[str, str]]:
    refs: list[dict[str, str]] = []
    seen: set[tuple[str, str]] = set()
    consumed_spans: list[tuple[int, int]] = []

    def add(target: str, kind: str) -> None:
        key = (target, kind)
        if target == f"{current_slug}#{current_article}" or key in seen:
            return
        seen.add(key)
        refs.append({"target": target, "type": kind})

    for match in LAW_QUOTE_REF_RE.finditer(text):
        consumed_spans.append(match.span())
        window = text[match.start() : match.end() + 24]
        target = f"{slugify(match.group('law'))}#{article_key(match.group('num'), match.group('sub'))}"
        add(target, ref_type(window))

    for match in INTERNAL_RULE_REF_RE.finditer(text):
        consumed_spans.append(match.span())
        rule_name = normalize_rule_title(match.group("rule"))
        rule_name = clean_internal_rule_name(rule_name, known_rule_names)
        if rule_name.startswith("제"):
            continue
        window = text[match.start() : match.end() + 24]
        add(f"{slugify(rule_name)}#{article_key(match.group('num'), match.group('sub'))}", ref_type(window))

    for match in SAME_RULE_REF_RE.finditer(text):
        if any(start <= match.start() and match.end() <= end for start, end in consumed_spans):
            continue
        window = text[max(0, match.start() - 20) : match.end() + 20]
        if "제" in window and "항" in window[window.find("제") : match.start() - max(0, match.start() - 20) + 1]:
            continue
        add(f"{current_slug}#{article_key(match.group('num'), match.group('sub'))}", ref_type(window))

    return refs


def clean_internal_rule_name(rule_name: str, known_rule_names: set[str] | None = None) -> str:
    rule_name = re.sub(r"^(및|또는)\s+", "", rule_name).strip()
    if known_rule_names:
        candidate_slug = slugify(rule_name)
        suffix_matches = [
            known_name
            for known_name in known_rule_names
            if candidate_slug.endswith(slugify(known_name))
        ]
        if suffix_matches:
            return max(suffix_matches, key=lambda name: len(slugify(name)))
    if " " in rule_name:
        tail = rule_name.rsplit(" ", 1)[-1]
        if re.search(r"(규정|규칙|정관|조례|강령)$", tail):
            return tail
    return rule_name


def is_legal_basis_candidate(article_index: int, title: str, article_text: str) -> bool:
    if article_index == 0 and ("목적" in title or "설치" in title or "근거" in article_text[:500]):
        return True
    return "설치" in title or "설립" in title or "근거" in title


def extract_legal_basis(article_text: str, *, is_basis_candidate: bool) -> list[dict[str, str]]:
    if not is_basis_candidate:
        return []
    basis: list[dict[str, str]] = []
    seen: set[tuple[str, str]] = set()
    first_paragraph = article_text[:1200]
    if not any(token in first_paragraph for token in ("근거", "의거", "따라", "위임", "법", "조례", "정관", "설립", "설치")):
        return []
    for match in LAW_QUOTE_REF_RE.finditer(first_paragraph):
        key = (match.group("law"), article_key(match.group("num"), match.group("sub")))
        if key in seen:
            continue
        seen.add(key)
        basis.append({"law": match.group("law").strip(), "article": key[1]})
    return basis


def looks_table_sensitive(article: Article) -> bool:
    body = article.body
    if any(marker in body for marker in ("[별표", "<별표", "별 표", "[별지", "<별지", "No.")):
        return True
    short_lines = sum(1 for line in body.splitlines() if 0 < len(line.strip()) <= 4)
    return short_lines >= 10


def yaml_scalar(value: str | None) -> str:
    if value is None:
        return "null"
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def render_markdown(article: Article) -> str:
    lines = [
        "---",
        f"institution: {INSTITUTION}",
        f"rule: {yaml_scalar(article.rule.rule)}",
        f"article: {yaml_scalar(article.article)}",
        f"title: {yaml_scalar(article.title)}",
        f"effective: {SOURCE_EFFECTIVE_DATE}",
        f"amended: {article.amended}",
        "status: active",
        f"source_pages: [{article.source_pages[0]}, {article.source_pages[1]}]",
        "supersedes: null",
        "legal_basis:",
    ]
    if article.legal_basis:
        for basis in article.legal_basis:
            lines.extend(
                [
                    f"  - law: {yaml_scalar(basis['law'])}",
                    f"    article: {yaml_scalar(basis['article'])}",
                    '    mst: ""',
                ]
            )
    else:
        lines.append("  []")
    lines.append("refs:")
    if article.refs:
        for ref in article.refs:
            lines.extend(
                [
                    f"  - target: {yaml_scalar(ref['target'])}",
                    f"    type: {yaml_scalar(ref['type'])}",
                ]
            )
    else:
        lines.append("  []")
    lines.extend(["---", article.body.strip(), ""])
    return "\n".join(lines)


def build(pdf_path: Path, output_dir: Path) -> BuildResult:
    doc = fitz.open(pdf_path)
    pages = read_pages(doc)
    rules = parse_toc(pages)
    articles: list[Article] = []
    no_article_rules: list[TocEntry] = []
    known_rule_names = {entry.rule for entry in rules}
    for entry in rules:
        text, page_spans = rule_text_with_page_spans(entry, pages)
        parsed = parse_articles(entry, text, known_rule_names=known_rule_names, page_spans=page_spans)
        if not parsed:
            no_article_rules.append(entry)
        articles.extend(parsed)

    skipped_duplicate_articles = find_skipped_duplicate_articles(rules, pages, articles)
    ambiguous_basis = [article for article in articles if article.article == "제1조" and not article.legal_basis]
    table_review = [article for article in articles if looks_table_sensitive(article)]

    rules_dir = output_dir / "rules"
    rules_dir.mkdir(parents=True, exist_ok=True)
    for article in articles:
        dest = output_dir / article.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(render_markdown(article), encoding="utf-8")

    result = BuildResult(
        rules=rules,
        articles=articles,
        no_article_rules=no_article_rules,
        ambiguous_basis=ambiguous_basis,
        table_review=table_review,
        skipped_duplicate_articles=skipped_duplicate_articles,
        output_dir=output_dir,
    )
    (output_dir / "qa-report.md").write_text(render_qa_report(result, pdf_path), encoding="utf-8")
    return result


def find_skipped_duplicate_articles(
    rules: list[TocEntry], pages: dict[int, str], emitted_articles: list[Article]
) -> list[Article]:
    emitted = {(article.rule.code, article.article) for article in emitted_articles}
    skipped: list[Article] = []
    for entry in rules:
        text, page_spans = rule_text_with_page_spans(entry, pages)
        seen: set[str] = set()
        matches = list(ARTICLE_RE.finditer(text))
        page_offsets = map_article_pages(entry, text, matches, page_spans)
        amended = latest_date(text[: matches[0].start()]) if matches else None
        for idx, match in enumerate(matches):
            key = article_key(match.group("num"), match.group("sub"))
            if key not in seen:
                seen.add(key)
                continue
            start = match.start()
            end = matches[idx + 1].start() if idx + 1 < len(matches) else len(text)
            title = (match.group("title") or "").strip()
            skipped.append(
                Article(
                    rule=entry,
                    article=key,
                    title=title,
                    body=text[start:end].strip(),
                    amended=amended or SOURCE_EFFECTIVE_DATE,
                    refs=(),
                    legal_basis=(),
                    source_pages=page_offsets.get(key, (entry.start_page, entry.end_page)),
                )
            )
    return [article for article in skipped if (article.rule.code, article.article) in emitted]


def render_qa_report(result: BuildResult, pdf_path: Path) -> str:
    rule_counts = Counter(article.rule.rule for article in result.articles)
    source_pages_covered = sum(1 for article in result.articles if article.source_pages)
    article_level_pages = sum(
        1
        for article in result.articles
        if (article.source_pages[0], article.source_pages[1]) != (article.rule.start_page, article.rule.end_page)
    )
    amended_extracted = sum(1 for article in result.articles if extract_article_amended(article.body))
    legal_basis_filled = sum(1 for article in result.articles if article.legal_basis)
    lines = [
        "# CNI rule Markdown build QA report",
        "",
        f"- Source PDF: `{pdf_path}`",
        f"- Source effective date: `{SOURCE_EFFECTIVE_DATE}`",
        f"- Rule units from TOC: {len(result.rules)}",
        f"- Article Markdown files emitted: {len(result.articles)}",
        f"- Rules without parsed articles: {len(result.no_article_rules)}",
        f"- Skipped duplicate article headings: {len(result.skipped_duplicate_articles)}",
        f"- First articles without extracted legal_basis: {len(result.ambiguous_basis)}",
        f"- Table/annex layout review candidates: {len(result.table_review)}",
        f"- source_pages coverage: {source_pages_covered}/{len(result.articles)}",
        f"- source_pages narrowed below rule range: {article_level_pages}",
        f"- amended markers extracted: {amended_extracted}",
        f"- legal_basis filled: {legal_basis_filled}",
        "",
        "## Rule article counts",
        "",
        "| rule | slug | articles | pages |",
        "| --- | --- | ---: | --- |",
    ]
    for entry in result.rules:
        lines.append(f"| {entry.rule} | `{entry.slug}` | {rule_counts[entry.rule]} | {entry.start_page}-{entry.end_page} |")

    lines.extend(["", "## Parsing failures", ""])
    if result.no_article_rules:
        lines.extend(["| rule | pages | reason |", "| --- | --- | --- |"])
        for entry in result.no_article_rules:
            lines.append(f"| {entry.rule} | {entry.start_page}-{entry.end_page} | no `제N조` heading detected |")
    else:
        lines.append("- None.")

    lines.extend(["", "## Legal basis 미확정 목록", ""])
    if result.ambiguous_basis:
        lines.extend(["| article_id | reason |", "| --- | --- |"])
        for article in result.ambiguous_basis[:200]:
            lines.append(f"| `{article.article_id}` | 미확정: 제1조에서 명시적 `「법령명」 제N조` 근거를 추출하지 못함 |")
        if len(result.ambiguous_basis) > 200:
            lines.append(f"| ... | {len(result.ambiguous_basis) - 200} more omitted |")
    else:
        lines.append("- None.")

    lines.extend(["", "## Table / annex layout review candidates", ""])
    if result.table_review:
        lines.extend(["| article_id | source pages | reason |", "| --- | --- | --- |"])
        for article in result.table_review[:200]:
            lines.append(
                f"| `{article.article_id}` | {article.source_pages[0]}-{article.source_pages[1]} | plain-text PDF extraction may flatten table or annex layout |"
            )
        if len(result.table_review) > 200:
            lines.append(f"| ... | {len(result.table_review) - 200} more omitted |")
    else:
        lines.append("- None.")

    lines.extend(["", "## Skipped duplicate article headings", ""])
    if result.skipped_duplicate_articles:
        lines.extend(["| article_id | source pages | reason |", "| --- | --- | --- |"])
        for article in result.skipped_duplicate_articles[:200]:
            lines.append(
                f"| `{article.article_id}` | {article.source_pages[0]}-{article.source_pages[1]} | duplicate article key, commonly from 부칙; not emitted to avoid overwriting main article file |"
            )
        if len(result.skipped_duplicate_articles) > 200:
            lines.append(f"| ... | {len(result.skipped_duplicate_articles) - 200} more omitted |")
    else:
        lines.append("- None.")

    return "\n".join(lines) + "\n"


def parse_args(argv: list[str]) -> argparse.Namespace:
    root = project_root()
    parser = argparse.ArgumentParser(description="Build CNI rule Markdown files from the PDF rule book.")
    parser.add_argument("--pdf", type=Path, default=default_pdf_path(root))
    parser.add_argument("--output", type=Path, default=root / "04_data" / "90_index-build")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    result = build(args.pdf.resolve(), args.output.resolve())
    print(f"source_pdf={args.pdf}")
    print(f"output_dir={result.output_dir}")
    print(f"rules={len(result.rules)}")
    print(f"articles={len(result.articles)}")
    print(f"no_article_rules={len(result.no_article_rules)}")
    print(f"legal_basis_review={len(result.ambiguous_basis)}")
    print(f"table_review={len(result.table_review)}")
    print(f"skipped_duplicate_articles={len(result.skipped_duplicate_articles)}")
    print(f"qa_report={result.output_dir / 'qa-report.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
