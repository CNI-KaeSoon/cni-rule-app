//! 평가 지표(PH-01). 결정적이며 부동소수 합산 순서는 순위 오름차순으로 고정한다.
//!
//! 관련 = rel >= 1, 정답 = rel == 2. ID 판정은 문자열 완전 일치다.
//! `rels`의 키는 항상 `기관/규정#조문` 형태의 접두어 ID이므로, 서버가 단일 팩 모드에서
//! 접두어 없는 ID를 돌려줄 때는 [`canonical_id`]로 먼저 보정한 뒤 비교한다.

use std::collections::BTreeMap;

/// 서버가 돌려준 ID를 `rels` 키와 같은 접두어 ID로 보정한다.
/// 이미 `<institution>/`로 시작하면 그대로 두고, 아니면 접두어를 붙인다.
pub fn canonical_id(institution: &str, article_id: &str) -> String {
    let prefix = format!("{institution}/");
    if article_id.starts_with(&prefix) {
        article_id.to_string()
    } else {
        format!("{prefix}{article_id}")
    }
}

fn grade(rels: &BTreeMap<String, u8>, id: &str) -> u8 {
    rels.get(id).copied().unwrap_or(0)
}

fn relevant_count(rels: &BTreeMap<String, u8>) -> usize {
    rels.values().filter(|&&g| g >= 1).count()
}

fn dcg(grades: &[u8]) -> f64 {
    let mut sum = 0.0_f64;
    for (idx, &g) in grades.iter().enumerate() {
        let gain = (1_u64 << g) as f64 - 1.0;
        sum += gain / ((idx + 2) as f64).log2();
    }
    sum
}

/// nDCG@k. gain = 2^rel - 1, 할인 = log2(순위 + 1). 관련 문서가 없으면 None.
pub fn ndcg_at(ranked: &[String], rels: &BTreeMap<String, u8>, k: usize) -> Option<f64> {
    if relevant_count(rels) == 0 {
        return None;
    }
    let got: Vec<u8> = ranked.iter().take(k).map(|id| grade(rels, id)).collect();
    let mut ideal: Vec<u8> = rels.values().copied().filter(|&g| g >= 1).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    ideal.truncate(k);
    let idcg = dcg(&ideal);
    if idcg <= 0.0 {
        return None;
    }
    Some(dcg(&got) / idcg)
}

/// Recall@k = |top-k ∩ 관련| / |관련|. 관련 문서가 없으면 None.
pub fn recall_at(ranked: &[String], rels: &BTreeMap<String, u8>, k: usize) -> Option<f64> {
    let total = relevant_count(rels);
    if total == 0 {
        return None;
    }
    let found = ranked
        .iter()
        .take(k)
        .filter(|id| grade(rels, id) >= 1)
        .count();
    Some(found as f64 / total as f64)
}

/// Precision@k = |top-k ∩ 관련| / k (분모는 항상 k).
pub fn precision_at(ranked: &[String], rels: &BTreeMap<String, u8>, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let found = ranked
        .iter()
        .take(k)
        .filter(|id| grade(rels, id) >= 1)
        .count();
    found as f64 / k as f64
}

/// top-k 안에 등급 `min_grade` 이상 문서가 하나라도 있는가.
pub fn hit_at(ranked: &[String], rels: &BTreeMap<String, u8>, k: usize, min_grade: u8) -> bool {
    ranked
        .iter()
        .take(k)
        .any(|id| grade(rels, id) >= min_grade.max(1))
}

/// MRR@k. 첫 정답(등급 2)의 1/순위, 없으면 0.
pub fn mrr_at(ranked: &[String], rels: &BTreeMap<String, u8>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .position(|id| grade(rels, id) >= 2)
        .map_or(0.0, |pos| 1.0 / (pos + 1) as f64)
}

/// top-k 결과 중 기관이 `scope`와 다른 비율. `scope == "all"`이거나 결과가 없으면 None.
/// 결과가 k개 미만이면 분모는 실제 결과 수다.
pub fn misattr_at(ranked_institutions: &[String], scope: &str, k: usize) -> Option<f64> {
    if scope == "all" {
        return None;
    }
    let top: Vec<&String> = ranked_institutions.iter().take(k).collect();
    if top.is_empty() {
        return None;
    }
    let wrong = top.iter().filter(|inst| inst.as_str() != scope).count();
    Some(wrong as f64 / top.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn rels(items: &[(&str, u8)]) -> BTreeMap<String, u8> {
        items.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn ndcg_perfect_ranking_is_one() {
        let r = rels(&[("t/가#제1조", 2), ("t/가#제2조", 1)]);
        let ranked = ids(&["t/가#제1조", "t/가#제2조", "t/나#제1조"]);
        let v = ndcg_at(&ranked, &r, 10).unwrap();
        assert!((v - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ndcg_graded_example_matches_hand_computation() {
        // rels: A=2, B=1. 순위: X, B, A.
        // DCG  = 0/log2(2) + 1/log2(3) + 3/log2(4) = 0.6309298 + 1.5 = 2.1309298
        // IDCG = 3/log2(2) + 1/log2(3) = 3 + 0.6309298 = 3.6309298
        // nDCG = 0.586882...
        let r = rels(&[("t/A#제1조", 2), ("t/B#제1조", 1)]);
        let ranked = ids(&["t/X#제1조", "t/B#제1조", "t/A#제1조"]);
        let v = ndcg_at(&ranked, &r, 10).unwrap();
        assert!((v - 0.586882).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn ndcg_empty_rels_returns_none() {
        let r = BTreeMap::new();
        assert!(ndcg_at(&ids(&["t/A#제1조"]), &r, 10).is_none());
        assert!(recall_at(&ids(&["t/A#제1조"]), &r, 5).is_none());
    }

    #[test]
    fn recall_and_precision_count_grade_one_as_relevant() {
        let r = rels(&[("t/A#제1조", 2), ("t/B#별표1", 1), ("t/C#제3조", 1)]);
        let ranked = ids(&[
            "t/B#별표1",
            "t/X#제1조",
            "t/A#제1조",
            "t/Y#제1조",
            "t/Z#제1조",
        ]);
        // top-5 안의 관련 2개 / 관련 3개
        assert!((recall_at(&ranked, &r, 5).unwrap() - 2.0 / 3.0).abs() < 1e-12);
        assert!((precision_at(&ranked, &r, 5) - 0.4).abs() < 1e-12);
        // top-1만 보면 관련 1개
        assert!((recall_at(&ranked, &r, 1).unwrap() - 1.0 / 3.0).abs() < 1e-12);
        // 결과가 k개보다 적어도 precision 분모는 k
        assert!((precision_at(&ranked[..2], &r, 5) - 0.2).abs() < 1e-12);
    }

    #[test]
    fn hit_and_mrr_use_grade_two_only() {
        let r = rels(&[("t/A#제1조", 2), ("t/B#별표1", 1)]);
        let only_b = ids(&["t/B#별표1", "t/X#제1조"]);
        assert!(!hit_at(&only_b, &r, 5, 2));
        assert!(hit_at(&only_b, &r, 5, 1));
        assert_eq!(mrr_at(&only_b, &r, 10), 0.0);
        let a_third = ids(&["t/B#별표1", "t/X#제1조", "t/A#제1조"]);
        assert!(hit_at(&a_third, &r, 5, 2));
        assert!(!hit_at(&a_third, &r, 2, 2));
        assert!((mrr_at(&a_third, &r, 10) - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(mrr_at(&a_third, &r, 2), 0.0);
    }

    #[test]
    fn misattr_ignores_scope_all_and_handles_short_lists() {
        let insts = ids(&["cni", "ctp", "cni", "cni"]);
        assert!(misattr_at(&insts, "all", 5).is_none());
        // 결과 4개, 다른 기관 1개 -> 분모는 4
        assert!((misattr_at(&insts, "cni", 5).unwrap() - 0.25).abs() < 1e-12);
        // k=2 이면 상위 2개만
        assert!((misattr_at(&insts, "cni", 2).unwrap() - 0.5).abs() < 1e-12);
        assert!(misattr_at(&[], "cni", 5).is_none());
    }

    #[test]
    fn canonical_id_adds_prefix_only_when_missing() {
        assert_eq!(canonical_id("ctp", "시험규칙#제1조"), "ctp/시험규칙#제1조");
        assert_eq!(
            canonical_id("ctp", "ctp/시험규칙#제1조"),
            "ctp/시험규칙#제1조"
        );
        // 접두어 보정 후에는 rels(접두어 ID)와 일치한다.
        let r = rels(&[("ctp/시험규칙#제1조", 2)]);
        let single = vec![canonical_id("ctp", "시험규칙#제1조")];
        let multi = vec![canonical_id("ctp", "ctp/시험규칙#제1조")];
        assert!(hit_at(&single, &r, 5, 2));
        assert_eq!(single, multi);
    }
}
