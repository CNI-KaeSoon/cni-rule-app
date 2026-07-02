export type ChatRole = "user" | "assistant";

export type ChatMessage = {
  id: string;
  conversation_id: string;
  role: ChatRole;
  content: string;
  created_at: string;
  streaming?: boolean;
};

export type CitationRef = {
  id: string;
  label: string;
};

export type TextSegment =
  | { type: "text"; text: string }
  | { type: "citation"; citation: CitationRef };

const citationPattern = /\[([^\]\n#]+#제\d+조(?:의\d+)?)\]/g;

export function parseCitationSegments(content: string): TextSegment[] {
  const segments: TextSegment[] = [];
  let cursor = 0;
  for (const match of content.matchAll(citationPattern)) {
    const raw = match[0];
    const id = match[1];
    const index = match.index ?? 0;
    if (index > cursor) {
      segments.push({ type: "text", text: content.slice(cursor, index) });
    }
    segments.push({ type: "citation", citation: { id, label: id.replace("#", " ") } });
    cursor = index + raw.length;
  }
  if (cursor < content.length) {
    segments.push({ type: "text", text: content.slice(cursor) });
  }
  return segments.length ? segments : [{ type: "text", text: content }];
}

export function extractCitationRefs(content: string): CitationRef[] {
  const refs = new Map<string, CitationRef>();
  for (const segment of parseCitationSegments(content)) {
    if (segment.type === "citation") refs.set(segment.citation.id, segment.citation);
  }
  return [...refs.values()];
}

export function appendAssistantDelta(
  messages: ChatMessage[],
  conversationId: string,
  content: string,
  done = false
): ChatMessage[] {
  const next = [...messages];
  const last = next[next.length - 1];
  if (last?.role === "assistant" && last.conversation_id === conversationId && last.streaming) {
    next[next.length - 1] = {
      ...last,
      content: done ? last.content : `${last.content}${content}`,
      streaming: !done
    };
    return next;
  }
  next.push({
    id: `stream-${conversationId}`,
    conversation_id: conversationId,
    role: "assistant",
    content: done ? "" : content,
    created_at: new Date().toISOString(),
    streaming: !done
  });
  return next;
}
