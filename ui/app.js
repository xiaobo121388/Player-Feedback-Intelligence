const invoke = window.__TAURI__?.core?.invoke;

const state = {
  account: null,
  activeTab: "comments",
  comments: { items: [], total: 0, offset: 0, hasMore: false, selectedId: null, loading: false },
  feedback: { items: [], total: 0, offset: 0, hasMore: false, selectedId: null, loading: false },
};

const $ = (id) => document.getElementById(id);
const boot = $("boot");
const loginView = $("login-view");
const appView = $("app-view");
let toastTimer;

document.addEventListener("DOMContentLoaded", () => {
  bindEvents();
  bootstrap();
});

async function bootstrap() {
  if (!invoke) {
    showLogin("必须从桌面应用启动，浏览器预览无法访问本地服务。");
    return;
  }
  try {
    const account = await invoke("account_status");
    if (account.session_state === "valid") {
      enterApp(account);
    } else {
      showLogin(account.session_state === "expired" ? "登录已过期，请重新登录。" : "");
    }
  } catch (error) {
    showLogin(`暂时无法验证已保存的登录态：${errorMessage(error)}`);
  }
}

function bindEvents() {
  $("password-form").addEventListener("submit", loginWithPassword);
  $("cookie-form").addEventListener("submit", loginWithCookie);
  $("pair-website-button").addEventListener("click", openPairDialog);
  $("pair-cancel").addEventListener("click", closePairDialog);
  $("pair-form").addEventListener("submit", pairWithWebsite);
  $("logout-button").addEventListener("click", logout);
  document.querySelectorAll(".nav-item").forEach((button) => {
    button.addEventListener("click", () => switchTab(button.dataset.tab));
  });
  document.querySelector('[data-action="refresh-comments"]').addEventListener("click", () => loadComments(true));
  document.querySelector('[data-action="refresh-feedback"]').addEventListener("click", () => loadFeedback(true));
  $("comments-more").addEventListener("click", () => loadComments(false));
  $("feedback-more").addEventListener("click", () => loadFeedback(false));
  $("comment-clear").addEventListener("click", clearCommentFilters);
  $("feedback-clear").addEventListener("click", clearFeedbackFilters);

  const debouncedComments = debounce(() => loadComments(true), 400);
  const debouncedFeedback = debounce(() => loadFeedback(true), 400);
  $("comment-keyword").addEventListener("input", debouncedComments);
  $("feedback-keyword").addEventListener("input", debouncedFeedback);
  ["comment-tag", "comment-start-date", "comment-end-date"].forEach((id) => $(id).addEventListener("change", () => loadComments(true)));
  ["feedback-type", "feedback-replied"].forEach((id) => $(id).addEventListener("change", () => loadFeedback(true)));
}

function openPairDialog() {
  $("pair-code").value = "";
  $("pair-notice").textContent = "";
  $("pair-notice").classList.add("hidden");
  $("pair-dialog").classList.remove("hidden");
  $("pair-code").focus();
}

function closePairDialog() {
  $("pair-code").value = "";
  $("pair-dialog").classList.add("hidden");
}

async function pairWithWebsite(event) {
  event.preventDefault();
  const button = $("pair-submit");
  const notice = $("pair-notice");
  button.disabled = true;
  button.textContent = "正在连接…";
  notice.classList.add("hidden");
  try {
    const result = await invoke("pair_with_website", { code: $("pair-code").value.trim() });
    closePairDialog();
    toast(`网站连接成功${result.nickname ? `：${result.nickname}` : ""}`);
  } catch (error) {
    notice.textContent = errorMessage(error);
    notice.classList.remove("hidden");
  } finally {
    button.disabled = false;
    button.textContent = "连接网站";
  }
}

async function loginWithPassword(event) {
  event.preventDefault();
  const account = $("account").value.trim();
  const passwordInput = $("password");
  const password = passwordInput.value;
  setLoginBusy(true, "正在安全登录…");
  setLoginNotice("");
  try {
    const outcome = await invoke("login_password", { account, password });
    passwordInput.value = "";
    enterApp(outcome.account);
    if (outcome.warning) toast(outcome.warning);
  } catch (error) {
    const parsed = normalizeError(error);
    setLoginNotice(parsed.message);
    if (parsed.code === "SECURITY_VERIFICATION_REQUIRED") {
      $("cookie-fallback").open = true;
    }
  } finally {
    passwordInput.value = "";
    setLoginBusy(false);
  }
}

async function loginWithCookie(event) {
  event.preventDefault();
  const cookieInput = $("cookie");
  const cookie = cookieInput.value;
  setLoginBusy(true, "正在验证会话…");
  setLoginNotice("");
  try {
    const outcome = await invoke("login_cookie", { cookie });
    cookieInput.value = "";
    enterApp(outcome.account);
    if (outcome.warning) toast(outcome.warning);
  } catch (error) {
    setLoginNotice(errorMessage(error));
  } finally {
    cookieInput.value = "";
    setLoginBusy(false);
  }
}

async function logout() {
  try { await invoke("logout"); } catch (_) { /* memory state still resets */ }
  resetData();
  showLogin("已安全退出，系统钥匙串中的会话已清除。");
}

function enterApp(account) {
  state.account = account;
  boot.classList.add("hidden");
  loginView.classList.add("hidden");
  appView.classList.remove("hidden");
  $("account-summary").textContent = `${account.nickname || "开发者"} · Lv.${account.level ?? 0}`;
  switchTab(state.activeTab, true);
}

function showLogin(message = "") {
  boot.classList.add("hidden");
  appView.classList.add("hidden");
  loginView.classList.remove("hidden");
  setLoginNotice(message);
}

function switchTab(tab, forceLoad = false) {
  state.activeTab = tab;
  document.querySelectorAll(".nav-item").forEach((item) => item.classList.toggle("active", item.dataset.tab === tab));
  $("comments-tab").classList.toggle("hidden", tab !== "comments");
  $("feedback-tab").classList.toggle("hidden", tab !== "feedback");
  if (tab === "comments" && (forceLoad || state.comments.items.length === 0)) loadComments(true);
  if (tab === "feedback" && (forceLoad || state.feedback.items.length === 0)) loadFeedback(true);
}

async function loadComments(reset) {
  const bucket = state.comments;
  if (bucket.loading) return;
  bucket.loading = true;
  if (reset) {
    bucket.offset = 0;
    bucket.selectedId = null;
    closeMobileDetail("comments-master-detail");
  }
  renderListStatus("comments", reset ? "正在读取评论…" : "正在加载更多…");
  updateMoreButton("comments", false);
  try {
    const query = {
      offset: reset ? 0 : bucket.items.length,
      limit: 20,
      keyword: optionalValue("comment-keyword"),
      tag: optionalValue("comment-tag"),
      start_date: optionalValue("comment-start-date"),
      end_date: optionalValue("comment-end-date"),
    };
    const page = await invoke("list_comments", { query });
    bucket.items = reset ? page.items : bucket.items.concat(page.items);
    bucket.total = page.total;
    bucket.offset = page.offset;
    bucket.hasMore = page.has_more;
    updateCommentTags(bucket.items);
    renderComments();
  } catch (error) {
    handleQueryError(error, "comments");
  } finally {
    bucket.loading = false;
  }
}

async function loadFeedback(reset) {
  const bucket = state.feedback;
  if (bucket.loading) return;
  bucket.loading = true;
  if (reset) {
    bucket.offset = 0;
    bucket.selectedId = null;
    closeMobileDetail("feedback-master-detail");
  }
  renderListStatus("feedback", reset ? "正在读取反馈…" : "正在加载更多…");
  updateMoreButton("feedback", false);
  try {
    const repliedValue = $("feedback-replied").value;
    const query = {
      offset: reset ? 0 : bucket.items.length,
      limit: 20,
      keyword: optionalValue("feedback-keyword"),
      type: optionalValue("feedback-type"),
      replied: repliedValue === "" ? null : repliedValue === "true",
    };
    const page = await invoke("list_feedback", { query });
    bucket.items = reset ? page.items : bucket.items.concat(page.items);
    bucket.total = page.total;
    bucket.offset = page.offset;
    bucket.hasMore = page.has_more;
    renderFeedback();
  } catch (error) {
    handleQueryError(error, "feedback");
  } finally {
    bucket.loading = false;
  }
}

function renderComments() {
  const bucket = state.comments;
  const list = $("comments-list");
  list.replaceChildren();
  renderListStatus("comments", `已加载 ${bucket.items.length} / ${bucket.total} 条评论`);
  if (!bucket.items.length) list.append(emptyMessage("暂无符合条件的评论"));
  bucket.items.forEach((item) => list.append(commentListItem(item)));
  updateMoreButton("comments", bucket.hasMore);
  if (!bucket.selectedId) renderEmptyDetail("comment-detail", "✦", "选择一条评论查看详情");
}

function renderFeedback() {
  const bucket = state.feedback;
  const list = $("feedback-list");
  list.replaceChildren();
  renderListStatus("feedback", `已加载 ${bucket.items.length} / ${bucket.total} 条反馈`);
  if (!bucket.items.length) list.append(emptyMessage("暂无符合条件的反馈"));
  bucket.items.forEach((item) => list.append(feedbackListItem(item)));
  updateMoreButton("feedback", bucket.hasMore);
  if (!bucket.selectedId) renderEmptyDetail("feedback-detail", "◇", "选择一条反馈查看详情");
}

function commentListItem(item) {
  const card = element("article", "list-item");
  card.tabIndex = 0;
  card.dataset.id = item.id;
  card.append(
    row(nameText(item.nickname || "匿名玩家"), timeText(item.published_at)),
    textElement("div", "list-content", item.content || "（无评论内容）"),
    chips([
      [item.resource_name || "未知组件", "green"],
      item.tag && [item.tag, ""],
      item.stars && [`★ ${item.stars}`, "amber"],
    ])
  );
  const select = () => selectComment(item.id);
  card.addEventListener("click", select);
  card.addEventListener("keydown", (event) => { if (event.key === "Enter" || event.key === " ") select(); });
  return card;
}

function feedbackListItem(item) {
  const card = element("article", "list-item");
  card.tabIndex = 0;
  card.dataset.id = item.id;
  card.append(
    row(nameText(item.nickname || "匿名玩家"), timeText(item.created_at)),
    textElement("div", "list-content", conflictSummary(item.content) || "（无反馈内容）"),
    chips([
      item.resource_name && [item.resource_name, "green"],
      [item.feedback_type_label, ""],
      item.developer_reply && ["已回复", "green"],
      item.image_urls?.length && [`${item.image_urls.length} 图`, ""],
    ])
  );
  const select = () => selectFeedback(item.id);
  card.addEventListener("click", select);
  card.addEventListener("keydown", (event) => { if (event.key === "Enter" || event.key === " ") select(); });
  return card;
}

function selectComment(id) {
  const item = state.comments.items.find((entry) => entry.id === id);
  if (!item) return;
  state.comments.selectedId = id;
  markSelected("comments-list", id);
  const panel = $("comment-detail");
  panel.replaceChildren(detailBack("comments-master-detail"));
  const content = element("div", "detail-content");
  content.append(
    textElement("h3", "detail-title", item.nickname || "匿名玩家"),
    textElement("p", "detail-meta", formatTime(item.published_at)),
    chips([[item.resource_name || "未知组件", "green"], item.tag && [item.tag, ""], item.stars && [`★ ${item.stars}`, "amber"]]),
    detailGrid([
      ["玩家 UID", item.player_uid || "—"],
      ["组件 IID", item.resource_id || "—"],
    ]),
    detailSection("评论内容", textElement("p", "detail-body", item.content || "（无评论内容）"))
  );
  panel.append(content);
  $("comments-master-detail").classList.add("has-detail");
}

function selectFeedback(id) {
  const item = state.feedback.items.find((entry) => entry.id === id);
  if (!item) return;
  state.feedback.selectedId = id;
  markSelected("feedback-list", id);
  const panel = $("feedback-detail");
  panel.replaceChildren(detailBack("feedback-master-detail"));
  const content = element("div", "detail-content");
  content.append(
    textElement("h3", "detail-title", item.nickname || "匿名玩家"),
    textElement("p", "detail-meta", formatTime(item.created_at)),
    chips([[item.resource_name || "未知组件", "green"], [item.feedback_type_label, ""]]),
    detailGrid([
      ["玩家 UID", item.player_uid || "—"],
      ["组件 IID", item.resource_id || "—"],
      ["反馈 ID", item.id || "—"],
      ["回复状态", item.developer_reply ? "已回复" : "未回复"],
    ])
  );
  const conflict = parseConflict(item.content);
  if (conflict) content.append(conflictSection(conflict));
  else content.append(detailSection("反馈内容", textElement("p", "detail-body", item.content || "（无反馈内容）")));
  if (item.image_urls?.length) content.append(imageSection(item.image_urls));
  if (item.developer_reply) content.append(detailSection("已有开发者回复", textElement("p", "detail-body", item.developer_reply)));
  if (item.log_file_url) content.append(linkSection("反馈日志", item.log_file_url));
  panel.append(content);
  $("feedback-master-detail").classList.add("has-detail");
}

function conflictSection(conflict) {
  const box = element("div", "conflict-box");
  (conflict.item_list || []).forEach((item) => {
    const line = element("div", "conflict-row");
    line.append(textElement("span", "", item.name || "未知组件"), textElement("span", "", item.iid == null ? "" : `IID ${item.iid}`));
    box.append(line);
  });
  if (conflict.detail) box.append(textElement("p", "detail-body", conflict.detail));
  return detailSection("组件冲突", box);
}

function imageSection(urls) {
  const strip = element("div", "image-strip");
  urls.forEach((url, index) => {
    const link = element("a", "");
    link.href = url;
    bindExternalLink(link, url);
    const image = element("img", "");
    image.src = url;
    image.alt = `反馈附件 ${index + 1}`;
    image.loading = "lazy";
    link.append(image);
    strip.append(link);
  });
  return detailSection("附件图片", strip);
}

function linkSection(title, url) {
  const link = element("a", "external-link");
  link.href = url;
  bindExternalLink(link, url);
  link.textContent = "在默认浏览器中打开日志文件";
  return detailSection(title, link);
}

function detailSection(title, child) {
  const section = element("section", "detail-section");
  section.append(textElement("h3", "", title), child);
  return section;
}

function detailGrid(fields) {
  const grid = element("div", "detail-grid detail-section");
  fields.forEach(([label, value]) => {
    const field = element("div", "detail-field");
    field.append(textElement("small", "", label), textElement("strong", "", value));
    grid.append(field);
  });
  return grid;
}

function detailBack(containerId) {
  const button = textElement("button", "mobile-detail-back", "← 返回列表");
  button.type = "button";
  button.addEventListener("click", () => closeMobileDetail(containerId));
  return button;
}

function closeMobileDetail(containerId) { $(containerId).classList.remove("has-detail"); }
function renderEmptyDetail(id, symbol, text) {
  const box = element("div", "detail-empty");
  box.append(textElement("span", "", symbol), textElement("p", "", text));
  $(id).replaceChildren(box);
}
function emptyMessage(text) { return textElement("div", "empty-list", text); }
function row(...children) { const node = element("div", "list-row"); node.append(...children); return node; }
function nameText(text) { return textElement("span", "list-name", text); }
function timeText(value) { return textElement("time", "list-time", formatTime(value)); }
function chips(definitions) {
  const container = element("div", "chips");
  definitions.filter(Boolean).forEach(([label, variant]) => container.append(textElement("span", `chip ${variant || ""}`.trim(), String(label))));
  return container;
}
function element(tag, className) { const node = document.createElement(tag); if (className) node.className = className; return node; }
function textElement(tag, className, text) { const node = element(tag, className); node.textContent = text; return node; }
function markSelected(listId, id) { $(listId).querySelectorAll(".list-item").forEach((item) => item.classList.toggle("selected", item.dataset.id === id)); }

function updateCommentTags(items) {
  const select = $("comment-tag");
  const selected = select.value;
  const tags = [...new Set(items.map((item) => item.tag).filter(Boolean))].sort((a, b) => a.localeCompare(b, "zh-CN"));
  if (selected && !tags.includes(selected)) tags.unshift(selected);
  select.replaceChildren(new Option("全部标签", ""), ...tags.map((tag) => new Option(tag, tag)));
  select.value = selected;
}

function clearCommentFilters() {
  ["comment-keyword", "comment-tag", "comment-start-date", "comment-end-date"].forEach((id) => { $(id).value = ""; });
  loadComments(true);
}
function clearFeedbackFilters() {
  ["feedback-keyword", "feedback-type", "feedback-replied"].forEach((id) => { $(id).value = ""; });
  loadFeedback(true);
}
function resetData() {
  state.comments = { items: [], total: 0, offset: 0, hasMore: false, selectedId: null, loading: false };
  state.feedback = { items: [], total: 0, offset: 0, hasMore: false, selectedId: null, loading: false };
}
function renderListStatus(type, message) { $(`${type}-status`).textContent = message; }
function updateMoreButton(type, visible) { $(`${type}-more`).classList.toggle("hidden", !visible); }
function optionalValue(id) { const value = $(id).value.trim(); return value || null; }

function handleQueryError(error, type) {
  const parsed = normalizeError(error);
  renderListStatus(type, parsed.message);
  toast(parsed.message);
  if (parsed.code === "SESSION_EXPIRED" || parsed.code === "AUTH_REQUIRED") {
    resetData();
    showLogin(parsed.message);
  }
}

function setLoginBusy(busy, label = "") {
  ["password-submit", "cookie-submit"].forEach((id) => { $(id).disabled = busy; });
  if (busy) $("password-submit").textContent = label;
  else $("password-submit").textContent = "登录开发者账号";
}
function setLoginNotice(message) {
  const notice = $("login-notice");
  notice.textContent = message;
  notice.classList.toggle("hidden", !message);
}
function toast(message) {
  const node = $("toast");
  node.textContent = message;
  node.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => node.classList.add("hidden"), 4200);
}
function normalizeError(error) {
  if (error && typeof error === "object") return { code: error.code || "UNKNOWN", message: error.message || JSON.stringify(error) };
  try { const parsed = JSON.parse(String(error)); return normalizeError(parsed); } catch (_) { return { code: "UNKNOWN", message: String(error || "未知错误") }; }
}
function errorMessage(error) { return normalizeError(error).message; }
function bindExternalLink(link, url) {
  link.addEventListener("click", async (event) => {
    event.preventDefault();
    try { await invoke("open_external", { url }); }
    catch (error) { toast(errorMessage(error)); }
  });
}
function formatTime(value) {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? String(value || "") : new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" }).format(date);
}
function debounce(fn, delay) { let timer; return (...args) => { clearTimeout(timer); timer = setTimeout(() => fn(...args), delay); }; }
function parseConflict(content) {
  try {
    const value = JSON.parse(content);
    return value && Array.isArray(value.item_list) && Array.isArray(value.conflict_type) ? value : null;
  } catch (_) { return null; }
}
function conflictSummary(content) {
  const conflict = parseConflict(content);
  return conflict ? `冲突组件：${conflict.item_list.map((item) => item.name).filter(Boolean).join("、")}` : content;
}
