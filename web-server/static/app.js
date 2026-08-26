const state = {
  csrf: "",
  account: null,
  conversations: [],
  currentConversation: null,
  messages: [],
  datasets: [],
  jobs: [],
  activeRun: null,
  activeAbort: null,
  pendingPrompt: null,
  currentView: "query"
};

const el = function (id) { return document.getElementById(id); };
const mutationMethods = new Set(["POST", "PUT", "PATCH", "DELETE"]);
const chatEmptyTemplate = el("chat-empty").cloneNode(true);
const markdownRenderer = window.markdownit ? window.markdownit({
  html: false,
  breaks: true,
  linkify: true,
  typographer: false
}) : null;

if (markdownRenderer) {
  markdownRenderer.validateLink = function (url) {
    return /^https?:\/\//i.test(String(url || "").trim());
  };
  markdownRenderer.renderer.rules.image = function (tokens, index) {
    return markdownRenderer.utils.escapeHtml(tokens[index].content || "");
  };
  const defaultLinkOpen = markdownRenderer.renderer.rules.link_open || function (tokens, index, options, environment, renderer) {
    return renderer.renderToken(tokens, index, options);
  };
  markdownRenderer.renderer.rules.link_open = function (tokens, index, options, environment, renderer) {
    tokens[index].attrSet("target", "_blank");
    tokens[index].attrSet("rel", "noopener noreferrer");
    return defaultLinkOpen(tokens, index, options, environment, renderer);
  };
}

export function renderMarkdown(content) {
  const source = String(content || "");
  if (markdownRenderer) return markdownRenderer.render(source);
  const node = document.createElement("div");
  node.textContent = source;
  return node.innerHTML.replace(/\n/g, "<br>");
}

async function api(path, options) {
  const config = Object.assign({}, options || {});
  config.method = (config.method || "GET").toUpperCase();
  config.credentials = "same-origin";
  config.headers = Object.assign({ "Accept": "application/json" }, config.headers || {});
  if (config.body && typeof config.body !== "string") {
    config.headers["Content-Type"] = "application/json";
    config.body = JSON.stringify(config.body);
  }
  if (mutationMethods.has(config.method) && state.csrf) config.headers["X-CSRF-Token"] = state.csrf;
  const response = await fetch(path, config);
  const type = response.headers.get("content-type") || "";
  const body = type.includes("application/json") ? await response.json() : await response.text();
  if (!response.ok) {
    if (response.status === 401 && !path.startsWith("/api/auth/login")) showLogin();
    const details = body && body.error ? body.error : body;
    const error = new Error(details && details.message ? details.message : "请求失败（" + response.status + "）");
    error.code = details && details.code;
    error.status = response.status;
    error.total = body && body.total;
    throw error;
  }
  return body;
}

function showLogin() {
  if (state.activeAbort) state.activeAbort.abort();
  state.csrf = "";
  state.account = null;
  state.conversations = [];
  state.currentConversation = null;
  state.messages = [];
  state.datasets = [];
  state.jobs = [];
  state.activeRun = null;
  state.activeAbort = null;
  state.pendingPrompt = null;
  state.currentView = "query";
  document.querySelectorAll(".view").forEach(function (node) { node.classList.add("hidden"); });
  el("view-query").classList.remove("hidden");
  document.querySelectorAll("#nav [data-view]").forEach(function (node) {
    node.classList.toggle("active", node.dataset.view === "query");
  });
  el("loading").classList.add("hidden");
  el("workspace").classList.add("hidden");
  el("login").classList.remove("hidden");
}

function showWorkspace() {
  el("loading").classList.add("hidden");
  el("login").classList.add("hidden");
  el("workspace").classList.remove("hidden");
}

function applyRole() {
  const isAdmin = Boolean(state.account && state.account.role === "admin");
  document.querySelectorAll(".admin-only").forEach(function (node) {
    node.classList.toggle("hidden", !isAdmin);
  });
  el("platform-user").textContent = "平台账号：" + (state.account ? state.account.email : "—");
  if (!isAdmin && state.currentView === "users") openView("settings");
}

function toast(message, isError) {
  const node = document.createElement("div");
  node.className = "toast" + (isError ? " error" : "");
  node.textContent = message;
  el("toast-region").append(node);
  window.setTimeout(function () { node.remove(); }, 4200);
}

function setBusy(button, busy, busyText) {
  if (!button) return;
  if (busy) {
    button.dataset.label = button.textContent;
    button.textContent = busyText || "处理中…";
    button.disabled = true;
  } else {
    button.textContent = button.dataset.label || button.textContent;
    button.disabled = false;
  }
}

function formatTime(timestamp) {
  if (!timestamp) return "—";
  return new Intl.DateTimeFormat("zh-CN", {
    year: "numeric", month: "2-digit", day: "2-digit",
    hour: "2-digit", minute: "2-digit"
  }).format(new Date(timestamp * 1000));
}

function formatSize(bytes) {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1048576) return (bytes / 1024).toFixed(1) + " KB";
  return (bytes / 1048576).toFixed(1) + " MB";
}

function badge(text, kind) {
  const node = document.createElement("span");
  node.className = "badge " + (kind || "neutral");
  node.textContent = text;
  return node;
}

async function boot() {
  bindEvents();
  try {
    const result = await api("/api/auth/me");
    state.csrf = result.csrf;
    state.account = result.admin;
    applyRole();
    showWorkspace();
    await Promise.allSettled([loadConversations(), refreshDeveloperStatus()]);
  } catch (_) {
    showLogin();
  }
}

function bindEvents() {
  el("login-form").addEventListener("submit", login);
  el("logout").addEventListener("click", logout);
  el("nav").addEventListener("click", function (event) {
    const button = event.target.closest("[data-view]");
    if (button) openView(button.dataset.view);
  });
  el("menu-button").addEventListener("click", function () {
    document.querySelector(".sidebar").classList.toggle("open");
  });
  el("new-conversation").addEventListener("click", createConversation);
  el("delete-conversation").addEventListener("click", deleteConversation);
  el("composer").addEventListener("submit", function (event) {
    event.preventDefault();
    const content = el("prompt").value.trim();
    if (content) sendChat(content, false, false);
  });
  el("prompt").addEventListener("keydown", function (event) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      el("composer").requestSubmit();
    }
  });
  document.querySelectorAll(".prompt-grid button").forEach(function (button) {
    button.addEventListener("click", function () {
      el("prompt").value = button.textContent;
      el("composer").requestSubmit();
    });
  });
  el("stop-generation").addEventListener("click", stopGeneration);
  el("large-cancel").addEventListener("click", clearLargeConfirmation);
  el("large-go").addEventListener("click", function () {
    if (state.pendingPrompt) sendChat(state.pendingPrompt, true, true);
  });
  el("new-job").addEventListener("click", function () { openJobDialog(); });
  el("close-job").addEventListener("click", closeJobDialog);
  el("cancel-job").addEventListener("click", closeJobDialog);
  el("job-form").addEventListener("submit", saveJob);
  ["schedule-type", "schedule-day", "schedule-time", "schedule-cron", "job-timezone"].forEach(function (id) {
    el(id).addEventListener("input", scheduleChanged);
  });
  el("refresh-runs").addEventListener("click", loadRuns);
  el("refresh-files").addEventListener("click", loadFiles);
  el("developer-form").addEventListener("submit", developerPasswordLogin);
  el("create-pairing").addEventListener("click", createDeveloperPairing);
  el("copy-pairing").addEventListener("click", copyDeveloperPairing);
  el("cookie-login").addEventListener("click", developerCookieLogin);
  el("developer-logout").addEventListener("click", developerLogout);
  el("model-form").addEventListener("submit", function (event) { saveModel(event, false); });
  el("test-model").addEventListener("click", function () { saveModel(null, true); });
  el("smtp-form").addEventListener("submit", function (event) { saveSmtp(event, false); });
  el("test-smtp").addEventListener("click", function () { saveSmtp(null, true); });
  el("smtp-preset").addEventListener("change", applySmtpPreset);
  el("refresh-users").addEventListener("click", loadUsers);
  el("generate-user-password").addEventListener("click", generateUserPassword);
  el("copy-user-password").addEventListener("click", copyUserPassword);
  el("user-form").addEventListener("submit", createUser);
}

async function login(event) {
  event.preventDefault();
  const button = event.submitter;
  el("login-error").classList.add("hidden");
  setBusy(button, true, "正在登录…");
  try {
    const result = await api("/api/auth/login", {
      method: "POST",
      body: { email: el("login-email").value.trim(), password: el("login-password").value }
    });
    state.csrf = result.csrf;
    state.account = result.admin;
    applyRole();
    el("login-password").value = "";
    showWorkspace();
    await Promise.allSettled([loadConversations(), refreshDeveloperStatus()]);
  } catch (error) {
    el("login-error").textContent = error.message;
    el("login-error").classList.remove("hidden");
  } finally {
    setBusy(button, false);
  }
}

async function logout() {
  if (state.activeRun) {
    try { await api("/api/chat/runs/" + encodeURIComponent(state.activeRun) + "/cancel", { method: "POST" }); } catch (_) {}
    if (state.activeAbort) state.activeAbort.abort();
  }
  try { await api("/api/auth/logout", { method: "POST" }); } catch (_) {}
  showLogin();
}

async function openView(name) {
  if (name === "users" && (!state.account || state.account.role !== "admin")) {
    toast("只有平台管理员可以管理用户", true);
    return;
  }
  state.currentView = name;
  document.querySelectorAll(".view").forEach(function (node) { node.classList.add("hidden"); });
  el("view-" + name).classList.remove("hidden");
  document.querySelectorAll("#nav [data-view]").forEach(function (node) {
    node.classList.toggle("active", node.dataset.view === name);
  });
  document.querySelector(".sidebar").classList.remove("open");
  if (name === "jobs") await loadJobs();
  if (name === "runs") await loadRuns();
  if (name === "files") await loadFiles();
  if (name === "settings") await loadSettings();
  if (name === "users") await loadUsers();
}

async function loadConversations(selectId) {
  const result = await api("/api/conversations");
  state.conversations = result.items || [];
  renderConversations();
  const wanted = selectId || (state.currentConversation && state.currentConversation.id) || (state.conversations[0] && state.conversations[0].id);
  if (wanted) await selectConversation(wanted);
  else renderMessages();
}

function renderConversations() {
  const list = el("conversation-list");
  list.replaceChildren();
  if (!state.conversations.length) {
    const empty = document.createElement("p");
    empty.className = "muted";
    empty.textContent = "还没有对话";
    list.append(empty);
    return;
  }
  state.conversations.forEach(function (conversation) {
    const button = document.createElement("button");
    button.className = "conversation-item" + (state.currentConversation && state.currentConversation.id === conversation.id ? " active" : "");
    const title = document.createElement("strong");
    title.textContent = conversation.title;
    const time = document.createElement("span");
    time.textContent = formatTime(conversation.updated_at);
    button.append(title, time);
    button.addEventListener("click", function () { selectConversation(conversation.id); });
    list.append(button);
  });
}

async function createConversation() {
  try {
    const result = await api("/api/conversations", { method: "POST", body: { title: "新对话" } });
    await loadConversations(result.item.id);
    el("prompt").focus();
  } catch (error) { toast(error.message, true); }
}

async function selectConversation(id) {
  state.currentConversation = state.conversations.find(function (item) { return item.id === id; }) || null;
  renderConversations();
  if (!state.currentConversation) return;
  const result = await api("/api/conversations/" + encodeURIComponent(id) + "/messages");
  state.messages = result.items || [];
  state.datasets = result.datasets || [];
  el("conversation-title").textContent = state.currentConversation.title;
  el("delete-conversation").classList.remove("hidden");
  renderMessages();
}

async function deleteConversation() {
  if (!state.currentConversation || !window.confirm("删除此对话及其消息？已生成的文件会保留到到期时间。")) return;
  try {
    await api("/api/conversations/" + encodeURIComponent(state.currentConversation.id), { method: "DELETE" });
    state.currentConversation = null;
    state.messages = [];
    state.datasets = [];
    await loadConversations();
  } catch (error) { toast(error.message, true); }
}

function renderMessages() {
  const area = el("messages");
  area.replaceChildren();
  if (!state.messages.length) {
    const empty = chatEmptyTemplate.cloneNode(true);
    empty.id = "chat-empty-rendered";
    empty.querySelectorAll(".prompt-grid button").forEach(function (button) {
      button.addEventListener("click", function () {
        el("prompt").value = button.textContent;
        el("composer").requestSubmit();
      });
    });
    area.append(empty);
    return;
  }
  state.messages.forEach(function (message) { appendMessage(message.role, message.content, message.tool_summary); });
  appendExportRow();
  area.scrollTop = area.scrollHeight;
}

function appendMessage(role, content, toolSummary) {
  const area = el("messages");
  const node = document.createElement("div");
  node.className = "message " + role;
  const label = document.createElement("div");
  label.className = "role";
  label.textContent = role === "user" ? "你" : "AI 分析";
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  if (role === "assistant") {
    bubble.classList.add("markdown-body");
    bubble.innerHTML = renderMarkdown(content);
  } else {
    bubble.textContent = content;
  }
  node.append(label);
  if (toolSummary) {
    const tools = document.createElement("div");
    tools.className = "tool-summary";
    try {
      const values = typeof toolSummary === "string" ? JSON.parse(toolSummary) : toolSummary;
      tools.textContent = "读取记录：" + (Array.isArray(values) ? values.join("；") : String(values));
    } catch (_) {
      tools.textContent = String(toolSummary);
    }
    node.append(tools);
  }
  node.append(bubble);
  area.append(node);
  return node;
}

function appendExportRow() {
  if (!state.currentConversation || !state.messages.some(function (item) { return item.role === "assistant"; })) return;
  const area = el("messages");
  const row = document.createElement("div");
  row.className = "export-row";
  [["docx", "下载 Word"], ["md", "下载 Markdown"]].forEach(function (item) {
    row.append(exportButton(item[1], item[0], null));
  });
  state.datasets.slice(0, 4).forEach(function (dataset) {
    row.append(exportButton("下载" + (dataset.kind === "comments" ? "评论" : "反馈") + " CSV（" + dataset.total + " 条）", "csv", dataset.id));
  });
  area.append(row);
}

function exportButton(label, format, datasetId) {
  const button = document.createElement("button");
  button.className = "export-button";
  button.textContent = label;
  button.addEventListener("click", function () { exportArtifact(format, datasetId, button); });
  return button;
}

async function exportArtifact(format, datasetId, button) {
  if (!state.currentConversation) return;
  setBusy(button, true, "生成中…");
  try {
    const result = await api("/api/artifacts/export", {
      method: "POST",
      body: { conversation_id: state.currentConversation.id, format: format, dataset_id: datasetId }
    });
    toast("文件已生成");
    window.location.assign(result.artifact.download_url);
  } catch (error) { toast(error.message, true); }
  finally { setBusy(button, false); }
}

async function sendChat(content, allowLarge, resume) {
  if (state.activeRun) return;
  if (!state.currentConversation) {
    try {
      const result = await api("/api/conversations", { method: "POST", body: { title: "新对话" } });
      await loadConversations(result.item.id);
    } catch (error) { toast(error.message, true); return; }
  }
  clearLargeConfirmation();
  state.pendingPrompt = content;
  if (!resume) {
    state.messages.push({ role: "user", content: content, tool_summary: null });
    renderMessages();
    el("prompt").value = "";
  }
  const assistantCountBefore = state.messages.filter(function (message) { return message.role === "assistant"; }).length;
  const runId = crypto.randomUUID();
  const abortController = new AbortController();
  state.activeRun = runId;
  state.activeAbort = abortController;
  el("stop-generation").classList.remove("hidden");
  el("send-message").disabled = true;
  el("chat-status").textContent = "正在连接模型…";
  try {
    const response = await fetch("/api/conversations/" + encodeURIComponent(state.currentConversation.id) + "/messages", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", "Accept": "text/event-stream", "X-CSRF-Token": state.csrf },
      body: JSON.stringify({ run_id: runId, content: content, allow_large: allowLarge, resume: resume }),
      signal: abortController.signal
    });
    if (!response.ok) {
      const body = await response.json().catch(function () { return {}; });
      const error = new Error((body.error && body.error.message) || body.message || "无法开始分析");
      error.status = response.status;
      throw error;
    }
    const terminal = await consumeSse(response);
    if (!terminal.done && !terminal.error) {
      await waitForBackgroundChat(runId, state.currentConversation.id, assistantCountBefore, abortController.signal);
    }
  } catch (error) {
    if (error.name === "AbortError") {
      el("chat-status").textContent = "已停止";
    } else if (!error.status) {
      try {
        await waitForBackgroundChat(runId, state.currentConversation.id, assistantCountBefore, abortController.signal);
      } catch (reconnectError) {
        if (reconnectError.name === "AbortError") el("chat-status").textContent = "已停止";
        else {
          toast(reconnectError.message, true);
          el("chat-status").textContent = "分析失败";
        }
      }
    } else {
      toast(error.message, true);
      el("chat-status").textContent = "分析失败";
    }
  } finally {
    if (state.activeRun === runId) {
      state.activeRun = null;
      state.activeAbort = null;
      el("stop-generation").classList.add("hidden");
      el("send-message").disabled = false;
    }
  }
}

async function consumeSse(response) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const terminal = { done: false, error: false };
  while (true) {
    const value = await reader.read();
    if (value.done) break;
    buffer += decoder.decode(value.value, { stream: true }).replace(/\r/g, "");
    let boundary;
    while ((boundary = buffer.indexOf("\n\n")) >= 0) {
      const block = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      let eventName = "message";
      const data = [];
      block.split("\n").forEach(function (line) {
        if (line.startsWith("event:")) eventName = line.slice(6).trim();
        if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
      });
      if (data.length) {
        handleSseEvent(eventName, JSON.parse(data.join("\n")));
        if (eventName === "done") terminal.done = true;
        if (eventName === "error") terminal.error = true;
      }
    }
  }
  return terminal;
}

async function waitForBackgroundChat(runId, conversationId, assistantCountBefore, signal) {
  el("chat-status").textContent = "页面连接中断，正在重连后台会话…";
  while (true) {
    if (signal.aborted) throw new DOMException("已停止", "AbortError");
    try {
      const result = await api("/api/conversations/" + encodeURIComponent(conversationId) + "/messages");
      const messages = result.items || [];
      const assistantCount = messages.filter(function (message) { return message.role === "assistant"; }).length;
      if (assistantCount > assistantCountBefore) {
        state.messages = messages;
        state.datasets = result.datasets || [];
        renderMessages();
        state.pendingPrompt = null;
        el("chat-status").textContent = "分析完成";
        await loadConversations(conversationId);
        return;
      }
      const run = await api("/api/chat/runs/" + encodeURIComponent(runId));
      if (!run.active) {
        const error = new Error("后台会话已结束，但没有生成结果");
        error.status = 409;
        throw error;
      }
      el("chat-status").textContent = "已重连后台会话，正在继续运行…";
    } catch (error) {
      if (error.status) throw error;
      el("chat-status").textContent = "网络仍未恢复，将继续尝试重连…";
    }
    await new Promise(function (resolve) { window.setTimeout(resolve, 5000); });
  }
}

function handleSseEvent(name, data) {
  if (name === "status") {
    el("chat-status").textContent = data.message || "正在分析…";
  } else if (name === "tool") {
    state.datasets = (data.dataset_ids || []).map(function (id) {
      return { id: id, kind: "data", total: 0 };
    });
  } else if (name === "text") {
    state.messages.push({ role: "assistant", content: data.content || "", tool_summary: null });
    renderMessages();
  } else if (name === "artifact") {
    toast("已生成文件：" + data.filename);
  } else if (name === "done") {
    el("chat-status").textContent = "分析完成";
    state.pendingPrompt = null;
    loadConversations(state.currentConversation.id).catch(function () {});
  } else if (name === "error") {
    if (data.code === "LARGE_CONFIRMATION_REQUIRED") {
      el("large-confirm-text").textContent = "预计需要分批分析约 " + (data.total || "很多") + " 条记录，模型调用可能超过 10 次。是否继续？";
      el("large-confirm").classList.remove("hidden");
      el("chat-status").textContent = "等待确认";
    } else {
      toast(data.message || "分析失败", true);
      el("chat-status").textContent = "分析失败";
    }
  }
}

async function stopGeneration() {
  if (!state.activeRun) return;
  try {
    await api("/api/chat/runs/" + encodeURIComponent(state.activeRun) + "/cancel", { method: "POST" });
    if (state.activeAbort) state.activeAbort.abort();
    el("chat-status").textContent = "正在停止…";
  } catch (error) { toast(error.message, true); }
}

function clearLargeConfirmation() {
  el("large-confirm").classList.add("hidden");
}

async function loadJobs() {
  try {
    const result = await api("/api/jobs");
    state.jobs = result.items || [];
    renderJobs();
  } catch (error) { toast(error.message, true); }
}

function renderJobs() {
  const list = el("jobs-list");
  list.replaceChildren();
  if (!state.jobs.length) {
    const node = document.createElement("div");
    node.className = "empty-card";
    node.textContent = "还没有 AI 工作。创建后，它会按指定时间独立运行。";
    list.append(node);
    return;
  }
  state.jobs.forEach(function (job) {
    const card = document.createElement("article");
    card.className = "job-card";
    const info = document.createElement("div");
    const title = document.createElement("h3");
    title.textContent = job.name;
    const prompt = document.createElement("p");
    prompt.textContent = job.prompt;
    const meta = document.createElement("div");
    meta.className = "meta-row";
    [
      job.enabled ? "下次：" + formatTime(job.next_run_at) : "已停用",
      "时区：" + job.timezone,
      job.formats.length ? "文件：" + job.formats.join(" / ").toUpperCase() : "不自动生成文件",
      job.email_to ? "固定邮件：" + job.email_to : "不发邮件"
    ].forEach(function (text) { const item = document.createElement("span"); item.textContent = text; meta.append(item); });
    info.append(title, prompt, meta);
    const actions = document.createElement("div");
    actions.className = "job-actions";
    actions.append(badge(job.running ? "运行中" : (job.enabled ? "已启用" : "已停用"), job.running ? "warning" : (job.enabled ? "success" : "neutral")));
    const run = actionButton("立即运行", function () { runJob(job, run); });
    const toggle = actionButton(job.enabled ? "停用" : "启用", function () { toggleJob(job, toggle); });
    const edit = actionButton("编辑", function () { openJobDialog(job); });
    const remove = actionButton("删除", function () { removeJob(job); }, true);
    actions.append(run, toggle, edit, remove);
    card.append(info, actions);
    list.append(card);
  });
}

function actionButton(text, handler, danger) {
  const button = document.createElement("button");
  button.className = "secondary" + (danger ? " danger-text" : "");
  button.textContent = text;
  button.addEventListener("click", handler);
  return button;
}

function openJobDialog(job) {
  el("job-form").reset();
  el("job-id").value = job ? job.id : "";
  el("job-dialog-title").textContent = job ? "编辑 AI 工作" : "新建 AI 工作";
  el("job-name").value = job ? job.name : "";
  el("job-prompt").value = job ? job.prompt : "";
  ["comments", "feedback", "account"].forEach(function (name) {
    const value = el("tool-" + name).value;
    el("tool-" + name).checked = job ? job.allowed_tools.includes(value) : name !== "account";
  });
  ["csv", "docx", "md"].forEach(function (format) {
    el("format-" + format).checked = job ? job.formats.includes(format) : false;
  });
  el("schedule-type").value = job ? "cron" : "daily";
  el("schedule-cron").value = job ? job.schedule_value : "";
  el("schedule-time").value = "09:00";
  el("job-timezone").value = job ? job.timezone : "Asia/Shanghai";
  el("job-email").value = job && job.email_to ? job.email_to : "";
  el("job-enabled").checked = job ? job.enabled : true;
  el("job-error").classList.add("hidden");
  scheduleChanged();
  el("job-dialog").showModal();
}

function closeJobDialog() { el("job-dialog").close(); }

function scheduleChanged() {
  const type = el("schedule-type").value;
  const needsDay = type === "weekly" || type === "monthly";
  el("schedule-day-wrap").classList.toggle("hidden", !needsDay);
  el("schedule-time-wrap").classList.toggle("hidden", type === "cron");
  el("schedule-cron-wrap").classList.toggle("hidden", type !== "cron");
  const day = el("schedule-day");
  const oldValue = day.value;
  day.replaceChildren();
  if (type === "weekly") {
    ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"].forEach(function (name, index) {
      const option = document.createElement("option"); option.value = index; option.textContent = name; day.append(option);
    });
  } else if (type === "monthly") {
    for (let index = 1; index <= 28; index++) {
      const option = document.createElement("option"); option.value = index; option.textContent = index + " 日"; day.append(option);
    }
  }
  if (Array.from(day.options).some(function (option) { return option.value === oldValue; })) day.value = oldValue;
  window.clearTimeout(scheduleChanged.timer);
  scheduleChanged.timer = window.setTimeout(previewSchedule, 350);
}

function scheduleExpression() {
  if (el("schedule-type").value === "cron") return el("schedule-cron").value.trim();
  const parts = (el("schedule-time").value || "09:00").split(":");
  const minute = Number(parts[1]);
  const hour = Number(parts[0]);
  if (el("schedule-type").value === "daily") return minute + " " + hour + " * * *";
  if (el("schedule-type").value === "weekly") return minute + " " + hour + " * * " + el("schedule-day").value;
  return minute + " " + hour + " " + el("schedule-day").value + " * *";
}

async function previewSchedule() {
  const expression = scheduleExpression();
  if (!expression) { el("schedule-preview").textContent = "请填写 Cron 表达式"; return; }
  try {
    const result = await api("/api/jobs/preview", {
      method: "POST", body: { expression: expression, timezone: el("job-timezone").value.trim() || "Asia/Shanghai" }
    });
    el("schedule-preview").textContent = result.items.map(formatTime).join("、");
  } catch (error) { el("schedule-preview").textContent = error.message; }
}

function jobPayload() {
  const allowed = ["comments", "feedback", "account"].filter(function (name) { return el("tool-" + name).checked; }).map(function (name) { return el("tool-" + name).value; });
  const formats = ["csv", "docx", "md"].filter(function (name) { return el("format-" + name).checked; });
  return {
    name: el("job-name").value.trim(),
    prompt: el("job-prompt").value.trim(),
    allowed_tools: allowed,
    formats: formats,
    schedule_kind: el("schedule-type").value,
    schedule_value: scheduleExpression(),
    timezone: el("job-timezone").value.trim(),
    enabled: el("job-enabled").checked,
    email_to: el("job-email").value.trim() || null
  };
}

async function saveJob(event) {
  event.preventDefault();
  const id = el("job-id").value;
  const button = event.submitter;
  setBusy(button, true, "保存中…");
  try {
    await api(id ? "/api/jobs/" + encodeURIComponent(id) : "/api/jobs", {
      method: id ? "PUT" : "POST", body: jobPayload()
    });
    closeJobDialog();
    toast("工作已保存");
    await loadJobs();
  } catch (error) {
    el("job-error").textContent = error.message;
    el("job-error").classList.remove("hidden");
  } finally { setBusy(button, false); }
}

async function toggleJob(job, button) {
  setBusy(button, true);
  try {
    const payload = Object.assign({}, job, { enabled: !job.enabled });
    await api("/api/jobs/" + encodeURIComponent(job.id), { method: "PUT", body: payload });
    await loadJobs();
  } catch (error) { toast(error.message, true); }
  finally { setBusy(button, false); }
}

async function runJob(job, button) {
  setBusy(button, true, "已提交");
  try {
    await api("/api/jobs/" + encodeURIComponent(job.id) + "/run", { method: "POST" });
    toast("工作已加入执行队列");
    window.setTimeout(loadJobs, 700);
  } catch (error) { toast(error.message, true); }
  finally { setBusy(button, false); }
}

async function removeJob(job) {
  if (!window.confirm("删除工作“" + job.name + "”？执行记录和已生成文件仍会保留到到期时间。")) return;
  try {
    await api("/api/jobs/" + encodeURIComponent(job.id), { method: "DELETE" });
    toast("工作已删除");
    await loadJobs();
  } catch (error) { toast(error.message, true); }
}

async function loadRuns() {
  try {
    const result = await api("/api/runs");
    renderRuns(result.items || []);
  } catch (error) { toast(error.message, true); }
}

function renderRuns(items) {
  const host = el("runs-list");
  host.replaceChildren();
  if (!items.length) { host.append(emptyTable("还没有执行记录")); return; }
  const table = document.createElement("table");
  const head = document.createElement("thead");
  const headRow = document.createElement("tr");
  ["工作", "状态", "开始时间", "耗时", "工具", "邮件", "结果"].forEach(function (text) { const th = document.createElement("th"); th.textContent = text; headRow.append(th); });
  head.append(headRow); table.append(head);
  const body = document.createElement("tbody");
  items.forEach(function (run) {
    const row = document.createElement("tr");
    addCell(row, run.job_name);
    const statusCell = document.createElement("td");
    const statusMap = { success: ["成功", "success"], completed: ["成功", "success"], failed: ["失败", "error"], running: ["运行中", "warning"], skipped: ["已跳过", "neutral"] };
    const status = statusMap[run.status] || [run.status, "neutral"];
    statusCell.append(badge(status[0], status[1])); row.append(statusCell);
    addCell(row, formatTime(run.started_at || run.scheduled_for));
    addCell(row, run.finished_at && run.started_at ? (run.finished_at - run.started_at) + " 秒" : "—");
    addCell(row, String(run.tool_count || 0) + " 次");
    addCell(row, run.email_status || "—");
    const result = document.createElement("td");
    const summary = document.createElement("div"); summary.className = "summary-cell"; summary.textContent = run.error || run.result || "—"; result.append(summary); row.append(result);
    body.append(row);
  });
  table.append(body); host.append(table);
}

async function loadFiles() {
  try {
    const result = await api("/api/artifacts");
    renderFiles(result.items || []);
  } catch (error) { toast(error.message, true); }
}

function renderFiles(items) {
  const host = el("files-list");
  host.replaceChildren();
  if (!items.length) { host.append(emptyTable("还没有生成文件")); return; }
  const table = document.createElement("table");
  const head = document.createElement("thead"); const headRow = document.createElement("tr");
  ["文件名", "格式", "大小", "生成时间", "到期时间", "操作"].forEach(function (text) { const th = document.createElement("th"); th.textContent = text; headRow.append(th); });
  head.append(headRow); table.append(head);
  const body = document.createElement("tbody");
  items.forEach(function (file) {
    const row = document.createElement("tr");
    const nameCell = document.createElement("td"); const name = document.createElement("span"); name.className = "file-name"; name.textContent = file.filename; nameCell.append(name); row.append(nameCell);
    addCell(row, file.kind.toUpperCase()); addCell(row, formatSize(file.size)); addCell(row, formatTime(file.created_at)); addCell(row, formatTime(file.expires_at));
    const actions = document.createElement("td");
    const download = document.createElement("a"); download.className = "artifact-link"; download.href = file.download_url; download.textContent = "下载";
    const remove = actionButton("删除", async function () {
      if (!window.confirm("删除文件“" + file.filename + "”？")) return;
      try { await api("/api/artifacts/" + encodeURIComponent(file.id), { method: "DELETE" }); await loadFiles(); } catch (error) { toast(error.message, true); }
    }, true);
    actions.append(download, document.createTextNode(" "), remove); row.append(actions); body.append(row);
  });
  table.append(body); host.append(table);
}

function emptyTable(text) {
  const node = document.createElement("div"); node.className = "empty-card"; node.textContent = text; return node;
}

function addCell(row, text) {
  const cell = document.createElement("td"); cell.textContent = text; row.append(cell);
}

async function loadSettings() {
  const tasks = [refreshDeveloperStatus()];
  if (state.account && state.account.role === "admin") tasks.push(loadModel(), loadSmtp());
  await Promise.allSettled(tasks);
}

async function loadUsers() {
  if (!state.account || state.account.role !== "admin") return;
  try {
    const result = await api("/api/admin/users");
    renderUsers(result.items || []);
  } catch (error) {
    toast(error.message, true);
  }
}

function renderUsers(users) {
  const host = el("users-list");
  host.replaceChildren();
  if (!users.length) {
    const empty = document.createElement("div");
    empty.className = "empty-card";
    empty.textContent = "还没有平台账号";
    host.append(empty);
    return;
  }
  const table = document.createElement("table");
  const head = document.createElement("thead");
  const headerRow = document.createElement("tr");
  ["平台账号", "角色", "网易开发者账号", "创建时间"].forEach(function (value) {
    const cell = document.createElement("th");
    cell.textContent = value;
    headerRow.append(cell);
  });
  head.append(headerRow);
  const body = document.createElement("tbody");
  users.forEach(function (user) {
    const row = document.createElement("tr");
    const email = document.createElement("td");
    email.textContent = user.email;
    const role = document.createElement("td");
    role.append(badge(user.role === "admin" ? "管理员" : "普通用户", user.role === "admin" ? "success" : "neutral"));
    const netease = document.createElement("td");
    netease.textContent = user.netease_account || "尚未绑定";
    const created = document.createElement("td");
    created.textContent = formatTime(user.created_at);
    row.append(email, role, netease, created);
    body.append(row);
  });
  table.append(head, body);
  host.append(table);
}

async function createUser(event) {
  event.preventDefault();
  const button = event.submitter;
  setBusy(button, true, "正在创建…");
  try {
    const result = await api("/api/admin/users", {
      method: "POST",
      body: {
        email: el("user-email").value.trim(),
        password: el("user-password").value
      }
    });
    el("user-form").reset();
    toast("平台账号 " + result.user.email + " 已创建");
    await loadUsers();
  } catch (error) {
    toast(error.message, true);
  } finally {
    el("user-password").value = "";
    el("user-password").type = "password";
    setBusy(button, false);
  }
}

function secureRandomIndex(length) {
  if (!Number.isInteger(length) || length < 1 || length > 256) throw new Error("随机字符集无效");
  const values = new Uint8Array(1);
  const limit = Math.floor(256 / length) * length;
  do { crypto.getRandomValues(values); } while (values[0] >= limit);
  return values[0] % length;
}

export function generateStrongPassword() {
  const groups = ["ABCDEFGHJKLMNPQRSTUVWXYZ", "abcdefghijkmnopqrstuvwxyz", "23456789", "!@#$%^&*_-+="];
  const all = groups.join("");
  const characters = groups.map(group => group[secureRandomIndex(group.length)]);
  while (characters.length < 20) characters.push(all[secureRandomIndex(all.length)]);
  for (let index = characters.length - 1; index > 0; index -= 1) {
    const other = secureRandomIndex(index + 1);
    [characters[index], characters[other]] = [characters[other], characters[index]];
  }
  return characters.join("");
}

function generateUserPassword() {
  const input = el("user-password");
  input.value = generateStrongPassword();
  input.type = "text";
  input.focus();
  input.select();
  toast("已生成 20 位强密码，请复制并安全保存");
}

async function copyUserPassword() {
  const input = el("user-password");
  if (!input.value) { toast("请先生成或输入密码", true); return; }
  try {
    await navigator.clipboard.writeText(input.value);
    toast("初始密码已复制");
  } catch (_) {
    input.type = "text";
    input.focus();
    input.select();
    toast("已选中密码，请按 Ctrl+C 复制");
  }
}

async function refreshDeveloperStatus() {
  try {
    const result = await api("/api/developer/status");
    const valid = result.session_state === "valid";
    el("developer-state").className = "badge " + (valid ? "success" : "warning");
    el("developer-state").textContent = valid ? "已登录" : (result.session_state === "expired" ? "已失效" : "未登录");
    const accountHint = result.account_hint ? (" · 账号 " + result.account_hint) : "";
    el("developer-summary").textContent = valid ? ((result.nickname || "开发者") + accountHint + " · 等级 " + (result.level == null ? "—" : result.level) + " · 在售组件 " + (result.on_sale_item_count == null ? "—" : result.on_sale_item_count)) : (result.account_hint ? ("已绑定 " + result.account_hint + "，请重新登录后读取数据。") : "登录后 AI 才能读取组件评论和玩家反馈。");
    el("developer-login-fields").classList.toggle("hidden", valid);
    el("developer-logout").classList.toggle("hidden", !valid);
    el("developer-pill").textContent = "网易账号：" + (valid ? (result.nickname || "已登录") : "未登录");
    el("mobile-status").classList.toggle("online", valid);
    return valid;
  } catch (error) {
    el("developer-state").className = "badge error";
    el("developer-state").textContent = "连接失败";
    el("developer-summary").textContent = error.message;
    return false;
  }
}

async function developerPasswordLogin(event) {
  event.preventDefault();
  const button = event.submitter;
  setBusy(button, true, "登录计算中…");
  try {
    const result = await api("/api/developer/login-password", {
      method: "POST", body: { account: el("developer-account").value.trim(), password: el("developer-password").value }
    });
    el("developer-password").value = "";
    toast(result.warning || "网易账号登录成功");
    await refreshDeveloperStatus();
  } catch (error) {
    if (error.code === "SECURITY_VERIFICATION_REQUIRED") el("local-pairing").open = true;
    toast(error.message, true);
  }
  finally { el("developer-password").value = ""; setBusy(button, false); }
}

let pairingPoll = null;

async function createDeveloperPairing() {
  const button = el("create-pairing");
  const account = el("developer-account").value.trim();
  if (!account) { toast("请先填写需要绑定的网易账号", true); return; }
  setBusy(button, true, "正在生成…");
  try {
    const result = await api("/api/developer/pairing", { method: "POST", body: { account } });
    el("pairing-code").value = result.code;
    el("pairing-result").classList.remove("hidden");
    el("pairing-status").textContent = "连接码 10 分钟内有效；请在本机程序中粘贴";
    if (pairingPoll) window.clearInterval(pairingPoll);
    pairingPoll = window.setInterval(async function () {
      if (await refreshDeveloperStatus()) {
        window.clearInterval(pairingPoll);
        pairingPoll = null;
        el("pairing-status").textContent = "连接成功";
        toast("网易开发者账号已通过本机安全连接");
      }
    }, 2500);
    window.setTimeout(function () {
      if (pairingPoll) { window.clearInterval(pairingPoll); pairingPoll = null; }
    }, (result.expires_in || 600) * 1000);
  } catch (error) { toast(error.message, true); }
  finally { setBusy(button, false); }
}

async function copyDeveloperPairing() {
  const code = el("pairing-code").value;
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    toast("连接码已复制");
  } catch (_) {
    el("pairing-code").focus();
    el("pairing-code").select();
    toast("已选中连接码，请按 Ctrl+C 复制");
  }
}

async function developerCookieLogin() {
  const button = el("cookie-login");
  setBusy(button, true, "正在验证…");
  try {
    const result = await api("/api/developer/login-cookie", { method: "POST", body: { cookie: el("developer-cookie").value.trim() } });
    el("developer-cookie").value = "";
    toast(result.warning || "Cookie 登录成功");
    await refreshDeveloperStatus();
  } catch (error) { toast(error.message, true); }
  finally { el("developer-cookie").value = ""; setBusy(button, false); }
}

async function developerLogout() {
  if (!window.confirm("退出网易开发者账号？AI 将无法读取数据，直到重新登录。")) return;
  try { await api("/api/developer/logout", { method: "POST" }); await refreshDeveloperStatus(); toast("已退出网易账号"); }
  catch (error) { toast(error.message, true); }
}

async function loadModel() {
  try {
    const result = await api("/api/settings/model");
    el("model-base-url").value = result.base_url || "";
    el("model-name").value = result.model || "";
    el("model-key").placeholder = result.api_key_configured ? "已安全保存；留空则不更改" : "请输入 API Key";
  } catch (error) { toast(error.message, true); }
}

async function saveModel(event, test) {
  if (event) event.preventDefault();
  const button = test ? el("test-model") : (event && event.submitter);
  setBusy(button, true, test ? "正在测试…" : "保存中…");
  try {
    await api("/api/settings/model", {
      method: "PUT",
      body: { base_url: el("model-base-url").value.trim(), model: el("model-name").value.trim(), api_key: el("model-key").value || null }
    });
    el("model-key").value = "";
    if (test) await api("/api/settings/model/test", { method: "POST" });
    el("model-state").className = "badge success";
    el("model-state").textContent = test ? "工具调用正常" : "已保存";
    toast(test ? "模型连接和工具调用测试通过" : "模型设置已保存");
    await loadModel();
  } catch (error) {
    el("model-state").className = "badge error";
    el("model-state").textContent = "失败";
    toast(error.message, true);
  } finally { setBusy(button, false); }
}

async function loadSmtp() {
  try {
    const result = await api("/api/settings/smtp");
    el("smtp-host").value = result.host || "";
    el("smtp-port").value = result.port || 465;
    el("smtp-security").value = result.security || "smtps";
    el("smtp-username").value = result.username || "";
    el("smtp-from-email").value = result.from_email || "";
    el("smtp-from-name").value = result.from_name || "MC 玩家反馈助手";
    el("smtp-password").placeholder = result.password_configured ? "已安全保存；留空则不更改" : "请输入授权码或密码";
  } catch (error) { toast(error.message, true); }
}

function applySmtpPreset() {
  const preset = el("smtp-preset").value;
  if (preset === "163") { el("smtp-host").value = "smtp.163.com"; el("smtp-port").value = "465"; el("smtp-security").value = "smtps"; }
  if (preset === "qq") { el("smtp-host").value = "smtp.qq.com"; el("smtp-port").value = "465"; el("smtp-security").value = "smtps"; }
  if (preset === "163" || preset === "qq") {
    const sender = el("smtp-from-email").value.trim();
    if (sender) el("smtp-username").value = sender;
  }
}

async function saveSmtp(event, test) {
  if (event) event.preventDefault();
  const button = test ? el("test-smtp") : (event && event.submitter);
  setBusy(button, true, test ? "正在发送…" : "保存中…");
  try {
    const host = el("smtp-host").value.trim().toLowerCase();
    const sender = el("smtp-from-email").value.trim();
    const recipient = el("smtp-test-recipient").value.trim();
    if (test && !recipient) throw new Error("请输入测试收件地址");
    if ((host === "smtp.qq.com" || host === "smtp.163.com") && sender) {
      el("smtp-username").value = sender;
    }
    await api("/api/settings/smtp", {
      method: "PUT",
      body: {
        host: host, port: Number(el("smtp-port").value),
        security: el("smtp-security").value, username: el("smtp-username").value.trim(),
        password: el("smtp-password").value || null, from_email: sender,
        from_name: el("smtp-from-name").value.trim()
      }
    });
    el("smtp-password").value = "";
    if (test) {
      await api("/api/settings/smtp/test", { method: "POST", body: { recipient: recipient } });
    }
    el("smtp-state").className = "badge success";
    el("smtp-state").textContent = test ? "测试成功" : "已保存";
    toast(test ? "测试邮件已发送" : "发信设置已保存");
    await loadSmtp();
  } catch (error) {
    el("smtp-state").className = "badge error";
    el("smtp-state").textContent = "失败";
    toast(error.message, true);
  } finally { setBusy(button, false); }
}

boot();
