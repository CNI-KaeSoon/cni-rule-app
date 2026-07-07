from cni_rule_pipeline.pipeline import (
    PageSpan,
    TocEntry,
    extract_refs,
    parse_articles,
    parse_toc,
    parse_toc_with_profile,
    render_markdown,
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
