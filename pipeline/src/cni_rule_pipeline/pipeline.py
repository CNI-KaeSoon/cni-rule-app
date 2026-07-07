from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
import os
import re
import sys
import unicodedata
from collections import Counter
from pathlib import Path
from typing import Iterable

import fitz

SOURCE_EFFECTIVE_DATE = "2026-02-27"
INSTITUTION = "cni"
INSTITUTION_NAME = "충남연구원"

TOC_CNI_RE = re.compile(
    r"^(?P<code>[IVX]+\s*-\s*\d+)\s+"
    r"(?P<title>.+?)"
    r"[·.]{3,}\s*"
    r"(?P<page>\d+)\s*$"
)
TOC_DOTTED_NO_CODE_RE = re.compile(
    r"^(?P<title>.+?)\s*[·.․…]{3,}\s*(?P<page>\d+)\s*$"
)
TOC_NUMBERED_DOTTED_RE = re.compile(
    r"^(?P<num>\d+)\.\s*(?P<title>.+?)\s*[·.․…]{3,}\s*(?P<page>\d+)\s*$"
)
TOC_SLASH_RE = re.compile(
    r"^(?P<num>\d+)\.\s*(?P<title>.+?)\s*/\s*(?P<page>\d+)\s*$"
)
ARTICLE_RE = re.compile(
    r"(?m)^제\s*(?P<num>\d+)\s*조(?:\s*의\s*(?P<sub>\d+))?\s*(?:\((?P<title>[^)\n]+)\))?"
)
ANNEX_HEADING_RE = re.compile(
    r"^\s*[\[\(<【〔]?\s*"
    r"(?P<label>"
    r"별\s*표\s*(?:제\s*)?(?P<table_no>\d+)(?:\s*호)?"
    r"|별\s*지\s*(?:제\s*)?(?P<form_no>\d+)\s*호\s*서\s*식"
    r")"
    r"\s*[\]\)>】〕]?"
    r"\s*(?P<title>[^\n]*)$"
)


@dataclasses.dataclass(frozen=True)
class BuildConfig:
    institution: str = INSTITUTION
    institution_name: str = INSTITUTION_NAME
    effective_date: str = SOURCE_EFFECTIVE_DATE
    footers: tuple[str, ...] = (INSTITUTION_NAME,)
    legacy_cni_report: bool = False
    source_url: str | None = None


@dataclasses.dataclass(frozen=True)
class TocProfile:
    name: str
    pattern: re.Pattern[str]
    code_group: str | None = None
    skip_numbered_chapters: bool = False


@dataclasses.dataclass(frozen=True)
class TocCandidate:
    code: str | None
    title: str
    start_page: int


@dataclasses.dataclass(frozen=True)
class TocParseResult:
    entries: list["TocEntry"]
    profile: str
    match_count: int
    toc_pages: tuple[int, ...]


TOC_PROFILES = (
    TocProfile("cni-roman-dotted", TOC_CNI_RE, code_group="code"),
    TocProfile("dotted-no-code", TOC_DOTTED_NO_CODE_RE, skip_numbered_chapters=True),
    TocProfile("numbered-dotted", TOC_NUMBERED_DOTTED_RE),
    TocProfile("numbered-slash", TOC_SLASH_RE),
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


@dataclasses.dataclass(frozen=True)
class Annex:
    rule: TocEntry
    annex: str
    title: str
    body: str
    source_pages: tuple[int, int]
    table_structured: bool
    structured_tables: tuple[str, ...] = ()

    @property
    def annex_id(self) -> str:
        return f"{self.rule.slug}#{self.annex}"

    @property
    def relative_path(self) -> Path:
        return Path("rules") / self.rule.slug / f"{self.annex}.md"


@dataclasses.dataclass(frozen=True)
class CoverageReport:
    eligible_pages: tuple[int, ...]
    covered_pages: tuple[int, ...]
    uncovered_pages: tuple[tuple[int, int], ...]
    percent: float


@dataclasses.dataclass(frozen=True)
class SuspiciousRule:
    rule: TocEntry
    article_count: int
    annex_count: int
    page_count: int
    reason: str


@dataclasses.dataclass
class BuildResult:
    rules: list[TocEntry]
    articles: list[Article]
    annexes: list[Annex]
    no_article_rules: list[TocEntry]
    ambiguous_basis: list[Article]
    table_review: list[Article]
    skipped_duplicate_articles: list[Article]
    coverage: CoverageReport
    suspicious_rules: list[SuspiciousRule]
    output_dir: Path
    toc_profile: str
    toc_match_count: int
    toc_pages: tuple[int, ...]
    config: BuildConfig


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
    return slug or hashlib.sha256(rule_name.encode("utf-8")).hexdigest()[:12]


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


def write_page_sidecars(pages: dict[int, str], output_dir: Path) -> None:
    pages_dir = output_dir / "pages"
    pages_dir.mkdir(parents=True, exist_ok=True)
    for page_no, text in sorted(pages.items()):
        (pages_dir / f"{page_no:04d}.txt").write_text(text, encoding="utf-8")


def move_existing_aside(path: Path) -> None:
    if not path.exists():
        return
    backup = path.with_name(f"{path.name}.previous-{os.getpid()}")
    if backup.exists():
        raise FileExistsError(f"backup path already exists: {backup}")
    path.rename(backup)


def prepare_output_tree(output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    move_existing_aside(output_dir / "rules")
    move_existing_aside(output_dir / "pages")


def parse_toc(pages: dict[int, str], toc_pages: Iterable[int] = (3, 4, 5)) -> list[TocEntry]:
    return parse_toc_with_profile(pages, toc_pages=tuple(toc_pages)).entries


def parse_toc_with_profile(pages: dict[int, str], toc_pages: Iterable[int] | None = None) -> TocParseResult:
    selected_pages = tuple(toc_pages) if toc_pages is not None else auto_detect_toc_pages(pages)
    profile, raw = best_toc_profile(pages, selected_pages)
    entries = toc_entries_from_candidates(raw, profile.name, max(pages), printed_page_map(pages))
    return TocParseResult(entries=entries, profile=profile.name, match_count=len(raw), toc_pages=selected_pages)


def auto_detect_toc_pages(pages: dict[int, str], scan_pages: int = 30) -> tuple[int, ...]:
    candidates: list[tuple[int, int, int]] = []
    max_page = min(max(pages), scan_pages)
    for start in range(1, max_page + 1):
        for end in range(start, max_page + 1):
            page_range = tuple(range(start, end + 1))
            _profile, raw = best_toc_profile(pages, page_range)
            if raw:
                candidates.append((len(raw), -(end - start + 1), start))
    if not candidates:
        return (3, 4, 5)
    _count, neg_len, start = max(candidates)
    return tuple(range(start, start - neg_len))


def best_toc_profile(pages: dict[int, str], toc_pages: Iterable[int]) -> tuple[TocProfile, list[TocCandidate]]:
    page_tuple = tuple(toc_pages)
    scored: list[tuple[int, int, TocProfile, list[TocCandidate]]] = []
    for order, profile in enumerate(TOC_PROFILES):
        raw = collect_toc_candidates(pages, page_tuple, profile)
        scored.append((len(raw), -order, profile, raw))
    _count, _order, profile, raw = max(scored, key=lambda item: (item[0], item[1]))
    return profile, raw


def collect_toc_candidates(pages: dict[int, str], toc_pages: Iterable[int], profile: TocProfile) -> list[TocCandidate]:
    raw: list[TocCandidate] = []
    for page_no in toc_pages:
        for line in pages.get(page_no, "").splitlines():
            line = normalize_spaces(line).strip()
            match = profile.pattern.match(line)
            if not match:
                continue
            title = normalize_rule_title(match.group("title"))
            if should_skip_toc_title(title, profile):
                continue
            code = None
            if profile.code_group:
                code = re.sub(r"\s+", "", match.group(profile.code_group))
            raw.append(TocCandidate(code=code, title=title, start_page=int(match.group("page"))))
    return raw


def should_skip_toc_title(title: str, profile: TocProfile) -> bool:
    if profile.skip_numbered_chapters and re.match(r"^제\s*\d+\s*[장절]\b", title):
        return True
    if profile.skip_numbered_chapters and re.match(r"^\d+\.", title):
        return True
    if not re.search(r"(규정|규칙|정관|조례|강령|지침|법|법률|시행령|요령|발췌)$", title):
        return True
    return False


def toc_entries_from_candidates(
    raw: list[TocCandidate],
    profile_name: str,
    max_page: int,
    page_map: dict[int, int] | None = None,
) -> list[TocEntry]:
    entries: list[TocEntry] = []
    page_map = page_map or {}
    for index, candidate in enumerate(raw):
        next_start_label = raw[index + 1].start_page if index + 1 < len(raw) else None
        start_page = page_map.get(candidate.start_page, candidate.start_page)
        if next_start_label is not None:
            next_start_page = page_map.get(next_start_label, next_start_label)
            end_page = max(start_page, next_start_page - 1)
        else:
            end_page = max_page
        code = candidate.code or f"{profile_name}-{index + 1:03d}"
        entries.append(
            TocEntry(
                code=code,
                rule=candidate.title,
                start_page=start_page,
                end_page=end_page,
            )
        )
    return entries


def printed_page_map(pages: dict[int, str]) -> dict[int, int]:
    mapped: dict[int, int] = {}
    for physical_page, text in pages.items():
        printed = extract_printed_page_number(text)
        if printed is None:
            continue
        mapped.setdefault(printed, physical_page)
    return mapped


def extract_printed_page_number(text: str) -> int | None:
    lines = [normalize_spaces(line).strip() for line in text.splitlines() if line.strip()]
    for line in [*lines[:5], *lines[-5:]]:
        match = re.fullmatch(r"-\s*(?P<num>\d{1,4})\s*-", line)
        if match:
            return int(match.group("num"))
        if re.fullmatch(r"\d{1,4}", line):
            return int(line)
    return None


def normalize_rule_title(title: str) -> str:
    title = normalize_spaces(title)
    title = title.replace("․", "·")
    title = re.sub(r"\s+", " ", title).strip()
    return title


def clean_page_text(text: str, footers: Iterable[str] = (INSTITUTION_NAME,)) -> str:
    footer_set = {normalize_spaces(footer).strip() for footer in footers if footer.strip()}
    cleaned: list[str] = []
    for line in text.splitlines():
        line = normalize_spaces(line).rstrip()
        stripped = line.strip()
        if re.fullmatch(r"-\s*\d+\s*-", stripped):
            continue
        if stripped in footer_set:
            continue
        cleaned.append(line)
    return "\n".join(cleaned).strip()


def rule_text(entry: TocEntry, pages: dict[int, str], footers: Iterable[str] = (INSTITUTION_NAME,)) -> str:
    text, _ = rule_text_with_page_spans(entry, pages, footers=footers)
    return text


def rule_text_with_page_spans(
    entry: TocEntry,
    pages: dict[int, str],
    footers: Iterable[str] = (INSTITUTION_NAME,),
) -> tuple[str, list[PageSpan]]:
    parts: list[str] = []
    spans: list[PageSpan] = []
    cursor = 0
    for page_no in range(entry.start_page, entry.end_page + 1):
        part = clean_page_text(pages.get(page_no, ""), footers=footers)
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
    text = re.sub(r"\n{3,}", "\n\n", text).strip()
    return trim_text_to_rule_heading(entry, text, spans)


def trim_text_to_rule_heading(entry: TocEntry, text: str, spans: list[PageSpan]) -> tuple[str, list[PageSpan]]:
    offset = find_rule_heading_offset(entry.rule, text)
    if offset <= 0:
        return text, spans
    trimmed = text[offset:].lstrip()
    whitespace_shift = len(text[offset:]) - len(trimmed)
    total_offset = offset + whitespace_shift
    return trimmed, shift_page_spans(spans, total_offset, len(trimmed))


def find_rule_heading_offset(rule_name: str, text: str) -> int:
    target = comparable_heading(rule_name)
    if not target:
        return 0
    cursor = 0
    for line in text.splitlines(keepends=True):
        stripped = normalize_rule_title(line.strip())
        comparable = comparable_heading(stripped)
        if comparable and (comparable == target or comparable.endswith(target)):
            return cursor + line.find(line.strip())
        cursor += len(line)
    return 0


def comparable_heading(value: str) -> str:
    value = normalize_rule_title(value)
    value = re.sub(r"^[\dIVX]+\s*[-.]\s*", "", value)
    value = re.sub(r"^(?:제\s*)?\d+\s*[편장절]\s*", "", value)
    return re.sub(r"[\s·ㆍ․]+", "", value)


def shift_page_spans(spans: list[PageSpan], offset: int, text_len: int) -> list[PageSpan]:
    shifted: list[PageSpan] = []
    for span in spans:
        if span.end <= offset:
            continue
        start = max(0, span.start - offset)
        end = min(text_len, span.end - offset)
        if end > start:
            shifted.append(PageSpan(page_no=span.page_no, start=start, end=end))
    return shifted


def parse_articles(
    entry: TocEntry,
    text: str,
    known_rule_names: set[str] | None = None,
    page_spans: list[PageSpan] | None = None,
    effective_date: str = SOURCE_EFFECTIVE_DATE,
) -> list[Article]:
    raw_matches = list(ARTICLE_RE.finditer(text))
    annex_spans = annex_block_spans(text, raw_matches)
    matches = [match for match in raw_matches if not position_in_spans(match.start(), annex_spans)]
    articles: list[Article] = []
    if not matches:
        return articles

    prefix = text_without_spans(text[: matches[0].start()], [(start, end) for start, end in annex_spans if end <= matches[0].start()])
    amended = latest_date(prefix) or effective_date

    article_page_offsets = map_article_pages(entry, text, matches, page_spans)
    seen_keys: set[str] = set()
    for idx, match in enumerate(matches):
        start = match.start()
        next_article = matches[idx + 1].start() if idx + 1 < len(matches) else len(text)
        next_annex = next((span_start for span_start, _span_end in annex_spans if span_start > start), len(text))
        end = min(next_article, next_annex)
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


def split_annex_text(text: str) -> tuple[str, str, int]:
    raw_articles = list(ARTICLE_RE.finditer(text))
    spans = annex_block_spans(text, raw_articles)
    if not spans:
        return text, "", len(text)
    article_text = text_without_spans(text, spans).strip()
    annex_text = "\n".join(text[start:end].strip() for start, end in spans if include_annex_span(start, raw_articles)).strip()
    return article_text, annex_text, spans[0][0]


def iter_annex_heading_matches(text: str) -> Iterable[re.Match[str]]:
    for match in re.finditer(r"(?m)^.*$", text):
        line = match.group(0)
        heading = ANNEX_HEADING_RE.match(line)
        title_tail = normalize_spaces(heading.group("title") or "").lstrip() if heading else ""
        if heading and not is_inline_annex_reference_tail(title_tail):
            yield match


def is_inline_annex_reference_tail(title_tail: str) -> bool:
    return bool(re.match(r"^(?:에|의)(?:\s|한|하여|해|의|를|을|로|으로|$)", title_tail))


def annex_block_spans(text: str, article_matches: list[re.Match[str]] | None = None) -> list[tuple[int, int]]:
    annex_matches = list(iter_annex_heading_matches(text))
    if not annex_matches:
        return []
    article_matches = article_matches if article_matches is not None else list(ARTICLE_RE.finditer(text))
    first_article = article_matches[0].start() if article_matches else None
    annex_starts = [match.start() for match in annex_matches]
    spans: list[tuple[int, int]] = []
    for match in annex_matches:
        following_annexes = [start for start in annex_starts if start > match.start()]
        if first_article is not None and match.start() < first_article:
            following_articles = [article.start() for article in article_matches if article.start() > match.start()]
            end = following_articles[0] if following_articles else (following_annexes[0] if following_annexes else len(text))
        else:
            end = following_annexes[0] if following_annexes else len(text)
        spans.append((match.start(), end))
    return spans


def include_annex_span(start: int, article_matches: list[re.Match[str]]) -> bool:
    if not article_matches:
        return True
    return start > article_matches[0].start()


def position_in_spans(position: int, spans: list[tuple[int, int]]) -> bool:
    return any(start <= position < end for start, end in spans)


def text_without_spans(text: str, spans: list[tuple[int, int]]) -> str:
    if not spans:
        return text
    parts: list[str] = []
    cursor = 0
    for start, end in sorted(spans):
        parts.append(text[cursor:start])
        cursor = max(cursor, end)
    parts.append(text[cursor:])
    return "".join(parts)


def normalize_annex_key(match: re.Match[str]) -> str:
    heading = ANNEX_HEADING_RE.match(match.group(0).strip())
    if heading is None:
        raise ValueError("annex heading match did not match ANNEX_HEADING_RE")
    if heading.group("table_no"):
        return f"별표{int(heading.group('table_no'))}"
    return f"별지제{int(heading.group('form_no'))}호"


def annex_title(match: re.Match[str]) -> str:
    heading = ANNEX_HEADING_RE.match(match.group(0).strip())
    if heading is None:
        return ""
    title = normalize_spaces(heading.group("title") or "").strip()
    return title.strip(" -:：")


def parse_annexes(
    entry: TocEntry,
    text: str,
    page_spans: list[PageSpan],
    doc: fitz.Document | None = None,
    offset: int = 0,
) -> list[Annex]:
    raw_articles = list(ARTICLE_RE.finditer(text))
    spans = annex_block_spans(text, raw_articles)
    matches = [match for match in iter_annex_heading_matches(text) if include_annex_span(match.start(), raw_articles)]
    span_by_start = {start: end for start, end in spans}
    annexes: list[Annex] = []
    seen: Counter[str] = Counter()
    for match in matches:
        start = match.start()
        end = span_by_start.get(start, len(text))
        body = text[start:end].strip()
        key = normalize_annex_key(match)
        seen[key] += 1
        if seen[key] > 1:
            key = f"{key}-{seen[key]}"
        pages = pages_for_text_span(entry, offset + start, offset + end, page_spans)
        structured_tables = extract_tables_markdown(doc, pages) if doc is not None else ()
        annexes.append(
            Annex(
                rule=entry,
                annex=key,
                title=annex_title(match),
                body=body,
                source_pages=pages,
                table_structured=bool(structured_tables),
                structured_tables=structured_tables,
            )
        )
    return annexes


def pages_for_text_span(
    entry: TocEntry,
    start: int,
    end: int,
    page_spans: list[PageSpan],
) -> tuple[int, int]:
    overlapping = [span.page_no for span in page_spans if span.start < end and span.end > start]
    if overlapping:
        return (min(overlapping), max(overlapping))
    return (entry.start_page, entry.end_page)


def extract_tables_markdown(doc: fitz.Document | None, pages: tuple[int, int]) -> tuple[str, ...]:
    if doc is None:
        return ()
    rendered: list[str] = []
    for page_no in range(pages[0], pages[1] + 1):
        try:
            page = doc.load_page(page_no - 1)
            finder = getattr(page, "find_tables", None)
            if finder is None:
                continue
            tables = finder()
            for table in getattr(tables, "tables", []):
                markdown = table_to_markdown(table.extract())
                if markdown:
                    rendered.append(markdown)
        except Exception:
            continue
    return tuple(rendered)


def table_to_markdown(rows: list[list[object]]) -> str:
    clean_rows = [
        [normalize_spaces("" if cell is None else str(cell)).replace("\n", "<br>").strip() for cell in row]
        for row in rows
        if any(str(cell or "").strip() for cell in row)
    ]
    if not clean_rows:
        return ""
    width = max(len(row) for row in clean_rows)
    clean_rows = [row + [""] * (width - len(row)) for row in clean_rows]
    header = clean_rows[0]
    lines = [
        "| " + " | ".join(escape_md_cell(cell) for cell in header) + " |",
        "| " + " | ".join("---" for _ in header) + " |",
    ]
    for row in clean_rows[1:]:
        lines.append("| " + " | ".join(escape_md_cell(cell) for cell in row) + " |")
    return "\n".join(lines)


def escape_md_cell(value: str) -> str:
    return value.replace("|", "\\|")


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

    for ref in extract_annex_refs(text, current_slug):
        add(ref["target"], ref["type"])

    return refs


def extract_annex_refs(text: str, current_slug: str) -> list[dict[str, str]]:
    refs: list[dict[str, str]] = []
    seen: set[str] = set()
    patterns = (
        re.compile(r"별\s*표\s*(?:제\s*)?(?P<num>\d+)(?:\s*호)?"),
        re.compile(r"별\s*지\s*(?:제\s*)?(?P<num>\d+)\s*호(?:\s*서\s*식)?"),
    )
    for pattern in patterns:
        for match in pattern.finditer(text):
            if pattern.pattern.startswith("별\\s*표"):
                target = f"{current_slug}#별표{int(match.group('num'))}"
            else:
                target = f"{current_slug}#별지제{int(match.group('num'))}호"
            if target in seen:
                continue
            seen.add(target)
            refs.append({"target": target, "type": "인용"})
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


def format_page_range(pages: Iterable[int]) -> str:
    page_tuple = tuple(pages)
    if not page_tuple:
        return ""
    if page_tuple == tuple(range(page_tuple[0], page_tuple[-1] + 1)):
        return f"{page_tuple[0]}-{page_tuple[-1]}"
    return ",".join(str(page) for page in page_tuple)


def parse_page_range(value: str) -> tuple[int, ...]:
    match = re.fullmatch(r"\s*(\d+)\s*-\s*(\d+)\s*", value)
    if not match:
        raise argparse.ArgumentTypeError("--toc-pages must use START-END")
    start = int(match.group(1))
    end = int(match.group(2))
    if start < 1 or end < start:
        raise argparse.ArgumentTypeError("--toc-pages must have 1 <= START <= END")
    return tuple(range(start, end + 1))


def parse_effective_date(value: str) -> str:
    try:
        return dt.date.fromisoformat(value).isoformat()
    except ValueError as exc:
        raise argparse.ArgumentTypeError("--effective-date must use YYYY-MM-DD") from exc


def render_markdown(article: Article, config: BuildConfig = BuildConfig()) -> str:
    lines = [
        "---",
        f"institution: {config.institution}",
        f"rule: {yaml_scalar(article.rule.rule)}",
        f"article: {yaml_scalar(article.article)}",
        f"title: {yaml_scalar(article.title)}",
        f"effective: {config.effective_date}",
        f"amended: {article.amended}",
        "status: active",
        f"source_pages: [{article.source_pages[0]}, {article.source_pages[1]}]",
        f"pages: [{article.source_pages[0]}, {article.source_pages[1]}]",
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


def render_annex_markdown(annex: Annex, config: BuildConfig = BuildConfig()) -> str:
    lines = [
        "---",
        "type: annex",
        f"institution: {config.institution}",
        f"rule: {yaml_scalar(annex.rule.rule)}",
        f"annex: {yaml_scalar(annex.annex)}",
        f"title: {yaml_scalar(annex.title)}",
        f"effective: {config.effective_date}",
        "status: active",
        f"source_pages: [{annex.source_pages[0]}, {annex.source_pages[1]}]",
        f"pages: [{annex.source_pages[0]}, {annex.source_pages[1]}]",
        f"table_structured: {str(annex.table_structured).lower()}",
        "---",
        annex.body.strip(),
    ]
    if annex.structured_tables:
        lines.extend(["", "## Extracted tables", ""])
        for idx, table in enumerate(annex.structured_tables, start=1):
            if len(annex.structured_tables) > 1:
                lines.extend([f"### Table {idx}", ""])
            lines.extend([table, ""])
    else:
        lines.append("")
    return "\n".join(lines)


def build(
    pdf_path: Path,
    output_dir: Path,
    config: BuildConfig = BuildConfig(),
    toc_pages: Iterable[int] | None = (3, 4, 5),
) -> BuildResult:
    doc = fitz.open(pdf_path)
    pages = read_pages(doc)
    prepare_output_tree(output_dir)
    write_page_sidecars(pages, output_dir)
    toc_result = parse_toc_with_profile(pages, toc_pages=toc_pages)
    rules = toc_result.entries
    if not rules:
        raise ValueError(
            f"TOC parsing found 0 rules: profile={toc_result.profile} "
            f"matches={toc_result.match_count} toc_pages={format_page_range(toc_result.toc_pages)}"
        )
    articles: list[Article] = []
    annexes: list[Annex] = []
    no_article_rules: list[TocEntry] = []
    known_rule_names = {entry.rule for entry in rules}
    for entry in rules:
        text, page_spans = rule_text_with_page_spans(entry, pages, footers=config.footers)
        parsed = parse_articles(
            entry,
            text,
            known_rule_names=known_rule_names,
            page_spans=page_spans,
            effective_date=config.effective_date,
        )
        parsed_annexes = parse_annexes(entry, text, page_spans, doc=doc)
        if not parsed:
            no_article_rules.append(entry)
        articles.extend(parsed)
        annexes.extend(parsed_annexes)
    if not articles:
        raise ValueError(
            f"Article parsing found 0 rules with articles: profile={toc_result.profile} "
            f"matches={toc_result.match_count} toc_pages={format_page_range(toc_result.toc_pages)}"
        )

    skipped_duplicate_articles = find_skipped_duplicate_articles(rules, pages, articles, config=config)
    ambiguous_basis = [article for article in articles if article.article == "제1조" and not article.legal_basis]
    table_review = [article for article in articles if looks_table_sensitive(article)]

    rules_dir = output_dir / "rules"
    rules_dir.mkdir(parents=True, exist_ok=True)
    for article in articles:
        dest = output_dir / article.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(render_markdown(article, config=config), encoding="utf-8")
    for annex in annexes:
        dest = output_dir / annex.relative_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(render_annex_markdown(annex, config=config), encoding="utf-8")

    coverage = calculate_page_coverage(rules, articles, annexes, toc_result.toc_pages)
    suspicious_rules = find_suspicious_rules(rules, articles, annexes)

    result = BuildResult(
        rules=rules,
        articles=articles,
        annexes=annexes,
        no_article_rules=no_article_rules,
        ambiguous_basis=ambiguous_basis,
        table_review=table_review,
        skipped_duplicate_articles=skipped_duplicate_articles,
        coverage=coverage,
        suspicious_rules=suspicious_rules,
        output_dir=output_dir,
        toc_profile=toc_result.profile,
        toc_match_count=toc_result.match_count,
        toc_pages=toc_result.toc_pages,
        config=config,
    )
    (output_dir / "qa-report.md").write_text(render_qa_report(result, pdf_path), encoding="utf-8")
    (output_dir / "qa.json").write_text(json.dumps(render_qa_json(result, pdf_path), ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result


def find_suspicious_rules(
    rules: list[TocEntry],
    articles: list[Article],
    annexes: list[Annex],
    *,
    min_pages: int = 10,
    max_articles: int = 3,
) -> list[SuspiciousRule]:
    article_counts = Counter(article.rule.rule for article in articles)
    annex_counts = Counter(annex.rule.rule for annex in annexes)
    suspicious: list[SuspiciousRule] = []
    for rule in rules:
        page_count = rule.end_page - rule.start_page + 1
        article_count = article_counts[rule.rule]
        if page_count >= min_pages and article_count <= max_articles:
            suspicious.append(
                SuspiciousRule(
                    rule=rule,
                    article_count=article_count,
                    annex_count=annex_counts[rule.rule],
                    page_count=page_count,
                    reason=f"{page_count} pages but only {article_count} parsed articles",
                )
            )
    return suspicious


def calculate_page_coverage(
    rules: list[TocEntry],
    articles: list[Article],
    annexes: list[Annex],
    toc_pages: Iterable[int] = (),
) -> CoverageReport:
    toc_page_set = set(toc_pages)
    eligible = {
        page
        for rule in rules
        for page in range(rule.start_page, rule.end_page + 1)
        if page not in toc_page_set
    }
    covered = set()
    for item in [*articles, *annexes]:
        for page in range(item.source_pages[0], item.source_pages[1] + 1):
            if page in eligible:
                covered.add(page)
    uncovered = tuple(ranges_from_pages(sorted(eligible - covered)))
    percent = round((len(covered) / len(eligible) * 100.0), 2) if eligible else 100.0
    return CoverageReport(
        eligible_pages=tuple(sorted(eligible)),
        covered_pages=tuple(sorted(covered)),
        uncovered_pages=uncovered,
        percent=percent,
    )


def ranges_from_pages(pages: list[int]) -> list[tuple[int, int]]:
    if not pages:
        return []
    ranges: list[tuple[int, int]] = []
    start = prev = pages[0]
    for page in pages[1:]:
        if page == prev + 1:
            prev = page
            continue
        ranges.append((start, prev))
        start = prev = page
    ranges.append((start, prev))
    return ranges


def find_skipped_duplicate_articles(
    rules: list[TocEntry],
    pages: dict[int, str],
    emitted_articles: list[Article],
    config: BuildConfig = BuildConfig(),
) -> list[Article]:
    emitted = {(article.rule.code, article.article) for article in emitted_articles}
    skipped: list[Article] = []
    for entry in rules:
        text, page_spans = rule_text_with_page_spans(entry, pages, footers=config.footers)
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
                    amended=amended or config.effective_date,
                    refs=(),
                    legal_basis=(),
                    source_pages=page_offsets.get(key, (entry.start_page, entry.end_page)),
                )
            )
    return [article for article in skipped if (article.rule.code, article.article) in emitted]


def render_qa_report(result: BuildResult, pdf_path: Path) -> str:
    rule_counts = Counter(article.rule.rule for article in result.articles)
    annex_counts = Counter(annex.rule.rule for annex in result.annexes)
    extracted_rules = {article.rule.rule for article in result.articles} | {annex.rule.rule for annex in result.annexes}
    source_pages_covered = sum(1 for article in result.articles if article.source_pages)
    article_level_pages = sum(
        1
        for article in result.articles
        if (article.source_pages[0], article.source_pages[1]) != (article.rule.start_page, article.rule.end_page)
    )
    amended_extracted = sum(1 for article in result.articles if extract_article_amended(article.body))
    legal_basis_filled = sum(1 for article in result.articles if article.legal_basis)
    structured_annexes = sum(1 for annex in result.annexes if annex.table_structured)
    lines = [
        f"# {result.config.institution.upper()} rule Markdown build QA report",
        "",
    ]
    if not result.config.legacy_cni_report:
        lines.append(f"- Institution: `{result.config.institution}` ({result.config.institution_name})")
    lines.extend(
        [
            f"- Source PDF: `{pdf_path}`",
            f"- Source effective date: `{result.config.effective_date}`",
        ]
    )
    if result.config.source_url:
        lines.append(f"- Source URL: {result.config.source_url}")
    if not result.config.legacy_cni_report:
        lines.extend(
            [
                f"- TOC profile: `{result.toc_profile}`",
                f"- TOC pages: {format_page_range(result.toc_pages)}",
                f"- TOC matches: {result.toc_match_count}",
            ]
        )
    lines.extend(
        [
        f"- Rule units from TOC: {len(result.rules)}",
        f"- Extracted rule units with articles or annexes: {len(extracted_rules)}",
        f"- Article Markdown files emitted: {len(result.articles)}",
        f"- Annex Markdown files emitted: {len(result.annexes)}",
        f"- Rules without parsed articles: {len(result.no_article_rules)}",
        f"- Skipped duplicate article headings: {len(result.skipped_duplicate_articles)}",
        f"- First articles without extracted legal_basis: {len(result.ambiguous_basis)}",
        f"- Table/annex layout review candidates: {len(result.table_review)}",
        f"- source_pages coverage: {source_pages_covered}/{len(result.articles)}",
        f"- source_pages narrowed below rule range: {article_level_pages}",
        f"- amended markers extracted: {amended_extracted}",
        f"- legal_basis filled: {legal_basis_filled}",
        f"- Page coverage: {result.coverage.percent:.2f}% ({len(result.coverage.covered_pages)}/{len(result.coverage.eligible_pages)})",
        f"- Uncovered page ranges: {format_ranges(result.coverage.uncovered_pages) or 'None'}",
        f"- Annex table_structured success: {structured_annexes}/{len(result.annexes)}",
        f"- Suspicious low-density rules: {len(result.suspicious_rules)}",
        "",
        "## Rule article counts",
        "",
        "| rule | slug | articles | annexes | pages |",
        "| --- | --- | ---: | ---: | --- |",
        ]
    )
    for entry in result.rules:
        lines.append(
            f"| {entry.rule} | `{entry.slug}` | {rule_counts[entry.rule]} | {annex_counts[entry.rule]} | {entry.start_page}-{entry.end_page} |"
        )

    lines.extend(["", "## Page coverage", ""])
    lines.extend(
        [
            f"- Eligible pages: {len(result.coverage.eligible_pages)}",
            f"- Covered pages: {len(result.coverage.covered_pages)}",
            f"- Coverage: {result.coverage.percent:.2f}%",
            f"- Uncovered ranges: {format_ranges(result.coverage.uncovered_pages) or 'None'}",
        ]
    )

    lines.extend(["", "## Annex statistics", ""])
    if result.annexes:
        lines.extend(["| rule | annexes | table_structured |", "| --- | ---: | ---: |"])
        for entry in result.rules:
            rule_annexes = [annex for annex in result.annexes if annex.rule.rule == entry.rule]
            if not rule_annexes:
                continue
            lines.append(
                f"| {entry.rule} | {len(rule_annexes)} | {sum(1 for annex in rule_annexes if annex.table_structured)} |"
            )
    else:
        lines.append("- None.")

    lines.extend(["", "## Suspicious low-density rules", ""])
    if result.suspicious_rules:
        lines.extend(["| rule | slug | pages | articles | annexes | reason |", "| --- | --- | ---: | ---: | ---: | --- |"])
        for item in result.suspicious_rules:
            lines.append(
                f"| {item.rule.rule} | `{item.rule.slug}` | {item.page_count} | {item.article_count} | {item.annex_count} | {item.reason} |"
            )
    else:
        lines.append("- None.")

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


def format_ranges(ranges: Iterable[tuple[int, int]]) -> str:
    return ", ".join(str(start) if start == end else f"{start}-{end}" for start, end in ranges)


def render_qa_json(result: BuildResult, pdf_path: Path) -> dict[str, object]:
    rule_counts = Counter(article.rule.rule for article in result.articles)
    annex_counts = Counter(annex.rule.rule for annex in result.annexes)
    extracted_rules = {article.rule.rule for article in result.articles} | {annex.rule.rule for annex in result.annexes}
    return {
        "schema_version": 1,
        "institution": result.config.institution,
        "source_pdf": str(pdf_path),
        "source_url": result.config.source_url,
        "effective_date": result.config.effective_date,
        "toc": {
            "profile": result.toc_profile,
            "pages": list(result.toc_pages),
            "expected_rule_count": len(result.rules),
            "extracted_rule_count": len(extracted_rules),
            "match_count": result.toc_match_count,
        },
        "articles": {
            "count": len(result.articles),
            "by_rule": {entry.rule: rule_counts[entry.rule] for entry in result.rules},
        },
        "annexes": {
            "count": len(result.annexes),
            "by_rule": {entry.rule: annex_counts[entry.rule] for entry in result.rules},
            "table_structured": sum(1 for annex in result.annexes if annex.table_structured),
            "table_structured_rate": round(
                sum(1 for annex in result.annexes if annex.table_structured) / len(result.annexes), 4
            )
            if result.annexes
            else 0.0,
        },
        "coverage": {
            "eligible_pages": list(result.coverage.eligible_pages),
            "covered_pages": list(result.coverage.covered_pages),
            "uncovered_ranges": [list(item) for item in result.coverage.uncovered_pages],
            "percent": result.coverage.percent,
        },
        "quality_flags": {
            "rules_without_parsed_articles": [entry.rule for entry in result.no_article_rules],
            "ambiguous_basis_count": len(result.ambiguous_basis),
            "table_review_count": len(result.table_review),
            "skipped_duplicate_articles": len(result.skipped_duplicate_articles),
            "suspicious_low_density_rules": [
                {
                    "rule": item.rule.rule,
                    "slug": item.rule.slug,
                    "pages": [item.rule.start_page, item.rule.end_page],
                    "page_count": item.page_count,
                    "articles": item.article_count,
                    "annexes": item.annex_count,
                    "reason": item.reason,
                }
                for item in result.suspicious_rules
            ],
        },
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    root = project_root()
    parser = argparse.ArgumentParser(description="Build CNI rule Markdown files from the PDF rule book.")
    parser.add_argument("--pdf", type=Path, default=default_pdf_path(root))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--institution", default=INSTITUTION)
    parser.add_argument("--institution-name", default=INSTITUTION_NAME)
    parser.add_argument("--effective-date", type=parse_effective_date, default=SOURCE_EFFECTIVE_DATE)
    parser.add_argument("--footer", action="append", default=[])
    parser.add_argument("--toc-pages", type=parse_page_range)
    parser.add_argument("--source-url")
    args = parser.parse_args(argv)
    if args.output is None:
        base_output = root / "04_data" / "90_index-build"
        args.output = base_output if args.institution == INSTITUTION else base_output / args.institution
    if args.toc_pages is None and args.institution == INSTITUTION:
        args.toc_pages = (3, 4, 5)
    return args


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    args = parse_args(argv)
    config = BuildConfig(
        institution=args.institution,
        institution_name=args.institution_name,
        effective_date=args.effective_date,
        footers=tuple(args.footer) if args.footer else (args.institution_name,),
        legacy_cni_report=(not argv and args.institution == INSTITUTION),
        source_url=args.source_url,
    )
    try:
        result = build(args.pdf.resolve(), args.output.resolve(), config=config, toc_pages=args.toc_pages)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    print(f"source_pdf={args.pdf}")
    print(f"output_dir={result.output_dir}")
    print(f"institution={result.config.institution}")
    print(f"institution_name={result.config.institution_name}")
    print(f"effective_date={result.config.effective_date}")
    print(f"toc_profile={result.toc_profile}")
    print(f"toc_pages={format_page_range(result.toc_pages)}")
    print(f"toc_matches={result.toc_match_count}")
    print(f"rules={len(result.rules)}")
    print(f"articles={len(result.articles)}")
    print(f"annexes={len(result.annexes)}")
    print(f"coverage_percent={result.coverage.percent:.2f}")
    print(f"no_article_rules={len(result.no_article_rules)}")
    print(f"legal_basis_review={len(result.ambiguous_basis)}")
    print(f"table_review={len(result.table_review)}")
    print(f"skipped_duplicate_articles={len(result.skipped_duplicate_articles)}")
    print(f"qa_report={result.output_dir / 'qa-report.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
