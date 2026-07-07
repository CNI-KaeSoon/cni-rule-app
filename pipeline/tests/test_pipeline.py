import dataclasses

from cni_rule_pipeline.pipeline import (
    PageSpan,
    TocEntry,
    calculate_page_coverage,
    extract_refs,
    find_suspicious_rules,
    parse_annexes,
    parse_articles,
    parse_toc,
    parse_toc_with_profile,
    render_markdown,
    rule_text_with_page_spans,
    split_annex_text,
    slugify,
)


def test_parse_toc_entries_from_pdf_text_page():
    pages = {
        3: "III - 2  직제규정··········································91\n"
        "III - 3  인사관리규정····································98\n",
        4: "",
        5: "",
        100: "",
    }

    entries = parse_toc(pages)

    assert entries[0].code == "III-2"
    assert entries[0].rule == "직제규정"
    assert entries[0].start_page == 91
    assert entries[0].end_page == 97


def test_toc_profile_selects_dotted_no_code_and_skips_chapter_lines():
    pages = {
        1: "이사회 규정 …………………………………………………………………… 35\n"
        "제2장 제안의 접수·심사 ……………………………………………………………… 360\n"
        "운영위원회 규정 ……………………………………………………………… 40\n",
        40: "",
    }

    result = parse_toc_with_profile(pages, toc_pages=(1,))

    assert result.profile == "dotted-no-code"
    assert result.match_count == 2
    assert [entry.rule for entry in result.entries] == ["이사회 규정", "운영위원회 규정"]


def test_toc_profile_selects_numbered_dotted():
    pages = {
        3: "제1편 조례 및 정관\n"
        "1. 충청남도역사문화연구원 설립 및 지원조례······································· 3\n"
        "2. 충청남도역사문화연구원 정관·························································· 9\n",
        9: "",
    }

    result = parse_toc_with_profile(pages, toc_pages=(3,))

    assert result.profile == "numbered-dotted"
    assert result.match_count == 2
    assert result.entries[0].start_page == 3


def test_toc_profile_selects_numbered_slash():
    pages = {
        2: "□ 제3편 규정\n"
        "1. 제규정의 제정규정  /   69\n"
        "2. 직제규정  /   73\n",
        73: "",
    }

    result = parse_toc_with_profile(pages, toc_pages=(2,))

    assert result.profile == "numbered-slash"
    assert result.match_count == 2
    assert [entry.start_page for entry in result.entries] == [69, 73]


def test_toc_pages_remap_printed_numbers_to_physical_pages():
    pages = {
        1: "인사 규정 ………………………………………………………………… 141\n인사 규칙 ………………………………………………………………… 180\n",
        141: "130\n직무권한 위임전결 규정\n[별표 1]\n",
        152: "141\nⅤ. 인사 및 복무 – 인사 규정\n제1조(목적) 본문\n",
        191: "180\nⅤ. 인사 및 복무 – 인사 규칙\n제1조(목적) 본문\n",
        200: "189\n끝\n",
    }

    result = parse_toc_with_profile(pages, toc_pages=(1,))

    assert result.entries[0].rule == "인사 규정"
    assert result.entries[0].start_page == 152
    assert result.entries[0].end_page == 190
    assert result.entries[1].start_page == 191


def test_rule_text_trims_previous_rule_tail_before_heading():
    entry = TocEntry(code="A", rule="한국유교문화 간행규정", start_page=301, end_page=304)
    pages = {
        301: "제18조(운영세칙) 이전 규정 꼬리\n제1조(시행일) 이전 부칙\n",
        302: "[별표 1]\n이전 규정 별표\n",
        303: "별표 계속\n",
        304: "한국유교문화 간행규정\n제1조(명칭) 본문\n제2조(발행) 본문\n",
    }

    text, spans = rule_text_with_page_spans(entry, pages, footers=())
    articles = parse_articles(entry, text, page_spans=spans)

    assert text.startswith("한국유교문화 간행규정")
    assert [article.article for article in articles] == ["제1조", "제2조"]
    assert articles[0].source_pages == (304, 304)


def test_parse_articles_keeps_article_keys_and_titles():
    entry = TocEntry(code="III-2", rule="직제규정", start_page=91, end_page=97)
    text = """직제규정
제정 1995. 6. 9.
개정 2026. 2. 9.
제1조(목적) 이 규정은 조직과 업무를 정한다.
제3조의2(조직의 운영) 원장은 진단을 실시한다.
"""

    articles = parse_articles(entry, text)

    assert [article.article for article in articles] == ["제1조", "제3조의2"]
    assert articles[1].title == "조직의 운영"
    assert articles[0].amended == "2026-02-09"


def test_extract_refs_for_internal_same_rule_and_quoted_law():
    refs = extract_refs(
        "직제규정 제6조와 제7조를 준용하고 「근로기준법」 제60조를 따른다.",
        current_slug="인사관리규정",
        current_article="제2조",
    )

    assert {"target": f"{slugify('직제규정')}#제6조", "type": "준용"} in refs
    assert {"target": "인사관리규정#제7조", "type": "준용"} in refs
    assert {"target": f"{slugify('근로기준법')}#제60조", "type": "인용"} in refs
    assert {"target": "인사관리규정#제60조", "type": "준용"} not in refs


def test_slugify_fallback_matches_rules_core_sha256_contract():
    assert slugify("!!!") == "e84c538e7fe2"


def test_parse_articles_skips_duplicate_article_keys():
    entry = TocEntry(code="III-2", rule="직제규정", start_page=91, end_page=97)
    text = """직제규정
제정 1995. 6. 9.
제1조(목적) 본문 제1조이다.
제2조(조직) 본문 제2조이다.
부 칙(2026. 2. 9.)
제1조(시행일) 부칙 제1조이다.
"""

    articles = parse_articles(entry, text)

    assert [article.article for article in articles] == ["제1조", "제2조"]
    assert articles[0].title == "목적"


def test_extract_refs_uses_known_rule_suffix():
    refs = extract_refs(
        "연구원의 직제규정 제4조와 직원대외활동규칙 제4조를 따른다.",
        current_slug="자체감사규칙",
        current_article="제1조",
        known_rule_names={"직제규정", "직원대외활동규칙"},
    )

    assert {"target": "직제규정#제4조", "type": "인용"} in refs
    assert {"target": "직원대외활동규칙#제4조", "type": "인용"} in refs


def test_parse_articles_uses_pdf_page_spans_for_source_pages():
    entry = TocEntry(code="III-2", rule="직제규정", start_page=91, end_page=92)
    page_91 = "직제규정\n제정 1995. 6. 9.\n제1조(목적) 첫 페이지 본문이다."
    page_92 = "제2조(조직) 다음 페이지 본문이다."
    text = f"{page_91}\n{page_92}"
    spans = [
        PageSpan(page_no=91, start=0, end=len(page_91)),
        PageSpan(page_no=92, start=len(page_91) + 1, end=len(text)),
    ]

    articles = parse_articles(entry, text, page_spans=spans)

    assert articles[0].source_pages == (91, 91)
    assert articles[1].source_pages == (92, 92)


def test_parse_articles_extracts_article_level_amended_marker_latest_date():
    entry = TocEntry(code="III-2", rule="직제규정", start_page=91, end_page=91)
    text = """직제규정
제정 1995. 6. 9.
개정 2020. 1. 1.
제1조(목적) 이 규정은 조직과 업무를 정한다. [개정 2024. 3. 4.]
제2조(조직) 원장은 진단을 실시한다. <개정 2025. 5. 6.> [개정 2023. 1. 2.]
"""

    articles = parse_articles(entry, text)

    assert articles[0].amended == "2024-03-04"
    assert articles[1].amended == "2025-05-06"


def test_parse_articles_extracts_amended_marker_split_by_pdf_line_break():
    entry = TocEntry(code="III-9", rule="여비지급규칙", start_page=346, end_page=346)
    text = """여비지급규칙
제정 1995. 6. 9.
개정 2026. 2. 27.
제10조(여비지급기준) 식비는 정하는 금액을 지급한다. [전문
개정 2023. 8. 1.]
"""

    articles = parse_articles(entry, text)

    assert articles[0].amended == "2023-08-01"


def test_parse_articles_extracts_legal_basis_from_first_purpose_article():
    entry = TocEntry(code="III-2", rule="직제규정", start_page=91, end_page=91)
    text = """직제규정
제정 1995. 6. 9.
제1조(목적) 이 규정은 「지방자치단체출연 연구원의 설립 및 운영에 관한 법률」 제4조에 따라 조직과 업무를 정한다.
제2조(조직) 조직은 별도로 정한다.
"""

    articles = parse_articles(entry, text)

    assert articles[0].legal_basis == (
        {"law": "지방자치단체출연 연구원의 설립 및 운영에 관한 법률", "article": "제4조"},
    )
    assert articles[1].legal_basis == ()


def test_render_markdown_includes_source_pages_frontmatter():
    entry = TocEntry(code="III-2", rule="직제규정", start_page=91, end_page=92)
    article = parse_articles(entry, "직제규정\n제정 1995. 6. 9.\n제1조(목적) 본문이다.")[0]

    rendered = render_markdown(article)

    assert "source_pages: [91, 92]" in rendered
    assert "pages: [91, 92]" in rendered


def test_annex_boundary_detection_accepts_common_variants():
    entry = TocEntry(code="III-9", rule="여비지급규칙", start_page=10, end_page=12)
    text = """[별표 1] 국내여비 지급표
구분 금액
별표 제2호 국외여비
지역 금액
[별지 제3호 서식] 신청서
성명:
별지 제4호서식 정산서
금액:
"""
    spans = [PageSpan(page_no=10, start=0, end=len(text))]

    annexes = parse_annexes(entry, text, spans)

    assert [annex.annex for annex in annexes] == ["별표1", "별표2", "별지제3호", "별지제4호"]
    assert annexes[0].title == "국내여비 지급표"


def test_split_annex_text_keeps_annex_out_of_previous_article_tail():
    entry = TocEntry(code="III-9", rule="여비지급규칙", start_page=10, end_page=11)
    text = """여비지급규칙
제1조(목적) 목적이다.
제2조(기준) 별표 1에 따른다.
[별표 1] 국내여비 지급표
구분 금액
"""

    article_text, annex_text, _offset = split_annex_text(text)
    articles = parse_articles(entry, article_text)

    assert "[별표 1]" not in articles[-1].body
    assert annex_text.startswith("[별표 1]")


def test_inline_annex_form_reference_is_not_boundary():
    entry = TocEntry(code="III-2", rule="인사관리규정", start_page=115, end_page=116)
    text = """제54조(징계) [별지 제3호 서식]에 의한 대장을 비치한다.
제55조(징계요구) 임용권자는 요구하여야 한다.
[별지 제3호 서식] <신설 2018. 12. 21.>
훈계 등 처분대장
"""

    article_text, annex_text, _offset = split_annex_text(text)
    articles = parse_articles(entry, article_text)

    assert [article.article for article in articles] == ["제54조", "제55조"]
    assert annex_text.startswith("[별지 제3호 서식] <신설")


def test_line_start_annex_reference_with_particle_is_not_boundary():
    entry = TocEntry(code="A", rule="재무회계규정", start_page=99, end_page=101)
    text = """제18조(예산배정) 예산담당관은
[별지 제11호 서식]의 세출예산 배정서에 의하여 예산을 배정하여야 한다.
제19조(예산의 정리) 예산담당관은 예산원부를 비치한다.
제20조(예산의 집행품의) 품의한다.
[별지 제11호 서식]
세출예산 배정서
"""

    articles = parse_articles(entry, text)
    annexes = parse_annexes(entry, text, [PageSpan(page_no=99, start=0, end=len(text))])

    assert [article.article for article in articles] == ["제18조", "제19조", "제20조"]
    assert [annex.annex for annex in annexes] == ["별지제11호"]


def test_page_coverage_calculates_uncovered_rule_pages_excluding_toc():
    rule = TocEntry(code="III-9", rule="여비지급규칙", start_page=10, end_page=13)
    article = parse_articles(rule, "제1조(목적) 본문")[0]
    article = dataclasses.replace(article, source_pages=(10, 11))
    annex = parse_annexes(
        rule,
        "[별표 1] 표\n내용",
        [PageSpan(page_no=13, start=0, end=20)],
        offset=0,
    )[0]

    coverage = calculate_page_coverage([rule], [article], [annex], toc_pages=(10,))

    assert coverage.eligible_pages == (11, 12, 13)
    assert coverage.covered_pages == (11, 13)
    assert coverage.uncovered_pages == ((12, 12),)
    assert coverage.percent == 66.67


def test_find_suspicious_rules_flags_long_rule_with_too_few_articles():
    sparse = TocEntry(code="A", rule="인사 규정", start_page=152, end_page=190)
    dense = TocEntry(code="B", rule="인사 규칙", start_page=191, end_page=200)
    articles = parse_articles(sparse, "제1조(목적) 본문\n제2조(정의) 본문")
    articles += parse_articles(dense, "제1조(목적) 본문\n제2조(정의) 본문\n제3조(절차) 본문\n제4조(기록) 본문")

    suspicious = find_suspicious_rules([sparse, dense], articles, [])

    assert [item.rule.rule for item in suspicious] == ["인사 규정"]
    assert suspicious[0].page_count == 39


def test_analyze_qlogs_groups_normalized_questions_and_miss_candidates(tmp_path):
    import importlib.util
    import json
    import sys

    script_path = __import__("pathlib").Path(__file__).resolve().parents[1] / "analyze_qlogs.py"
    spec = importlib.util.spec_from_file_location("analyze_qlogs", script_path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)

    qlog = tmp_path / "qlog-install.jsonl"
    records = [
        {"question": "국내 출장 일비는 얼마인가요?", "mode": "Interpret"},
        {"question": "국내 출장 일비는 얼마인가요", "mode": "Interpret"},
        {"question": "육아휴직 규정 비교", "mode": "Compare", "miss": True},
    ]
    qlog.write_text("\n".join(json.dumps(record, ensure_ascii=False) for record in records) + "\n", encoding="utf-8")

    entries = module.read_qlogs(tmp_path)
    rendered = module.render_markdown(entries, diagnostics=[], limit=5, include_miss=True)

    assert "| 1 | 2 | 국내 출장 일비는 얼마인가요? | `국내 출장 일비는 얼마인가요` |" in rendered
    assert "## 실검색 Miss 후보" in rendered
    assert "육아휴직 규정 비교" in rendered
    assert "faq.json을 자동 생성하지 않습니다" in rendered
