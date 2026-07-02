import { describe, expect, it } from "vitest";
import {
  APP_VERSION_LABEL,
  BETA_BADGE_LABEL,
  FEEDBACK_URL,
  LABOR_DISCLAIMER,
  LABOR_MODE_NOTICE,
  toBackendMode
} from "./constants";

describe("mode constants", () => {
  it("keeps labor mode copy aligned with the contract", () => {
    expect(LABOR_MODE_NOTICE).toBe("이 도구는 내 상황이 무엇인지 최초 판단을 돕는 참고 도구입니다.");
    expect(LABOR_DISCLAIMER).toBe(
      "본 내용은 법률 자문이 아니며, 구체적인 사안은 노무사 등 전문가와 상담하시기 바랍니다."
    );
  });

  it("maps Korean UI labels to backend modes", () => {
    expect(toBackendMode("규정해석")).toBe("Interpret");
    expect(toBackendMode("노무상담")).toBe("Labor");
    expect(toBackendMode("규정비교")).toBe("Compare");
  });

  it("exposes beta channel labels and feedback URL", () => {
    expect(APP_VERSION_LABEL).toBe("0.1.0-beta");
    expect(BETA_BADGE_LABEL).toBe("베타");
    expect(FEEDBACK_URL).toBe("https://github.com/CNI-KaeSoon/cni-rule-app/issues");
  });
});
