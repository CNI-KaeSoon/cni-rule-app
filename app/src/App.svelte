<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount, tick } from "svelte";
  import {
    APP_VERSION_LABEL,
    BETA_BADGE_LABEL,
    FEEDBACK_URL,
    LABOR_DISCLAIMER,
    LABOR_MODE_NOTICE,
    modes,
    toBackendMode,
    type ModeLabel,
    type ThemeChoice
  } from "./lib/constants";
  import {
    appendAssistantDelta,
    extractCitationRefs,
    parseCitationSegments,
    type ChatMessage
  } from "./lib/chat";

  type Screen = "start" | "interpret" | "labor" | "compare" | "engine" | "settings";
  type SidebarTab = "chat" | "article";
  type MainView = "chat" | "rulebook";
  type QuestionTelemetrySettings = {
    consent: boolean | null;
    shared_dir: string | null;
    install_id: string;
  };
  type RulebookArticle = {
    article: {
      id: string;
      rule: string;
      article: string;
      title: string;
      body: string;
      effective: string;
      amended: string;
    };
    source_pages: number[];
  };
  type Conversation = {
    id: string;
    title: string;
    mode: string;
    engine: string;
    created_at: string;
    updated_at: string;
    deleted_at: string | null;
  };
  type ConversationDetail = {
    conversation: Conversation;
    messages: ChatMessage[];
  };
  type ConversationGroup = {
    group: string;
    items: Conversation[];
  };
  type EngineKind = "ChatGpt" | "Claude" | "Gemini" | { ApiKey: "OpenAi" | "Anthropic" | "Google" | { Custom: string } };
  type EngineStatusDto = {
    kind: EngineKind;
    label: string;
    status: "Installed" | "NeedsLogin" | "Ready" | "Missing" | string;
  };
  type UpdateStatus = {
    institution: string;
    effective_date: string;
    source_commit: string;
    index_built_at: string;
    stale: boolean;
  };
  type UpdateProgress = {
    stage: string;
    message: string;
  };
  type SearchHit = {
    article_id: string;
    score: number;
    snippet: string;
    rule: string;
    title: string;
    effective: string;
  };

  let screen: Screen = "start";
  let activeMode: ModeLabel = "규정해석";
  let sidebarTab: SidebarTab = "chat";
  let mainView: MainView = "chat";
  let theme: ThemeChoice = "auto";
  let engineOpen = false;
  let settingsTab: "general" | "engine" | "privacy" | "rules" = "rules";
  let sidebarOpen = false;
  let prompt = "";
  let rulebookArticles: RulebookArticle[] = [];
  let activeRulebookPage: number | null = null;
  let telemetryConsent: boolean | null = null;
  let telemetrySharedDir = "";
  let telemetryInstallId = "";
  let telemetryStatus = "";
  let telemetryLoaded = false;
  let conversations: Conversation[] = [];
  let activeConversationId: string | null = null;
  let messages: ChatMessage[] = [];
  let conversationSearch = "";
  let engines: EngineStatusDto[] = [];
  let activeEngine: EngineStatusDto | null = null;
  let updateStatus: UpdateStatus | null = null;
  let updateProgress: UpdateProgress | null = null;
  let updateBusy = false;
  let updateMessage = "";
  let articleSearch = "";
  let searchHits: SearchHit[] = [];
  let selectedArticle: RulebookArticle | null = null;
  let settings: { key: string; value: string }[] = [];

  $: document.documentElement.setAttribute("data-theme", theme);
  $: conversationGroups = groupConversations(conversations.filter((item) => item.title.includes(conversationSearch.trim())));
  $: visibleArticles = selectedArticle
    ? [selectedArticle]
    : rulebookArticles.slice(0, 18);
  $: engineLabel = activeEngine?.label ?? "엔진";

  function openScreen(next: Screen) {
    screen = next;
    mainView = "chat";
    engineOpen = next === "engine";
    if (next === "labor") activeMode = "노무상담";
    if (next === "compare") activeMode = "규정비교";
    if (next === "start" || next === "interpret" || next === "engine" || next === "settings") {
      activeMode = "규정해석";
    }
    sidebarTab = next === "interpret" || next === "engine" || next === "settings" ? "article" : "chat";
  }

  function groupConversations(items: Conversation[]): ConversationGroup[] {
    const now = new Date();
    const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    const groups = new Map<string, Conversation[]>();
    for (const item of items) {
      const updated = new Date(item.updated_at).getTime();
      const days = Math.floor((startOfToday - new Date(new Date(updated).getFullYear(), new Date(updated).getMonth(), new Date(updated).getDate()).getTime()) / 86_400_000);
      const label = days <= 0 ? "오늘" : days === 1 ? "어제" : days <= 7 ? "지난 7일" : "이전";
      groups.set(label, [...(groups.get(label) ?? []), item]);
    }
    return ["오늘", "어제", "지난 7일", "이전"]
      .map((group) => ({ group, items: groups.get(group) ?? [] }))
      .filter((group) => group.items.length > 0);
  }

  function engineStatusLabel(status: string, label = "") {
    if (label === "Gemini") return "API 키 필요";
    if (status === "Ready" || status === "Installed") return "연결됨";
    if (status === "NeedsLogin") return "로그인 필요";
    return "미설치";
  }

  function engineDotClass(status: string, label = "") {
    if (status === "Ready" || status === "Installed") return "connected";
    if (label === "Gemini") return "key";
    return "";
  }

  function sameEngine(a: EngineKind, b: EngineKind) {
    return JSON.stringify(a) === JSON.stringify(b);
  }

  function messageParagraphs(content: string) {
    return content
      .replace(LABOR_DISCLAIMER, "")
      .split(/\n{2,}/)
      .map((paragraph) => paragraph.trim())
      .filter((paragraph) => paragraph.length > 0);
  }

  async function loadRulebook() {
    try {
      const result = await invoke<{ articles: RulebookArticle[] }>("get_rulebook");
      rulebookArticles = result.articles;
    } catch {
      rulebookArticles = [];
    }
  }

  async function loadConversations() {
    try {
      conversations = await invoke<Conversation[]>("conversations_list", { includeDeleted: false });
    } catch {
      conversations = [];
    }
  }

  async function loadConversation(id: string) {
    try {
      const detail = await invoke<ConversationDetail>("conversations_get", { id });
      activeConversationId = detail.conversation.id;
      messages = detail.messages;
      screen = detail.conversation.mode === "Labor" ? "labor" : detail.conversation.mode === "Compare" ? "compare" : "interpret";
      activeMode = detail.conversation.mode === "Labor" ? "노무상담" : detail.conversation.mode === "Compare" ? "규정비교" : "규정해석";
      mainView = "chat";
      sidebarTab = "chat";
    } catch {
      updateMessage = "대화를 불러오지 못했습니다.";
    }
  }

  function startNewConversation() {
    activeConversationId = null;
    messages = [];
    prompt = "";
    openScreen("start");
  }

  async function renameConversation(id: string, currentTitle: string) {
    const title = window.prompt("대화 이름", currentTitle)?.trim();
    if (!title || title === currentTitle) return;
    try {
      await invoke("conversations_rename", { id, title });
      await loadConversations();
    } catch {
      updateMessage = "대화 이름을 바꾸지 못했습니다.";
    }
  }

  async function trashConversation(id: string) {
    if (!window.confirm("이 대화를 휴지통으로 이동할까요?")) return;
    try {
      await invoke("conversations_delete_to_trash", { id });
      if (activeConversationId === id) startNewConversation();
      await loadConversations();
    } catch {
      updateMessage = "대화를 휴지통으로 이동하지 못했습니다.";
    }
  }

  async function loadEngines() {
    try {
      engines = await invoke<EngineStatusDto[]>("list_engines");
      activeEngine = await invoke<EngineStatusDto>("engine_status");
    } catch {
      engines = [];
      activeEngine = null;
    }
  }

  async function selectEngine(engine: EngineStatusDto) {
    try {
      activeEngine = await invoke<EngineStatusDto>("set_engine", { kind: engine.kind });
      await loadEngines();
      engineOpen = false;
    } catch {
      updateMessage = "엔진을 전환하지 못했습니다.";
    }
  }

  async function openRulebook(page: number | undefined) {
    if (!page) return;
    activeRulebookPage = page;
    mainView = "rulebook";
    if (!rulebookArticles.length) await loadRulebook();
    try {
      await invoke("open_rulebook", { page });
    } catch {
      // Browser preview without Tauri runtime keeps local UI state only.
    }
    await tick();
    document.getElementById(`page-${page}`)?.scrollIntoView({ block: "start" });
  }

  async function openArticle(id: string) {
    try {
      selectedArticle = await invoke<RulebookArticle>("get_article", { id });
      sidebarTab = "article";
      mainView = "rulebook";
      const page = selectedArticle.source_pages[0];
      if (page) await openRulebook(page);
    } catch {
      updateMessage = "조문을 불러오지 못했습니다.";
    }
  }

  async function searchArticles() {
    const q = articleSearch.trim();
    if (!q) {
      searchHits = [];
      selectedArticle = null;
      return;
    }
    try {
      searchHits = await invoke<SearchHit[]>("search", { q, filter: null });
    } catch {
      searchHits = [];
    }
  }

  async function checkAndApplyUpdate() {
    if (updateBusy) return;
    updateBusy = true;
    updateMessage = "업데이트 확인 중";
    try {
      updateStatus = await invoke<UpdateStatus>("check_update");
      updateMessage = "규정팩 적용 중";
      updateStatus = await invoke<UpdateStatus>("apply_update");
      updateMessage = "업데이트 완료";
      await loadRulebook();
    } catch {
      updateMessage = "업데이트를 적용하지 못했습니다.";
    } finally {
      updateBusy = false;
    }
  }

  async function loadSettings() {
    try {
      settings = await invoke<{ key: string; value: string }[]>("settings_list");
      const savedTheme = settings.find((item) => item.key === "theme")?.value;
      if (savedTheme === "auto" || savedTheme === "light" || savedTheme === "dark") theme = savedTheme;
    } catch {
      settings = [];
    }
  }

  async function saveTheme(next: ThemeChoice) {
    theme = next;
    try {
      await invoke("settings_set", { key: "theme", value: next });
    } catch {
      // Browser preview without Tauri runtime keeps local UI state only.
    }
  }

  async function loadTelemetrySettings() {
    try {
      const settings = await invoke<QuestionTelemetrySettings>("question_telemetry_get");
      telemetryConsent = settings.consent;
      telemetrySharedDir = settings.shared_dir ?? "";
      telemetryInstallId = settings.install_id;
      telemetryLoaded = true;
    } catch {
      telemetryConsent = false;
      telemetryLoaded = true;
    }
  }

  async function saveTelemetrySettings(consent: boolean, sharedDir = telemetrySharedDir) {
    try {
      const settings = await invoke<QuestionTelemetrySettings>("question_telemetry_set", {
        consent,
        sharedDir: sharedDir.trim() || null
      });
      telemetryConsent = settings.consent;
      telemetrySharedDir = settings.shared_dir ?? "";
      telemetryInstallId = settings.install_id;
      telemetryStatus = "저장됨";
    } catch {
      telemetryConsent = consent;
      telemetryStatus = "미리보기 모드";
    }
  }

  onMount(() => {
    void loadRulebook();
    void loadConversations();
    void loadEngines();
    void loadSettings();
    void loadTelemetrySettings();
    const unlisten = listen<{ page: number }>("rulebook://open", async (event) => {
      activeRulebookPage = event.payload.page;
      mainView = "rulebook";
      if (!rulebookArticles.length) await loadRulebook();
      await tick();
      document.getElementById(`page-${event.payload.page}`)?.scrollIntoView({ block: "start" });
    }).catch(() => undefined);
    const unlistenDelta = listen<{ conversation_id: string; content: string; done: boolean }>("chat://delta", (event) => {
      if (event.payload.conversation_id !== activeConversationId) return;
      messages = appendAssistantDelta(messages, event.payload.conversation_id, event.payload.content, event.payload.done);
    }).catch(() => undefined);
    const unlistenProgress = listen<UpdateProgress>("update://progress", (event) => {
      updateProgress = event.payload;
      updateMessage = event.payload.message;
    }).catch(() => undefined);
    const unlistenDone = listen<UpdateStatus>("update://done", (event) => {
      updateStatus = event.payload;
      updateMessage = "업데이트 완료";
    }).catch(() => undefined);
    return () => {
      void unlisten.then((dispose) => dispose?.());
      void unlistenDelta.then((dispose) => dispose?.());
      void unlistenProgress.then((dispose) => dispose?.());
      void unlistenDone.then((dispose) => dispose?.());
    };
  });

  function selectMode(mode: ModeLabel) {
    activeMode = mode;
    if (mode === "노무상담") openScreen("labor");
    if (mode === "규정비교") openScreen("compare");
    if (mode === "규정해석") openScreen("interpret");
  }

  async function submitPrompt() {
    const text = prompt.trim();
    if (!text) return;
    prompt = "";
    try {
      let conversationId = activeConversationId;
      if (!conversationId) {
        const conversation = await invoke<Conversation>("conversations_create", {
          title: text,
          mode: toBackendMode(activeMode)
        });
        conversationId = conversation.id;
        activeConversationId = conversation.id;
        await loadConversations();
      }
      screen = activeMode === "노무상담" ? "labor" : activeMode === "규정비교" ? "compare" : "interpret";
      mainView = "chat";
      messages = [
        ...messages,
        {
          id: `local-${Date.now()}`,
          conversation_id: conversationId,
          role: "user",
          content: text,
          created_at: new Date().toISOString()
        }
      ];
      await invoke("send_chat", {
        conversationId,
        mode: toBackendMode(activeMode),
        text
      });
      const detail = await invoke<ConversationDetail>("conversations_get", { id: conversationId });
      messages = detail.messages;
      await loadConversations();
    } catch {
      updateMessage = "답변을 가져오지 못했습니다.";
    }
  }
</script>

<main class="app-shell">
  <header class="app-header">
    <div class="header-left">
      <button class="hamburger-btn" title="사이드바 열기/닫기" aria-label="사이드바 토글" on:click={() => (sidebarOpen = !sidebarOpen)}>☰</button>
      <div class:open={engineOpen} class="engine-picker-wrap">
        <button class="engine-picker" aria-haspopup="listbox" aria-expanded={engineOpen} on:click={() => (engineOpen = !engineOpen)}>
          <span class="engine-dot {engineDotClass(activeEngine?.status ?? '', activeEngine?.label)}" aria-hidden="true"></span>
          <span>{engineLabel}</span>
          <span class="chevron">▾</span>
        </button>
        <div class="engine-menu" role="listbox">
          {#each engines as engine}
            <button
              class:active={activeEngine ? sameEngine(engine.kind, activeEngine.kind) : false}
              class="engine-item"
              role="option"
              aria-selected={activeEngine ? sameEngine(engine.kind, activeEngine.kind) : false}
              on:click={() => selectEngine(engine)}
            >
              <span class="dot {engineDotClass(engine.status, engine.label)}"></span>
              {engine.label}
              <span class={engine.status === "Ready" || engine.status === "Installed" ? "check" : "status"}>
                {engineStatusLabel(engine.status, engine.label)}
              </span>
            </button>
          {/each}
          {#if !engines.length}
            <div class="engine-empty">엔진 상태를 불러오는 중</div>
          {/if}
        </div>
      </div>
      <span class="beta-badge">{BETA_BADGE_LABEL}</span>
    </div>

    <nav class="mode-tabs" aria-label="대화 모드">
      {#each modes as mode}
        <button class:active={activeMode === mode} class="mode-tab" on:click={() => selectMode(mode)}>{mode}</button>
      {/each}
    </nav>

    <div class="header-right">
      <button class="icon-btn" title="규정 업데이트 확인" on:click={() => void checkAndApplyUpdate()}>🔄{#if updateBusy}<span class="badge">…</span>{/if}</button>
      <div class="theme-toggle-group" role="group" aria-label="테마 선택">
        <button class:active={theme === "auto"} class="theme-btn" title="자동 (시스템 설정)" on:click={() => void saveTheme("auto")}>🖥</button>
        <button class:active={theme === "light"} class="theme-btn" title="라이트" on:click={() => void saveTheme("light")}>☀</button>
        <button class:active={theme === "dark"} class="theme-btn" title="다크" on:click={() => void saveTheme("dark")}>🌙</button>
      </div>
      <button class="icon-btn" title="설정" on:click={() => openScreen("settings")}>⚙</button>
    </div>
  </header>

  <div class:sidebar-open={sidebarOpen} class="app-body">
    <aside class="sidebar">
      <div class="sidebar-header">
        <button class="new-chat-btn" on:click={startNewConversation}><span class="logo-badge">CNI</span> 새 대화</button>
      </div>
      <div class="sidebar-search">
        <span class="search-icon">⌕</span>
        <input type="text" placeholder="대화 검색" bind:value={conversationSearch} />
      </div>
      <div class="sidebar-tabs" role="tablist">
        <button class:active={sidebarTab === "chat"} class="sbtab" on:click={() => (sidebarTab = "chat")}>💬 대화</button>
        <button class:active={sidebarTab === "article"} class="sbtab" on:click={() => (sidebarTab = "article")}>📖 조문</button>
      </div>

      {#if sidebarTab === "chat"}
        <div class="sidebar-panel chat-panel">
          {#each conversationGroups as group}
            <div class="convo-group-label">{group.group}</div>
            {#each group.items as item}
              <div class:active={item.id === activeConversationId} class="convo-item">
                <button class="convo-title" on:click={() => loadConversation(item.id)}>{item.title}</button>
                <button class="convo-more" title="이름 변경" on:click={() => renameConversation(item.id, item.title)}>✎</button>
                <button class="convo-more" title="휴지통 이동" on:click={() => trashConversation(item.id)}>🗑</button>
              </div>
            {/each}
          {/each}
          {#if !conversationGroups.length}
            <p class="empty-state">저장된 대화가 없습니다.</p>
          {/if}
        </div>
      {:else}
        <div class="sidebar-panel article-panel">
          <div class="article-search">
            <input type="text" placeholder="조문 검색" bind:value={articleSearch} on:keydown={(event) => event.key === "Enter" && void searchArticles()} />
            <button class="article-source-btn" on:click={() => void searchArticles()}>검색</button>
          </div>
          {#if searchHits.length}
            {#each searchHits as hit}
              <button class="search-hit" on:click={() => openArticle(hit.article_id)}>
                <span class="article-breadcrumb">{hit.rule} &gt; {hit.article_id.split("#")[1]}</span>
                <strong>{hit.title}</strong>
                <span>{hit.snippet}</span>
              </button>
            {/each}
          {/if}
          {#each visibleArticles as item}
            <div class="article-card compact">
              <div class="article-breadcrumb">{item.article.rule} &gt; {item.article.article}</div>
              <h3 class="article-title">{item.article.article}({item.article.title})</h3>
              <p class="article-meta">개정 {item.article.amended}</p>
              <div class="article-body clamp">{item.article.body}</div>
              {#if item.source_pages[0]}
                <button class="article-source-btn" on:click={() => openRulebook(item.source_pages[0])}>
                  규정집에서 보기(p.{item.source_pages[0]})
                </button>
              {/if}
            </div>
          {/each}
          {#if !searchHits.length && !visibleArticles.length}
            <p class="empty-state">표시할 조문이 없습니다.</p>
          {/if}
        </div>
      {/if}

      <div class="sidebar-footer">
        <span class="avatar">연</span>
        <span class="user-chip">연구원 계정</span>
      </div>
    </aside>

    {#if screen === "start"}
      <section class="main-content start-screen">
        <div class="start-center">
          <div class="brand-mark">CNI</div>
          <h1 class="start-heading">무엇을 도와드릴까요?</h1>
          <button class="update-banner" on:click={() => void checkAndApplyUpdate()}>
            <span class="banner-icon">📋</span>
            <span>{updateMessage || "규정집 업데이트 확인"}</span>
            <span class="banner-arrow">→</span>
          </button>
          <div class="suggestion-grid">
            {#each ["연차휴가 이월 기준은?", "국내 출장 일비는 얼마인가요?", "육아휴직 중 보수는?", "겸직 허가 절차는?"] as suggestion}
              <button class="suggestion-card" on:click={() => (prompt = suggestion)}><span class="s-icon">✦</span><span class="s-text">{suggestion}</span></button>
            {/each}
          </div>
        </div>
        <div class="composer-area">
          <div class="composer">
            <button class="composer-attach" title="파일 첨부">+</button>
            <textarea class="composer-input" bind:value={prompt} placeholder="무엇이든 물어보세요" rows="1"></textarea>
            <button class="composer-send" title="전송" on:click={submitPrompt}>↑</button>
          </div>
          <div class="composer-disclaimer">CNI 규정도우미는 실수를 할 수 있습니다. 중요한 내용은 규정 원문을 확인하세요.</div>
        </div>
      </section>
    {:else}
      <section class:dimmed={screen === "settings"} class="main-content chat-screen">
        <div class="main-view-tabs" role="tablist" aria-label="메인 보기">
          <button class:active={mainView === "chat"} class="main-view-tab" on:click={() => (mainView = "chat")}>대화내용 보기</button>
          <button class:active={mainView === "rulebook"} class="main-view-tab" on:click={() => (mainView = "rulebook")}>규정집 보기</button>
        </div>
        {#if activeMode === "노무상담"}
          <div class="mode-banner"><span>ℹ</span><span>{LABOR_MODE_NOTICE}</span></div>
        {/if}
        {#if mainView === "rulebook"}
          <div class="rulebook-scroll" aria-label="규정집 보기">
            {#each (selectedArticle ? [selectedArticle] : rulebookArticles) as item}
              {@const page = item.source_pages[0]}
              <article
                id={page ? `page-${page}` : item.article.id}
                class:active-page={activeRulebookPage === page}
                class="rulebook-article"
              >
                <div class="rulebook-page-anchor">p.{page ?? "미지정"}</div>
                <div class="article-breadcrumb">{item.article.rule} &gt; {item.article.article}</div>
                <h2>{item.article.article}({item.article.title})</h2>
                <p class="article-meta">시행 {item.article.effective} · 개정 {item.article.amended}</p>
                <pre>{item.article.body}</pre>
              </article>
            {/each}
            {#if !rulebookArticles.length && !selectedArticle}
              <p class="empty-state">규정집을 불러오면 조문이 여기에 표시됩니다.</p>
            {/if}
          </div>
        {:else}
        <div class="chat-scroll">
          {#if messages.length}
            {#each messages as message (message.id)}
              {#if message.role === "user"}
                <div class="msg-user"><div class="bubble">{message.content}</div></div>
              {:else}
                <div class="msg-ai">
                  <div class="ai-avatar">C</div>
                  <div class="ai-content">
                    {#each messageParagraphs(message.content) as paragraph}
                      <p>
                        {#each parseCitationSegments(paragraph) as segment}
                          {#if segment.type === "citation"}
                            <button class="inline-citation" on:click={() => openArticle(segment.citation.id)}>{segment.citation.label}</button>
                          {:else}
                            {segment.text}
                          {/if}
                        {/each}
                      </p>
                    {/each}
                    {#if extractCitationRefs(message.content).length}
                      <div class="citation-row">
                        <span class="citation-label">근거</span>
                        {#each extractCitationRefs(message.content) as citation}
                          <button class="citation-chip" on:click={() => openArticle(citation.id)}>{citation.label}</button>
                        {/each}
                      </div>
                    {/if}
                    {#if activeMode === "노무상담" && message.content.includes(LABOR_DISCLAIMER)}
                      <div class="disclaimer-box"><span>⚠</span><span>{LABOR_DISCLAIMER}</span></div>
                    {/if}
                  </div>
                </div>
              {/if}
            {/each}
          {:else}
            <p class="empty-state">새 질문을 입력하면 대화가 시작됩니다.</p>
          {/if}
        </div>
        {/if}
        <div class="composer-area">
          <div class="composer">
            <button class="composer-attach" title="파일 첨부">+</button>
            <textarea class="composer-input" bind:value={prompt} placeholder="무엇이든 물어보세요" rows="1"></textarea>
            <button class="composer-send" title="전송" on:click={submitPrompt}>↑</button>
          </div>
        </div>
      </section>
    {/if}
  </div>

  {#if screen === "settings"}
    <div class="modal-backdrop">
      <div class="settings-modal" role="dialog" aria-label="설정">
        <div class="settings-modal-header"><h2>설정</h2><button class="icon-btn modal-close" title="닫기" on:click={() => openScreen("interpret")}>✕</button></div>
        <div class="settings-modal-body">
          <nav class="settings-nav">
            <button class:active={settingsTab === "general"} class="settings-nav-item" on:click={() => (settingsTab = "general")}>일반</button>
            <button class:active={settingsTab === "engine"} class="settings-nav-item" on:click={() => (settingsTab = "engine")}>엔진 연결</button>
            <button class:active={settingsTab === "privacy"} class="settings-nav-item" on:click={() => (settingsTab = "privacy")}>데이터 · 개인정보</button>
            <button class:active={settingsTab === "rules"} class="settings-nav-item" on:click={() => (settingsTab = "rules")}>규정집 정보</button>
          </nav>
          <div class="settings-content">
            {#if settingsTab === "general"}
              <h3 class="settings-section-title">일반</h3>
              <div class="settings-row"><span class="settings-row-label">채널</span><span class="settings-row-value">{BETA_BADGE_LABEL} <span class="mono">{APP_VERSION_LABEL}</span></span></div>
              <div class="settings-row"><span class="settings-row-label">테마</span><span class="settings-row-value">자동 / 라이트 / 다크</span></div>
              <div class="settings-row"><span class="settings-row-label">언어</span><span class="settings-row-value">한국어</span></div>
              <a class="feedback-link" href={FEEDBACK_URL} target="_blank" rel="noreferrer">피드백 보내기</a>
            {:else if settingsTab === "engine"}
              <h3 class="settings-section-title">엔진 연결</h3>
              {#each engines as engine}
                <div class="settings-row">
                  <span class="settings-row-label">{engine.label}</span>
                  <span class="settings-row-value">{engineStatusLabel(engine.status, engine.label)}</span>
                  <button class="settings-action-btn compact-action" on:click={() => selectEngine(engine)}>사용</button>
                </div>
              {/each}
              <p class="settings-help">Gemini CLI 경로는 차단 상태입니다. Gemini 사용은 API 키 경로가 필요합니다.</p>
              <input class="key-input" type="password" placeholder="sk-••••••••••••••••••••" />
            {:else if settingsTab === "privacy"}
              <h3 class="settings-section-title">데이터 · 개인정보</h3>
              <p class="settings-row-label">🔒 대화는 이 PC에만 저장됩니다. 외부 서버로 전송되지 않습니다.</p>
              <div class="settings-row telemetry-row">
                <span class="settings-row-label">
                  질문 문장 익명 팀 공유
                  <span class="settings-help">노무상담 모드는 수집하지 않습니다.</span>
                </span>
                <label class="switch">
                  <input
                    type="checkbox"
                    checked={telemetryConsent === true}
                    on:change={(event) => void saveTelemetrySettings(event.currentTarget.checked)}
                  />
                  <span></span>
                </label>
              </div>
              <div class="settings-row telemetry-path-row">
                <label class="settings-row-label" for="telemetry-shared-dir">공유 경로</label>
                <input
                  id="telemetry-shared-dir"
                  class="path-input"
                  type="text"
                  bind:value={telemetrySharedDir}
                  placeholder="예: Z:\\cni-rule-beta-qlogs"
                  on:change={() => void saveTelemetrySettings(telemetryConsent === true)}
                />
              </div>
              <div class="settings-row">
                <span class="settings-row-label">설치 ID</span>
                <span class="settings-row-value mono">{telemetryInstallId || "생성 전"}</span>
              </div>
              {#if telemetryStatus}
                <p class="settings-status">{telemetryStatus}</p>
              {/if}
              <div class="settings-row"><span class="settings-row-label">대화 내보내기</span><button class="settings-action-btn">Markdown으로 내보내기</button></div>
              <div class="settings-row"><span class="settings-row-label">전체 대화 휴지통 이동</span><button class="settings-action-btn danger">휴지통으로 이동</button></div>
            {:else}
              <h3 class="settings-section-title">규정집 정보</h3>
              <div class="settings-row"><span class="settings-row-label">기관</span><span class="settings-row-value">{updateStatus?.institution ?? "확인 전"}</span></div>
              <div class="settings-row"><span class="settings-row-label">규정집 버전</span><span class="settings-row-value">{updateStatus?.effective_date ?? "확인 전"}<span class="mono">{updateStatus?.source_commit ?? ""}</span></span></div>
              <div class="settings-row"><span class="settings-row-label">진행 상태</span><span class="settings-row-value">{updateProgress?.message ?? updateMessage ?? "대기"}</span></div>
              <button class="settings-action-btn" on:click={() => void checkAndApplyUpdate()} disabled={updateBusy}>지금 업데이트 확인</button>
            {/if}
          </div>
        </div>
        <div class="settings-modal-footer">🔒 대화는 이 PC에만 저장됩니다.</div>
      </div>
    </div>
  {/if}

  {#if telemetryLoaded && telemetryConsent === null}
    <div class="modal-backdrop consent-backdrop">
      <div class="consent-modal" role="dialog" aria-label="질문 공유 동의">
        <div class="settings-modal-header"><h2>질문 공유 동의</h2><span class="beta-badge">{BETA_BADGE_LABEL}</span></div>
        <div class="consent-body">
          <p>질문 문장만 익명으로 팀 공유 경로에 기록해 서비스 개선에 사용합니다</p>
          <p class="settings-row-label">답변과 대화 맥락은 수집하지 않으며, 노무상담 모드는 코드 레벨에서 수집 대상에서 제외됩니다.</p>
          <p class="settings-row-label">질문에 개인정보(이름·연락처 등) 입력은 자제해 주세요.</p>
        </div>
        <div class="consent-actions">
          <button class="settings-action-btn secondary" on:click={() => void saveTelemetrySettings(false)}>거부</button>
          <button class="settings-action-btn" on:click={() => void saveTelemetrySettings(true)}>동의</button>
        </div>
      </div>
    </div>
  {/if}
</main>

<style>
  :global(:root),
  :global(:root[data-theme="light"]) {
    --bg: #ffffff;
    --bg-elevated: #ffffff;
    --sidebar-bg: #f9f9f9;
    --border: rgba(0, 0, 0, 0.12);
    --border-soft: rgba(0, 0, 0, 0.08);
    --text: #0d0d0d;
    --text-secondary: #6e6e80;
    --bubble-user-bg: #f4f4f4;
    --hover: rgba(0, 0, 0, 0.05);
    --input-bg: #ffffff;
    --input-border: rgba(0, 0, 0, 0.16);
    --shadow: 0 8px 28px rgba(0, 0, 0, 0.1);
    --accent: #7d9cc4;
    --accent-strong: #6a8ab3;
    --accent-soft: #e8eef7;
    --accent-text: #2d4a72;
    --on-accent: #16294a;
    --danger-soft: #f6e6e2;
    --danger-text: #8a4a34;
    --good: #6fbf8b;
  }

  @media (prefers-color-scheme: dark) {
    :global(:root[data-theme="auto"]) {
      --bg: #212121;
      --bg-elevated: #2a2a2a;
      --sidebar-bg: #171717;
      --border: rgba(255, 255, 255, 0.08);
      --border-soft: rgba(255, 255, 255, 0.06);
      --text: #ececec;
      --text-secondary: #a3a3ad;
      --bubble-user-bg: #2f2f2f;
      --hover: rgba(255, 255, 255, 0.07);
      --input-bg: #2f2f2f;
      --input-border: rgba(255, 255, 255, 0.14);
      --shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
      --accent: #8fa8c9;
      --accent-strong: #a3bad7;
      --accent-soft: rgba(143, 168, 201, 0.16);
      --accent-text: #bcd0e6;
      --on-accent: #16294a;
      --danger-soft: rgba(202, 138, 110, 0.16);
      --danger-text: #e0b39d;
      --good: #7fcf9c;
    }
  }

  :global(:root[data-theme="dark"]) {
    --bg: #212121;
    --bg-elevated: #2a2a2a;
    --sidebar-bg: #171717;
    --border: rgba(255, 255, 255, 0.08);
    --border-soft: rgba(255, 255, 255, 0.06);
    --text: #ececec;
    --text-secondary: #a3a3ad;
    --bubble-user-bg: #2f2f2f;
    --hover: rgba(255, 255, 255, 0.07);
    --input-bg: #2f2f2f;
    --input-border: rgba(255, 255, 255, 0.14);
    --shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
    --accent: #8fa8c9;
    --accent-soft: rgba(143, 168, 201, 0.16);
    --accent-text: #bcd0e6;
    --on-accent: #16294a;
    --danger-soft: rgba(202, 138, 110, 0.16);
    --danger-text: #e0b39d;
    --good: #7fcf9c;
  }

  :global(*) { box-sizing: border-box; }
  :global(html), :global(body), :global(#app) { margin: 0; min-height: 100%; height: 100%; }
  :global(body) {
    font-family: Pretendard, -apple-system, BlinkMacSystemFont, "Malgun Gothic", system-ui, sans-serif;
    color: var(--text);
    overflow: hidden;
  }
  button, textarea, input { font-family: inherit; }
  button { cursor: pointer; }
  .app-shell { display: flex; flex-direction: column; height: 100vh; background: var(--bg); color: var(--text); position: relative; }
  .app-header { flex-shrink: 0; height: 56px; display: flex; align-items: center; gap: 12px; padding: 0 16px; border-bottom: 1px solid var(--border); background: var(--bg); }
  .header-left, .header-right { display: flex; align-items: center; gap: 8px; }
  .header-right { margin-left: auto; }
  .hamburger-btn { display: none; width: 36px; height: 36px; border-radius: 8px; align-items: center; justify-content: center; background: transparent; border: 1px solid var(--border); color: var(--text); }
  .engine-picker-wrap { position: relative; }
  .engine-picker { display: flex; align-items: center; gap: 6px; background: transparent; border: 1px solid var(--border); color: var(--text); padding: 6px 12px 6px 10px; border-radius: 999px; font-size: 13.5px; font-weight: 500; }
  .engine-dot, .dot { width: 7px; height: 7px; border-radius: 50%; background: var(--good); flex-shrink: 0; }
  .dot { background: var(--text-secondary); }
  .dot.connected { background: var(--good); }
  .dot.key { background: var(--accent); }
  .chevron { font-size: 10px; color: var(--text-secondary); transition: transform 0.15s; }
  .open .chevron { transform: rotate(180deg); }
  .engine-menu { display: none; position: absolute; top: calc(100% + 6px); left: 0; min-width: 240px; background: var(--bg-elevated); border: 1px solid var(--border); border-radius: 12px; padding: 6px; box-shadow: var(--shadow); z-index: 60; }
  .open .engine-menu { display: block; }
  .engine-item { width: 100%; display: flex; align-items: center; gap: 8px; background: transparent; border: none; text-align: left; padding: 9px 10px; border-radius: 8px; font-size: 13.5px; color: var(--text); }
  .engine-item:hover { background: var(--hover); }
  .engine-item.active { background: var(--accent-soft); color: var(--accent-text); font-weight: 600; }
  .engine-empty { padding: 10px; color: var(--text-secondary); font-size: 12.5px; }
  .status, .check { margin-left: auto; font-size: 12px; color: var(--text-secondary); font-weight: 400; }
  .check { color: var(--good); font-weight: 600; }
  .mode-tabs { display: flex; gap: 4px; margin: 0 auto; }
  .mode-tab { background: transparent; border: none; color: var(--text-secondary); padding: 7px 16px; border-radius: 999px; font-size: 13.5px; font-weight: 500; }
  .mode-tab:hover, .icon-btn:hover, .theme-btn:hover, .new-chat-btn:hover, .convo-item:hover { background: var(--hover); }
  .mode-tab.active { background: var(--accent-soft); color: var(--accent-text); font-weight: 700; }
  .icon-btn { position: relative; width: 36px; height: 36px; border-radius: 50%; display: flex; align-items: center; justify-content: center; background: transparent; border: none; color: var(--text); font-size: 16px; }
  .theme-toggle-group { display: flex; align-items: center; gap: 1px; background: var(--sidebar-bg); border: 1px solid var(--border); border-radius: 999px; padding: 2px; flex-shrink: 0; }
  .theme-btn { width: 26px; height: 26px; border-radius: 999px; border: none; background: transparent; color: var(--text-secondary); font-size: 12px; display: flex; align-items: center; justify-content: center; }
  .theme-btn.active { background: var(--accent-soft); color: var(--accent-text); }
  .badge { position: absolute; top: 3px; right: 3px; min-width: 14px; height: 14px; border-radius: 7px; background: var(--accent); color: var(--on-accent); font-size: 9px; font-weight: 700; line-height: 14px; text-align: center; }
  .beta-badge { display: inline-flex; align-items: center; justify-content: center; min-height: 22px; padding: 3px 8px; border-radius: 999px; background: var(--accent-soft); color: var(--accent-text); border: 1px solid var(--border); font-size: 11px; font-weight: 800; line-height: 1; white-space: nowrap; }
  .app-body { flex: 1; min-height: 0; display: flex; position: relative; }
  .sidebar { width: 268px; flex-shrink: 0; background: var(--sidebar-bg); border-right: 1px solid var(--border); display: flex; flex-direction: column; min-height: 0; }
  .sidebar-header { padding: 12px 12px 6px; }
  .new-chat-btn { width: 100%; display: flex; align-items: center; gap: 8px; background: transparent; border: 1px solid var(--border); color: var(--text); padding: 9px 12px; border-radius: 10px; font-size: 13.5px; font-weight: 500; }
  .logo-badge { width: 20px; height: 20px; border-radius: 6px; background: var(--accent-soft); color: var(--accent-text); display: flex; align-items: center; justify-content: center; font-size: 9px; font-weight: 800; }
  .sidebar-search { margin: 4px 12px 8px; display: flex; align-items: center; gap: 8px; border: 1px solid var(--border); border-radius: 10px; padding: 7px 10px; }
  .sidebar-search input { flex: 1; border: none; background: transparent; outline: none; color: var(--text); font-size: 13px; min-width: 0; }
  .search-icon, .convo-group-label, .article-breadcrumb, .composer-disclaimer, .settings-row-label { color: var(--text-secondary); }
  .sidebar-tabs { display: flex; border-bottom: 1px solid var(--border); padding: 0 8px; }
  .sbtab { flex: 1; text-align: center; padding: 9px 4px; background: transparent; border: none; border-bottom: 2px solid transparent; color: var(--text-secondary); font-size: 13px; font-weight: 500; }
  .sbtab.active { color: var(--accent-text); border-color: var(--accent); font-weight: 700; }
  .sidebar-panel { flex: 1; min-height: 0; overflow-y: auto; padding: 6px 8px; }
  .convo-group-label { font-size: 12px; padding: 10px 8px 4px; font-weight: 600; }
  .convo-item { display: flex; align-items: center; gap: 6px; padding: 8px 10px; border-radius: 8px; }
  .convo-item.active { background: var(--accent-soft); }
  .convo-title { flex: 1; font-size: 13.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; color: var(--text); background: transparent; border: none; text-align: left; padding: 0; min-width: 0; }
  .convo-more { background: transparent; border: none; color: var(--text-secondary); width: 22px; height: 22px; border-radius: 6px; }
  .empty-state { color: var(--text-secondary); font-size: 13px; line-height: 1.6; padding: 12px; text-align: center; }
  .article-search { display: flex; gap: 6px; align-items: center; padding: 6px 4px 10px; }
  .article-search input { flex: 1; min-width: 0; border: 1px solid var(--border); background: var(--input-bg); color: var(--text); border-radius: 8px; padding: 8px 10px; font-size: 12.5px; outline: none; }
  .search-hit { width: 100%; display: flex; flex-direction: column; gap: 4px; text-align: left; background: transparent; color: var(--text); border: 1px solid var(--border-soft); border-radius: 8px; padding: 9px; margin-bottom: 6px; }
  .search-hit:hover { background: var(--hover); }
  .search-hit span:last-child { color: var(--text-secondary); font-size: 12.5px; line-height: 1.5; }
  .article-card { padding: 6px 6px 10px; }
  .article-card.compact { border-bottom: 1px solid var(--border-soft); padding: 10px 6px 12px; }
  .article-breadcrumb { font-size: 11.5px; margin-bottom: 6px; }
  .article-title { font-size: 15px; font-weight: 700; margin: 0 0 2px; color: var(--text); }
  .article-meta { font-size: 12px; color: var(--accent-text); margin: 0 0 10px; font-weight: 600; }
  .article-body { font-size: 13.2px; line-height: 1.8; color: var(--text); }
  .article-body.clamp { display: -webkit-box; -webkit-line-clamp: 4; -webkit-box-orient: vertical; overflow: hidden; white-space: pre-line; }
  .article-source-btn { margin-top: 10px; border: 1px solid var(--border); background: var(--bg); color: var(--accent-text); border-radius: 8px; padding: 7px 10px; font-size: 12.5px; font-weight: 700; }
  .article-source-btn:hover { background: var(--accent-soft); }
  .sidebar-footer { flex-shrink: 0; padding: 10px 14px; border-top: 1px solid var(--border); display: flex; align-items: center; gap: 8px; }
  .avatar, .brand-mark, .ai-avatar { border-radius: 50%; background: var(--accent-soft); color: var(--accent-text); display: flex; align-items: center; justify-content: center; font-weight: 800; }
  .avatar { width: 26px; height: 26px; font-size: 12px; }
  .user-chip { font-size: 13px; color: var(--text-secondary); }
  .main-content { flex: 1; min-width: 0; display: flex; flex-direction: column; min-height: 0; position: relative; background: var(--bg); }
  .main-content.dimmed { filter: blur(1.5px) brightness(0.88); pointer-events: none; user-select: none; }
  .start-screen, .chat-screen { justify-content: space-between; }
  .start-center { flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 18px; padding: 24px; overflow-y: auto; }
  .brand-mark { width: 52px; height: 52px; font-size: 15px; letter-spacing: 0; }
  .start-heading { font-size: 26px; font-weight: 700; margin: 0; color: var(--text); letter-spacing: 0; }
  .update-banner { display: flex; align-items: center; gap: 10px; background: var(--accent-soft); color: var(--accent-text); padding: 10px 18px; border-radius: 12px; font-size: 13.5px; font-weight: 500; border: 1px solid transparent; max-width: 640px; }
  .suggestion-grid { display: grid; grid-template-columns: repeat(2, minmax(230px, 1fr)); gap: 12px; width: 100%; max-width: 660px; }
  .suggestion-card { display: flex; flex-direction: column; gap: 8px; text-align: left; padding: 14px 16px; border: 1px solid var(--border); border-radius: 14px; background: var(--bg); color: var(--text); }
  .s-icon { font-size: 19px; }
  .s-text { font-size: 13.8px; line-height: 1.4; }
  .composer-area { flex-shrink: 0; display: flex; flex-direction: column; align-items: center; gap: 8px; padding: 12px 24px 20px; }
  .composer { width: 100%; max-width: 760px; display: flex; align-items: flex-end; gap: 8px; background: var(--input-bg); border: 1px solid var(--input-border); border-radius: 28px; padding: 8px 8px 8px 16px; box-shadow: var(--shadow); }
  .composer-attach, .composer-send { width: 32px; height: 32px; border-radius: 50%; flex-shrink: 0; display: flex; align-items: center; justify-content: center; }
  .composer-attach { border: 1px solid var(--border); background: transparent; color: var(--text); font-size: 17px; }
  .composer-send { border: none; background: var(--accent); color: var(--on-accent); font-size: 15px; font-weight: 700; }
  .composer-input { flex: 1; border: none; background: transparent; outline: none; resize: none; color: var(--text); font-size: 15px; line-height: 1.5; padding: 6px 0; max-height: 120px; }
  .composer-disclaimer { font-size: 11.5px; text-align: center; max-width: 640px; }
  .chat-scroll { flex: 1; overflow-y: auto; padding: 24px 24px 8px; display: flex; flex-direction: column; gap: 22px; max-width: 820px; width: 100%; margin: 0 auto; }
  .main-view-tabs { flex-shrink: 0; display: flex; justify-content: center; gap: 4px; padding: 12px 16px 0; }
  .main-view-tab { border: 1px solid var(--border); background: transparent; color: var(--text-secondary); padding: 7px 14px; border-radius: 999px; font-size: 13px; font-weight: 700; }
  .main-view-tab.active { background: var(--accent-soft); color: var(--accent-text); border-color: transparent; }
  .rulebook-scroll { flex: 1; overflow-y: auto; padding: 18px 24px 24px; display: flex; flex-direction: column; gap: 14px; max-width: 900px; width: 100%; margin: 0 auto; }
  .rulebook-article { position: relative; border-bottom: 1px solid var(--border-soft); padding: 16px 4px 20px 74px; scroll-margin-top: 16px; }
  .rulebook-article.active-page { background: var(--accent-soft); border-radius: 8px; padding-right: 12px; }
  .rulebook-page-anchor { position: absolute; left: 4px; top: 18px; min-width: 52px; color: var(--accent-text); font-size: 12px; font-weight: 800; }
  .rulebook-article h2 { margin: 0 0 4px; font-size: 17px; letter-spacing: 0; }
  .rulebook-article pre { margin: 12px 0 0; white-space: pre-wrap; font-family: inherit; font-size: 14px; line-height: 1.8; color: var(--text); }
  .msg-user { display: flex; justify-content: flex-end; }
  .bubble { background: var(--bubble-user-bg); color: var(--text); padding: 11px 18px; border-radius: 22px; max-width: 72%; font-size: 15px; line-height: 1.6; }
  .msg-ai { display: flex; gap: 12px; align-items: flex-start; }
  .ai-avatar { width: 28px; height: 28px; flex-shrink: 0; font-size: 12.5px; margin-top: 2px; }
  .ai-content { flex: 1; min-width: 0; font-size: 15px; line-height: 1.8; color: var(--text); }
  .ai-content p { margin: 0 0 12px; }
  .footnote { color: var(--accent-text); font-weight: 700; }
  .citation-row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; margin: 4px 0; }
  .citation-label { font-size: 12.8px; color: var(--text-secondary); font-weight: 700; }
  .citation-chip, .inline-citation { background: var(--accent-soft); color: var(--accent-text); border: 1px solid transparent; padding: 4px 12px; border-radius: 999px; font-size: 12.3px; font-weight: 700; }
  .inline-citation { display: inline-flex; margin: 0 2px; vertical-align: baseline; }
  .mode-banner { max-width: 820px; width: calc(100% - 48px); margin: 16px auto 0; display: flex; align-items: center; gap: 10px; background: var(--accent-soft); color: var(--accent-text); padding: 11px 16px; border-radius: 12px; font-size: 13.3px; font-weight: 500; }
  .disclaimer-box { margin-top: 12px; border: 1px solid var(--border); border-radius: 10px; padding: 10px 14px; font-size: 12.6px; color: var(--text-secondary); display: flex; gap: 8px; align-items: flex-start; }
  .modal-backdrop { position: absolute; inset: 0; background: rgba(0, 0, 0, 0.5); display: flex; align-items: center; justify-content: center; z-index: 80; padding: 24px; }
  .settings-modal { width: 720px; max-width: 100%; max-height: 82vh; background: var(--bg); border: 1px solid var(--border); border-radius: 16px; display: flex; flex-direction: column; overflow: hidden; box-shadow: var(--shadow); }
  .settings-modal-header { flex-shrink: 0; padding: 16px 20px; border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
  .settings-modal-header h2 { margin: 0; font-size: 16px; }
  .settings-modal-body { flex: 1; min-height: 0; display: flex; overflow: hidden; }
  .settings-nav { width: 190px; flex-shrink: 0; background: var(--sidebar-bg); border-right: 1px solid var(--border); padding: 10px; display: flex; flex-direction: column; gap: 2px; }
  .settings-nav-item { text-align: left; background: transparent; border: none; color: var(--text-secondary); padding: 10px 12px; border-radius: 8px; font-size: 13.5px; }
  .settings-nav-item.active { background: var(--accent-soft); color: var(--accent-text); font-weight: 700; }
  .settings-content { flex: 1; min-width: 0; overflow-y: auto; padding: 22px 26px; }
  .settings-section-title { font-size: 15px; margin: 0 0 14px; }
  .settings-row { display: flex; justify-content: space-between; align-items: center; gap: 16px; font-size: 13.8px; padding: 10px 0; border-bottom: 1px solid var(--border-soft); }
  .settings-row-value { color: var(--text); font-weight: 600; text-align: right; }
  .settings-help { display: block; margin-top: 4px; color: var(--text-secondary); font-size: 12px; line-height: 1.4; }
  .mono { font-family: "SFMono-Regular", Consolas, monospace; font-size: 12px; color: var(--text-secondary); font-weight: 400; margin-left: 4px; }
  .feedback-link { display: inline-flex; align-items: center; justify-content: center; margin-top: 14px; min-height: 34px; padding: 8px 14px; border-radius: 8px; border: 1px solid var(--border); color: var(--accent-text); text-decoration: none; font-size: 13.3px; font-weight: 700; }
  .settings-action-btn { margin-top: 14px; padding: 8px 18px; border-radius: 8px; background: var(--accent); color: var(--on-accent); border: none; font-size: 13.3px; font-weight: 700; }
  .settings-action-btn:disabled { opacity: 0.65; cursor: default; }
  .compact-action { margin-top: 0; padding: 6px 10px; }
  .settings-action-btn.secondary { background: transparent; color: var(--text); border: 1px solid var(--border); }
  .settings-action-btn.danger { background: transparent; color: var(--danger-text); border: 1px solid var(--border); }
  .key-input, .path-input { width: 100%; border: 1px solid var(--border); background: var(--input-bg); color: var(--text); font-size: 13px; padding: 9px 12px; border-radius: 9px; outline: none; }
  .path-input { max-width: 310px; }
  .telemetry-row { align-items: flex-start; }
  .telemetry-path-row { align-items: center; }
  .settings-status { margin: 8px 0 0; color: var(--accent-text); font-size: 12px; font-weight: 700; }
  .switch { position: relative; width: 46px; height: 26px; flex-shrink: 0; }
  .switch input { position: absolute; opacity: 0; inset: 0; margin: 0; }
  .switch span { position: absolute; inset: 0; border-radius: 999px; background: var(--border); transition: background 0.15s; }
  .switch span::after { content: ""; position: absolute; width: 20px; height: 20px; left: 3px; top: 3px; border-radius: 50%; background: var(--bg); box-shadow: 0 1px 4px rgba(0, 0, 0, 0.2); transition: transform 0.15s; }
  .switch input:checked + span { background: var(--accent); }
  .switch input:checked + span::after { transform: translateX(20px); }
  .settings-modal-footer { flex-shrink: 0; padding: 12px 20px; border-top: 1px solid var(--border); background: var(--sidebar-bg); font-size: 12.3px; color: var(--text-secondary); }
  .consent-backdrop { z-index: 100; }
  .consent-modal { width: 460px; max-width: 100%; background: var(--bg); border: 1px solid var(--border); border-radius: 16px; overflow: hidden; box-shadow: var(--shadow); }
  .consent-body { padding: 20px; }
  .consent-body p { margin: 0 0 10px; line-height: 1.65; font-size: 14px; }
  .consent-actions { display: flex; justify-content: flex-end; gap: 8px; padding: 0 20px 20px; }
  .consent-actions .settings-action-btn { margin-top: 0; }

  @media (max-width: 900px) {
    .hamburger-btn { display: flex; }
    .sidebar { position: absolute; top: 0; bottom: 0; left: 0; z-index: 40; transform: translateX(-100%); transition: transform 0.2s ease; box-shadow: var(--shadow); }
    .sidebar-open .sidebar { transform: translateX(0); }
    .mode-tabs { margin: 0; }
    .mode-tab { padding: 6px 9px; font-size: 12.5px; }
    .suggestion-grid { grid-template-columns: 1fr; }
    .settings-modal-body { flex-direction: column; overflow-y: auto; }
    .settings-nav { width: 100%; flex-direction: row; overflow-x: auto; border-right: none; border-bottom: 1px solid var(--border); }
    .telemetry-path-row { align-items: flex-start; flex-direction: column; }
    .path-input { max-width: none; }
  }
  @media (max-width: 620px) {
    .app-header { padding: 0 8px; gap: 6px; }
    .engine-picker span:not(.chevron):not(.engine-dot) { display: none; }
    .bubble { max-width: 88%; }
  }
</style>
