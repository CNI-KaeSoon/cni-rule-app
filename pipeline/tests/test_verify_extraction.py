import importlib.util
import sys
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "verify_extraction.py"
SPEC = importlib.util.spec_from_file_location("verify_extraction", MODULE_PATH)
assert SPEC is not None
verify_extraction = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules["verify_extraction"] = verify_extraction
SPEC.loader.exec_module(verify_extraction)

normalize_text = verify_extraction.normalize_text
similarity_score = verify_extraction.similarity_score
verdict = verify_extraction.verdict
ExtractedItem = verify_extraction.ExtractedItem
diagnose_mismatch = verify_extraction.diagnose_mismatch


def test_normalize_text_folds_whitespace_and_nbsp():
    assert normalize_text(" 제1조\u00a0 목적\n\n본문\t내용 ") == "제1조 목적 본문 내용"


def test_similarity_score_matches_contained_body():
    expected = "제1조 목적 이 규정은 조직과 업무를 정한다."
    actual = "머리말 제1조 목적 이 규정은 조직과 업무를 정한다. 다음 조문"

    assert similarity_score(expected, actual) == 1.0
    assert verdict(similarity_score(expected, actual)) == "match"


def test_similarity_score_classifies_partial_and_mismatch():
    partial = similarity_score("제1조 목적 예산 회계 감사 직원", "제1조 목적 예산 회계")
    mismatch = similarity_score("제1조 목적 예산 회계 감사 직원", "완전히 다른 본문")

    assert verdict(partial) == "partial"
    assert verdict(mismatch) == "mismatch"


def test_diagnose_mismatch_classifies_page_error_with_offset():
    item = ExtractedItem(
        path=Path("rules/규칙/제2조.md"),
        item_id="규칙#제2조",
        institution="cni",
        rule="규칙",
        kind="article",
        label="제2조",
        pages=(5, 5),
        body="제2조(정의) 이 규칙에서 직원은 근로자를 말한다.",
    )

    diagnosis, detail = diagnose_mismatch(
        item,
        recorded_score=0.1,
        page_texts={
            5: "부 칙 제2조(다른 규칙의 개정) 관련 문구",
            3: "제2조(정의) 이 규칙에서 직원은 근로자를 말한다.",
        },
    )

    assert diagnosis == "page-error(actual=3, recorded=5, offset=-2)"
    assert detail["kind"] == "page-error"
    assert detail["actual_page"] == 3


def test_diagnose_mismatch_classifies_text_error_without_better_page():
    item = ExtractedItem(
        path=Path("rules/규칙/제2조.md"),
        item_id="규칙#제2조",
        institution="cni",
        rule="규칙",
        kind="article",
        label="제2조",
        pages=(5, 5),
        body="제2조(정의) 이 규칙에서 직원은 근로자를 말한다.",
    )

    diagnosis, detail = diagnose_mismatch(
        item,
        recorded_score=0.1,
        page_texts={
            5: "부 칙 제2조(다른 규칙의 개정) 관련 문구",
            3: "완전히 다른 본문",
        },
    )

    assert diagnosis == "text-error"
    assert detail["kind"] == "text-error"
