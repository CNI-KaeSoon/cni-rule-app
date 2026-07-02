import { describe, expect, it } from "vitest";
import { appendAssistantDelta, extractCitationRefs, parseCitationSegments, type ChatMessage } from "./chat";

describe("chat stream rendering helpers", () => {
  it("accumulates deltas into the current assistant message", () => {
    const base: ChatMessage[] = [
      {
        id: "u1",
        conversation_id: "c1",
        role: "user",
        content: "연차휴가 이월 기준은?",
        created_at: "2026-07-02T00:00:00Z"
      }
    ];

    const first = appendAssistantDelta(base, "c1", "인사관리규정#제35조", false);
    const second = appendAssistantDelta(first, "c1", "에 따릅니다.", false);
    const done = appendAssistantDelta(second, "c1", "", true);

    expect(done).toHaveLength(2);
    expect(done[1]).toMatchObject({
      role: "assistant",
      content: "인사관리규정#제35조에 따릅니다.",
      streaming: false
    });
  });

  it("parses bracketed rule article references as citation segments", () => {
    const segments = parseCitationSegments("기준은 [인사관리규정#제35조의2]를 확인하세요.");

    expect(segments).toEqual([
      { type: "text", text: "기준은 " },
      { type: "citation", citation: { id: "인사관리규정#제35조의2", label: "인사관리규정 제35조의2" } },
      { type: "text", text: "를 확인하세요." }
    ]);
  });

  it("deduplicates citation chips", () => {
    expect(extractCitationRefs("[여비규정#제12조]와 [여비규정#제12조]")).toEqual([
      { id: "여비규정#제12조", label: "여비규정 제12조" }
    ]);
  });
});
