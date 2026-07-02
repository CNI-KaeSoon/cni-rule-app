from __future__ import annotations

import argparse
import json
import re
import unicodedata
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


QLOG_GLOB = "qlog-*.jsonl"
DIAGNOSTIC_GLOB = "diagnostics/*.json"
PUNCT_RE = re.compile(r"[!?？。，.,;:()\[\]{}\"'`~]")
SPACE_RE = re.compile(r"\s+")


@dataclass(frozen=True)
class QuestionEntry:
    question: str
    normalized: str
    source: Path
    miss: bool


@dataclass(frozen=True)
class DiagnosticEntry:
    path: Path
    reason: str
    question: str
    message_id: str
    citations_in_context: bool | None
    extracted_citations: tuple[str, ...]


def normalize_question(question: str) -> str:
    text = unicodedata.normalize("NFC", question)
    text = text.strip().lower()
    text = PUNCT_RE.sub(" ", text)
    text = SPACE_RE.sub(" ", text).strip()
    return text


def read_qlogs(shared_dir: Path) -> list[QuestionEntry]:
    entries: list[QuestionEntry] = []
    for path in sorted(shared_dir.glob(QLOG_GLOB)):
        with path.open("r", encoding="utf-8") as handle:
            for line_no, line in enumerate(handle, start=1):
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError as exc:
                    raise ValueError(f"{path}:{line_no}: invalid JSONL record") from exc
                question = str(record.get("question", "")).strip()
                if not question:
                    continue
                entries.append(
                    QuestionEntry(
                        question=question,
                        normalized=normalize_question(question),
                        source=path,
                        miss=bool(record.get("miss") or record.get("search_miss")),
                    )
                )
    return entries


def read_diagnostics(shared_dir: Path) -> list[DiagnosticEntry]:
    entries: list[DiagnosticEntry] = []
    for path in sorted(shared_dir.glob(DIAGNOSTIC_GLOB)):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise ValueError(f"{path}: invalid diagnostic JSON") from exc
        trace = record.get("trace") or {}
        entries.append(
            DiagnosticEntry(
                path=path,
                reason=str(record.get("reason") or "미지정"),
                question=str(record.get("question") or "").strip(),
                message_id=str(record.get("message_id") or ""),
                citations_in_context=trace.get("citations_in_context")
                if isinstance(trace.get("citations_in_context"), bool)
                else None,
                extracted_citations=tuple(str(item) for item in trace.get("extracted_citations") or []),
            )
        )
    return entries


def top_questions(entries: Iterable[QuestionEntry], limit: int) -> list[tuple[str, int, str]]:
    counts: Counter[str] = Counter()
    examples: dict[str, str] = {}
    for entry in entries:
        if not entry.normalized:
            continue
        counts[entry.normalized] += 1
        examples.setdefault(entry.normalized, entry.question)
    return [
        (normalized, count, examples[normalized])
        for normalized, count in counts.most_common(limit)
    ]


def miss_candidates(entries: Iterable[QuestionEntry], limit: int) -> list[tuple[str, int, str]]:
    grouped: defaultdict[str, list[QuestionEntry]] = defaultdict(list)
    for entry in entries:
        if entry.miss and entry.normalized:
            grouped[entry.normalized].append(entry)
    ranked = sorted(grouped.items(), key=lambda item: (-len(item[1]), item[0]))
    return [
        (normalized, len(group), group[0].question)
        for normalized, group in ranked[:limit]
    ]


def render_markdown(
    entries: list[QuestionEntry],
    diagnostics: list[DiagnosticEntry],
    limit: int,
    include_miss: bool,
) -> str:
    lines = [
        "# FAQ 후보 질문",
        "",
        "> 사람 검토용 초안입니다. 이 스크립트는 faq.json을 자동 생성하지 않습니다.",
        "",
        f"- 입력 로그: {len(entries)}개 질문",
        f"- 진단 리포트: {len(diagnostics)}개",
        f"- 중복 접기 기준: NFC, 소문자화, 문장부호 제거, 공백 정규화",
        "",
        "## 빈도 상위 질문",
        "",
        "| 순위 | 빈도 | 대표 질문 | 정규화 키 |",
        "| --- | ---: | --- | --- |",
    ]
    for index, (normalized, count, example) in enumerate(top_questions(entries, limit), start=1):
        lines.append(f"| {index} | {count} | {example} | `{normalized}` |")
    if include_miss:
        lines.extend(
            [
                "",
                "## 실검색 Miss 후보",
                "",
                "| 순위 | 빈도 | 대표 질문 | 정규화 키 |",
                "| --- | ---: | --- | --- |",
            ]
        )
        misses = miss_candidates(entries, limit)
        if misses:
            for index, (normalized, count, example) in enumerate(misses, start=1):
                lines.append(f"| {index} | {count} | {example} | `{normalized}` |")
        else:
            lines.append("| - | 0 | 표시할 miss 후보 없음 | - |")
    if diagnostics:
        lines.extend(
            [
                "",
                "## 신고 사유 집계",
                "",
                "| 사유 | 건수 |",
                "| --- | ---: |",
            ]
        )
        for reason, count in Counter(entry.reason for entry in diagnostics).most_common():
            lines.append(f"| {reason} | {count} |")

        lines.extend(
            [
                "",
                "## CSR false 신고",
                "",
                "| 질문 | 사유 | 추출 인용 | 파일 |",
                "| --- | --- | --- | --- |",
            ]
        )
        false_entries = [entry for entry in diagnostics if entry.citations_in_context is False]
        if false_entries:
            for entry in false_entries[:limit]:
                citations = ", ".join(entry.extracted_citations) or "없음"
                lines.append(f"| {entry.question} | {entry.reason} | {citations} | `{entry.path}` |")
        else:
            lines.append("| - | - | - | 표시할 CSR false 신고 없음 |")
    lines.append("")
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Analyze beta question JSONL logs and diagnostics.")
    parser.add_argument(
        "shared_dir",
        type=Path,
        help="Directory containing qlog-*.jsonl files and optional diagnostics/*.json reports",
    )
    parser.add_argument("-n", "--limit", type=int, default=20, help="Number of candidates to show")
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        default=Path("faq-candidates.md"),
        help="Markdown output path",
    )
    parser.add_argument(
        "--include-miss",
        action="store_true",
        help="Show records marked with miss/search_miss=true as search miss candidates",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    entries = read_qlogs(args.shared_dir)
    diagnostics = read_diagnostics(args.shared_dir)
    output = render_markdown(entries, diagnostics, args.limit, args.include_miss)
    args.output.write_text(output, encoding="utf-8")


if __name__ == "__main__":
    main()
