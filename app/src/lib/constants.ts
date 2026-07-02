export const LABOR_MODE_NOTICE =
  "이 도구는 내 상황이 무엇인지 최초 판단을 돕는 참고 도구입니다.";

export const LABOR_DISCLAIMER =
  "본 내용은 법률 자문이 아니며, 구체적인 사안은 노무사 등 전문가와 상담하시기 바랍니다.";

export const modes = ["규정해석", "노무상담", "규정비교"] as const;
export const APP_VERSION_LABEL = "0.1.0-beta";
export const BETA_BADGE_LABEL = "베타";
export const FEEDBACK_URL = "https://github.com/CNI-KaeSoon/cni-rule-app/issues";

export type ModeLabel = (typeof modes)[number];
export type ThemeChoice = "auto" | "light" | "dark";

export function toBackendMode(mode: ModeLabel): "Interpret" | "Labor" | "Compare" {
  if (mode === "노무상담") return "Labor";
  if (mode === "규정비교") return "Compare";
  return "Interpret";
}
