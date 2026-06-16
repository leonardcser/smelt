import { attr, escapeHtml, fmtBytes, fmtCost, fmtNumber, icon, modeBadge, relativeTime, shortText } from "./format.js";
import { highlightJsonBlocks, highlightedJson } from "./json_view.js";
import { renderConversation } from "./render_conversation.js";
import { renderOverview } from "./render_overview.js";
import { renderRequestPanel, renderRequests, renderResponseRaw } from "./render_requests.js";

const sidebar = document.getElementById("sidebar");
const detail = document.getElementById("detail");
const state = {
  sessions: [],
  selectedId: null,
  session: null,
  requests: [],
  page: "overview",
  search: "",
  group: "project",
  sort: "updated",
  status: "all",
  conversationLimit: 100,
  sidebarLimit: 200,
};

const sessionCache = new Map();
const sessionInflight = new Map();
const sessionSearchText = new WeakMap();
let selectionToken = 0;
let sidebarRenderTimer = 0;

window.inspectorRequestQuery = "";
window.inspectorRequestStatus = "all";

init();

async function init() {
  wireEvents();
  await loadSessions();
  const id = new URLSearchParams(location.search).get("session") || state.sessions[0]?.id;
  if (id) selectSession(id);
}

function wireEvents() {
  sidebar.addEventListener("input", (event) => {
    if (event.target.id === "session-search") {
      state.search = event.target.value;
      state.sidebarLimit = 200;
      scheduleSidebarRender("session-search");
    }
  });
  sidebar.addEventListener("change", (event) => {
    if (event.target.id === "session-group") state.group = event.target.value;
    if (event.target.id === "session-sort") state.sort = event.target.value;
    if (event.target.id === "session-status") state.status = event.target.value;
    state.sidebarLimit = 200;
    renderSidebar();
  });
  sidebar.addEventListener("click", (event) => {
    const load = event.target.closest("[data-load-sessions]");
    if (load) {
      state.sidebarLimit += 200;
      renderSidebar();
      return;
    }
    const item = event.target.closest("[data-session-id]");
    if (item) selectSession(item.dataset.sessionId);
  });
  sidebar.addEventListener("pointerover", (event) => {
    const item = event.target.closest("[data-session-id]");
    if (item) prefetchSession(item.dataset.sessionId);
  });
  sidebar.addEventListener("focusin", (event) => {
    const item = event.target.closest("[data-session-id]");
    if (item) prefetchSession(item.dataset.sessionId);
  });
  detail.addEventListener("click", async (event) => {
    const tab = event.target.closest("[data-page]");
    if (tab) {
      state.page = tab.dataset.page;
      renderDetail();
      return;
    }
    const requestTab = event.target.closest("[data-request-tab]");
    if (requestTab) {
      const panel = document.getElementById(requestTab.dataset.requestTab);
      if (!panel) return;
      const article = requestTab.closest(".request");
      article.querySelectorAll(".tab").forEach((el) => el.classList.toggle("active", el === requestTab));
      article.querySelectorAll(".request-body").forEach((el) => { el.hidden = el !== panel; });
      if (!panel.dataset.rendered) {
        const index = Number(requestTab.dataset.requestIndex);
        const entry = state.requests[index];
        panel.innerHTML = renderRequestPanel(requestTab.dataset.requestPanel, entry, index);
        panel.dataset.rendered = "true";
      }
      await highlightJsonBlocks(panel);
      return;
    }
    const loadConversation = event.target.closest("[data-load-conversation]");
    if (loadConversation) {
      state.conversationLimit += 100;
      renderDetail();
      return;
    }
    const copy = event.target.closest("[data-copy]");
    if (copy) {
      await navigator.clipboard?.writeText(copy.dataset.copy || "");
      copy.innerHTML = `${icon("check")}copied`;
      setTimeout(() => { copy.innerHTML = `${icon("copy")}copy`; }, 900);
    }
  });
  detail.addEventListener("input", (event) => {
    if (event.target.id === "request-search") {
      window.inspectorRequestQuery = event.target.value;
      renderDetail();
      const input = document.getElementById("request-search");
      input?.focus();
      input?.setSelectionRange(input.value.length, input.value.length);
    }
  });
  detail.addEventListener("change", (event) => {
    if (event.target.id === "request-status") {
      window.inspectorRequestStatus = event.target.value;
      renderDetail();
    }
  });
  detail.addEventListener("toggle", handleLazyToggle, true);
}

async function loadSessions() {
  try {
    const response = await fetch("/api/sessions");
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    state.sessions = await response.json();
    state.sessions.forEach(indexSessionSearchText);
    renderSidebar();
    state.sessions.slice(0, 3).forEach((session) => prefetchSession(session.id));
  } catch (error) {
    sidebar.innerHTML = `<div class="empty">Failed to load sessions: ${escapeHtml(error.message)}</div>`;
  }
}

async function selectSession(id) {
  const token = ++selectionToken;
  state.selectedId = id;
  state.conversationLimit = 100;
  updateSidebarActive();
  history.replaceState(null, "", `?session=${encodeURIComponent(id)}`);

  const cached = sessionCache.get(id);
  if (cached) {
    applySessionData(cached);
    return;
  }

  state.session = null;
  state.requests = [];
  detail.innerHTML = `<div class="empty">Loading session…</div>`;

  try {
    const data = await fetchSessionData(id);
    if (token !== selectionToken || state.selectedId !== id) return;
    applySessionData(data);
  } catch (error) {
    if (token !== selectionToken || state.selectedId !== id) return;
    detail.innerHTML = `<div class="empty">Failed to load session: ${escapeHtml(error.message)}</div>`;
  }
}

function applySessionData(data) {
  state.session = data.session;
  state.requests = data.requests;
  renderDetail();
}

function prefetchSession(id) {
  if (!id || sessionCache.has(id) || sessionInflight.has(id)) return;
  fetchSessionData(id).catch(() => {});
}

async function fetchSessionData(id) {
  const cached = sessionCache.get(id);
  if (cached) return cached;
  const inflight = sessionInflight.get(id);
  if (inflight) return inflight;

  const promise = Promise.all([
    fetch(`/api/sessions/${encodeURIComponent(id)}`),
    fetch(`/api/sessions/${encodeURIComponent(id)}/requests`),
    fetch(`/api/sessions/${encodeURIComponent(id)}/summary`),
  ]).then(async ([sessionResponse, requestResponse, summaryResponse]) => {
    if (!sessionResponse.ok) throw new Error(`session HTTP ${sessionResponse.status}`);
    const session = await sessionResponse.json();
    const requests = requestResponse.ok ? await requestResponse.json() : [];
    if (summaryResponse.ok) Object.assign(session, await summaryResponse.json());
    const data = { session, requests };
    sessionCache.set(id, data);
    return data;
  }).finally(() => {
    sessionInflight.delete(id);
  });

  sessionInflight.set(id, promise);
  return promise;
}

function scheduleSidebarRender(focusId = null) {
  if (sidebarRenderTimer) clearTimeout(sidebarRenderTimer);
  sidebarRenderTimer = setTimeout(() => {
    sidebarRenderTimer = 0;
    renderSidebar();
    if (focusId) {
      const input = document.getElementById(focusId);
      input?.focus();
      input?.setSelectionRange(input.value.length, input.value.length);
    }
  }, 35);
}

function indexSessionSearchText(session) {
  const text = [
    session.id,
    session.title,
    session.slug,
    session.first_user_message,
    session.cwd,
    session.model,
    session.mode,
    session.project,
    session.path_group,
  ].filter(Boolean).join(" ").toLowerCase();
  sessionSearchText.set(session, text);
  return text;
}

function getSessionSearchText(session) {
  return sessionSearchText.get(session) || indexSessionSearchText(session);
}

function renderSidebar() {
  const sessions = filteredSessions();
  const visible = sessions.slice(0, state.sidebarLimit);
  const remaining = sessions.length - visible.length;
  sidebar.innerHTML = `
    <div class="sidebar-tools">
      <label class="search-field">${icon("search")}<input type="search" id="session-search" placeholder="Search sessions…" value="${attr(state.search)}"></label>
      <div class="controls">
        <select id="session-status">
          ${option("all", "All", state.status)}
          ${option("errors", "Errors", state.status)}
          ${option("active", "Active", state.status)}
        </select>
        <select id="session-group">
          ${option("project", "Project", state.group)}
          ${option("mode", "Mode", state.group)}
          ${option("none", "No group", state.group)}
        </select>
        <select id="session-sort">
          ${option("updated", "Updated", state.sort)}
          ${option("requests", "Requests", state.sort)}
          ${option("cost", "Cost", state.sort)}
        </select>
        <span class="sidebar-count">${fmtNumber(visible.length)} / ${fmtNumber(sessions.length)}</span>
      </div>
    </div>
    ${renderGroups(visible)}
    ${remaining > 0 ? `<button class="load-more" data-load-sessions>${fmtNumber(remaining)} more sessions — show next 200</button>` : ""}
  `;
}

function option(value, label, selected) {
  return `<option value="${value}" ${value === selected ? "selected" : ""}>${escapeHtml(label)}</option>`;
}

function filteredSessions() {
  const q = state.search.toLowerCase().trim();
  return [...state.sessions]
    .filter((session) => {
      const stats = session.request_stats || {};
      if (state.status === "errors" && !stats.error_count) return false;
      if (state.status === "active" && !stats.request_count) return false;
      if (!q) return true;
      return getSessionSearchText(session).includes(q);
    })
    .sort((a, b) => {
      if (state.sort === "requests") return (b.request_stats?.request_count || 0) - (a.request_stats?.request_count || 0);
      if (state.sort === "cost") return (b.request_stats?.total_cost_usd || 0) - (a.request_stats?.total_cost_usd || 0);
      return (b.updated_at_ms || 0) - (a.updated_at_ms || 0);
    });
}

function renderGroups(sessions) {
  if (!sessions.length) return `<div class="empty">No sessions match.</div>`;
  if (state.group === "none") return sessions.map(renderSessionItem).join("");
  const groups = new Map();
  for (const session of sessions) {
    const key = state.group === "mode" ? (session.mode || "normal") : (session.project || projectName(session.cwd) || "unknown project");
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(session);
  }
  return [...groups.entries()].map(([key, values]) => `<div class="group-title">${escapeHtml(key)} · ${fmtNumber(values.length)}</div>${values.map(renderSessionItem).join("")}`).join("");
}

function renderSessionItem(session) {
  const stats = session.request_stats || {};
  const active = session.id === state.selectedId ? "active" : "";
  const title = session.title || session.slug || shortText(session.first_user_message, 80) || session.id;
  const preview = session.first_user_message || session.cwd || "No prompt preview";
  return `<div class="session-item ${active}" data-session-id="${attr(session.id)}">
    <div class="title">${icon("folder")}${escapeHtml(title)}</div>
    <div class="preview">${escapeHtml(preview)}</div>
    <div class="session-meta">
      ${modeBadge(session.mode)}
      <span>${escapeHtml(relativeTime(session.updated_at_ms))}</span><span class="dot">·</span>
      <span>${icon("server")}${fmtNumber(stats.request_count || 0)} req</span>
      ${session.text_bytes ? `<span class="badge">${icon("database")}${escapeHtml(fmtBytes(session.text_bytes))}</span>` : ""}
      ${stats.error_count ? `<span class="badge err">${fmtNumber(stats.error_count)} err</span>` : ""}
      ${stats.total_cost_usd ? `<span class="badge cache">${escapeHtml(fmtCost(stats.total_cost_usd))}</span>` : ""}
    </div>
  </div>`;
}

function updateSidebarActive() {
  sidebar.querySelectorAll("[data-session-id]").forEach((item) => {
    item.classList.toggle("active", item.dataset.sessionId === state.selectedId);
  });
}

async function handleLazyToggle(event) {
  const node = event.target;
  if (!node.open) return;

  if (node.matches?.("[data-response-raw-index]")) {
    const body = node.querySelector(".tool-inner");
    if (!body || body.dataset.rendered) return;
    const index = Number(node.dataset.responseRawIndex);
    body.innerHTML = renderResponseRaw(state.requests[index]);
    body.dataset.rendered = "true";
    await highlightJsonBlocks(body);
  }
}

function renderDetail() {
  if (!state.session) return;
  detail.innerHTML = `
    <div class="page-tabs tabs">
      ${pageTab("overview", "Overview")}
      ${pageTab("conversation", "Conversation")}
      ${pageTab("requests", `Requests (${fmtNumber(state.requests.length)})`)}
      ${pageTab("raw", "Raw")}
    </div>
    ${renderPage()}
  `;
  highlightJsonBlocks(detail);
}

function pageTab(page, label) {
  const icons = { overview: "dashboard", conversation: "comment-discussion", requests: "pulse", raw: "json" };
  return `<button class="tab ${state.page === page ? "active" : ""}" data-page="${page}">${icon(icons[page] || "circle-outline")}${escapeHtml(label)}</button>`;
}

function renderPage() {
  if (state.page === "conversation") return renderConversation(state.session, state.conversationLimit);
  if (state.page === "requests") return renderRequests(state.requests);
  if (state.page === "raw") return `<section class="card"><h3>Session JSON</h3>${highlightedJson(state.session)}</section><section class="card"><h3>Request log JSON</h3>${highlightedJson(state.requests)}</section>`;
  return renderOverview(state.session, state.requests);
}

function projectName(cwd) {
  if (!cwd) return null;
  return String(cwd).split(/[\\/]/).filter(Boolean).pop();
}
