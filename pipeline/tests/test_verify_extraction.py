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
