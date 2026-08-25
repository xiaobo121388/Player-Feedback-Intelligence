import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const baseUrl = process.env.MC_BROWSER_BASE_URL || "http://localhost:5678";
const email = process.env.MC_BROWSER_ADMIN_EMAIL;
const password = process.env.MC_BROWSER_ADMIN_PASSWORD;
if (!email || !password) throw new Error("缺少浏览器测试管理员凭据");

const candidates = [
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
];
const browserPath = candidates.find(fs.existsSync);
if (!browserPath) throw new Error("没有找到 Chrome/Edge");

const profile = fs.mkdtempSync(path.join(os.tmpdir(), "mc-feedback-browser-"));
const outputDir = path.resolve("target", "browser-smoke");
fs.mkdirSync(outputDir, { recursive: true });
const port = 19347;
const browser = spawn(browserPath, [
  "--headless=new",
  "--disable-gpu",
  "--no-first-run",
  "--no-default-browser-check",
  "--remote-debugging-port=" + port,
  "--user-data-dir=" + profile,
  "--window-size=1440,900",
  "about:blank",
], { stdio: "ignore", windowsHide: true });

let socket;
let sequence = 0;
const pending = new Map();
const consoleErrors = [];
const failedResponses = [];

function delay(ms) { return new Promise(resolve => setTimeout(resolve, ms)); }

async function retry(operation, timeout = 15000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    try { return await operation(); } catch (error) { last = error; await delay(150); }
  }
  throw last || new Error("等待超时");
}

async function command(method, params = {}) {
  const id = ++sequence;
  const result = new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
  socket.send(JSON.stringify({ id, method, params }));
  return result;
}

async function evaluate(expression) {
  const response = await command("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (response.exceptionDetails) throw new Error(response.exceptionDetails.text || "页面脚本执行失败");
  return response.result.value;
}

async function waitFor(expression, timeout = 20000) {
  return retry(async () => {
    const value = await evaluate(expression);
    if (!value) throw new Error("条件尚未满足");
    return value;
  }, timeout);
}

async function screenshot(name) {
  const result = await command("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
  fs.writeFileSync(path.join(outputDir, name), Buffer.from(result.data, "base64"));
}

try {
  await retry(async () => {
    const response = await fetch("http://127.0.0.1:" + port + "/json/version");
    if (!response.ok) throw new Error("CDP 未就绪");
    return response.json();
  });
  const pageResponse = await fetch(
    "http://127.0.0.1:" + port + "/json/new?" + encodeURIComponent(baseUrl),
    { method: "PUT" },
  );
  const page = await pageResponse.json();
  socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) {
      const callback = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) callback.reject(new Error(message.error.message));
      else callback.resolve(message.result);
    }
    if (message.method === "Runtime.exceptionThrown") {
      consoleErrors.push(message.params.exceptionDetails.text || "未捕获页面异常");
    }
    if (message.method === "Log.entryAdded" && message.params.entry.level === "error" && message.params.entry.source === "javascript") {
      consoleErrors.push(message.params.entry.text);
    }
    if (message.method === "Network.responseReceived" && message.params.response.status >= 400) {
      failedResponses.push({
        status: message.params.response.status,
        url: message.params.response.url,
      });
    }
  });
  await command("Page.enable");
  await command("Runtime.enable");
  await command("Log.enable");
  await command("Network.enable");
  await command("Emulation.setDeviceMetricsOverride", {
    width: 1440, height: 900, deviceScaleFactor: 1, mobile: false,
  });
  await command("Page.reload", { ignoreCache: true });
  await waitFor("document.readyState === 'complete' && !document.querySelector('#login').classList.contains('hidden')");
  const title = await evaluate("document.title");
  if (!title.includes("玩家反馈")) throw new Error("登录页标题不是中文");

  const loginScript = [
    "(() => {",
    "const set = (node, value) => {",
    "const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;",
    "setter.call(node, value);",
    "node.dispatchEvent(new Event('input', { bubbles: true }));",
    "};",
    "set(document.querySelector('#login-email'), " + JSON.stringify(email) + ");",
    "set(document.querySelector('#login-password'), " + JSON.stringify(password) + ");",
    "document.querySelector('#login-form button[type=submit]').click();",
    "return true;",
    "})()",
  ].join("\n");
  await evaluate(loginScript);
  await waitFor("!document.querySelector('#workspace').classList.contains('hidden')", 30000);
  await waitFor("document.querySelector('#developer-pill').textContent.includes('网易账号：') && !document.querySelector('#developer-pill').textContent.includes('检查中')", 20000);

  const desktop = await evaluate("(() => ({ nav: [...document.querySelectorAll('#nav button')].map(node => node.textContent.trim()), columns: getComputedStyle(document.querySelector('.query-view')).gridTemplateColumns, mobile: getComputedStyle(document.querySelector('.mobile-head')).display, conversations: document.querySelectorAll('.conversation-item').length }))()");
  if (desktop.nav.join("|") !== "◫AI 查询|◷AI 工作|✓执行记录|↓文件|⚙设置|♙用户管理") {
    throw new Error("主导航中文内容异常");
  }
  if (desktop.mobile !== "none") throw new Error("桌面布局异常：" + JSON.stringify(desktop));
  const markdown = await evaluate("(async () => { const module = await import('/app.js'); const html = module.renderMarkdown('## 标题\\n\\n- 项目\\n\\n| 列 | 值 |\\n|---|---|\\n| A | B |\\n\\n<script>alert(1)</script>\\n\\n[危险](javascript:alert(1))'); return { html, loaded: typeof window.markdownit === 'function' }; })()");
  if (!markdown.loaded || !markdown.html.includes("<h2>标题</h2>") || !markdown.html.includes("<table>") || markdown.html.includes("<script>") || markdown.html.includes("href=\"javascript:")) {
    throw new Error("Markdown 安全渲染异常：" + JSON.stringify(markdown));
  }
  const desktopScroll = await evaluate("(() => { const area = document.querySelector('#messages'); const previous = area.innerHTML; area.innerHTML = '<div class=\"message assistant\"><div class=\"bubble markdown-body\">' + '<p>很长的回复内容</p>'.repeat(500) + '</div></div>'; const result = { clientHeight: area.clientHeight, scrollHeight: area.scrollHeight, overflowY: getComputedStyle(area).overflowY, scrollBehavior: getComputedStyle(area).scrollBehavior }; area.scrollTop = area.scrollHeight; result.scrollTop = area.scrollTop; area.innerHTML = previous; return result; })()");
  if (desktopScroll.scrollHeight <= desktopScroll.clientHeight || desktopScroll.scrollTop <= 0 || desktopScroll.overflowY !== "auto" || desktopScroll.scrollBehavior === "smooth") {
    throw new Error("桌面长回复无法滚动：" + JSON.stringify(desktopScroll));
  }
  await screenshot("desktop.png");

  for (const view of ["jobs", "runs", "files", "settings", "query"]) {
    await evaluate("document.querySelector('[data-view=\"" + view + "\"]').click()");
    await waitFor("!document.querySelector('#view-" + view + "').classList.contains('hidden')");
  }
  await evaluate("document.querySelector('[data-view=\"jobs\"]').click()");
  await waitFor("!document.querySelector('#view-jobs').classList.contains('hidden')");
  await evaluate("document.querySelector('#new-job').click()");
  await waitFor("document.querySelector('#job-dialog').open");
  const jobDialog = await evaluate("(() => ({ title: document.querySelector('#job-dialog-title').textContent, timezone: document.querySelector('#job-timezone').value, enabled: document.querySelector('#job-enabled').checked }))()");
  if (jobDialog.title !== "新建 AI 工作" || jobDialog.timezone !== "Asia/Shanghai" || !jobDialog.enabled) {
    throw new Error("AI 工作表单默认值异常");
  }
  await evaluate("document.querySelector('#cancel-job').click()");

  await evaluate("document.querySelector('[data-view=\"users\"]').click()");
  await waitFor("!document.querySelector('#view-users').classList.contains('hidden')");
  const passwords = await evaluate("(async () => { const module = await import('/app.js'); const values = Array.from({ length: 128 }, () => module.generateStrongPassword()); const valid = values.every(value => value.length === 20 && /[A-Z]/.test(value) && /[a-z]/.test(value) && /[0-9]/.test(value) && [...value].some(character => '!@#$%^&*_-+='.includes(character))); document.querySelector('#generate-user-password').click(); const field = document.querySelector('#user-password'); return { valid, unique: new Set(values).size, fieldLength: field.value.length, fieldType: field.type }; })()");
  if (!passwords.valid || passwords.unique !== 128 || passwords.fieldLength !== 20 || passwords.fieldType !== "text") {
    throw new Error("随机强密码功能异常：" + JSON.stringify(passwords));
  }

  await command("Emulation.setDeviceMetricsOverride", {
    width: 390, height: 844, deviceScaleFactor: 1, mobile: true,
  });
  await command("Page.reload", { ignoreCache: true });
  await waitFor("document.readyState === 'complete' && !document.querySelector('#workspace').classList.contains('hidden')", 30000);
  const mobile = await evaluate("(() => ({ header: getComputedStyle(document.querySelector('.mobile-head')).display, workspace: getComputedStyle(document.querySelector('.workspace')).display, width: document.documentElement.scrollWidth, viewport: window.innerWidth }))()");
  if (mobile.header === "none" || mobile.width > mobile.viewport + 1) throw new Error("窄屏布局出现横向溢出");
  const mobileScroll = await evaluate("(() => { const area = document.querySelector('#messages'); const previous = area.innerHTML; area.innerHTML = '<div class=\"message assistant\"><div class=\"bubble markdown-body\">' + '<p>移动端长回复</p>'.repeat(500) + '</div></div>'; const result = { clientHeight: area.clientHeight, scrollHeight: area.scrollHeight, pageWidth: document.documentElement.scrollWidth, viewport: window.innerWidth, scrollBehavior: getComputedStyle(area).scrollBehavior }; area.scrollTop = area.scrollHeight; result.scrollTop = area.scrollTop; area.innerHTML = previous; return result; })()");
  if (mobileScroll.scrollHeight <= mobileScroll.clientHeight || mobileScroll.scrollTop <= 0 || mobileScroll.pageWidth > mobileScroll.viewport + 1 || mobileScroll.scrollBehavior === "smooth") {
    throw new Error("移动端长回复无法滚动：" + JSON.stringify(mobileScroll));
  }
  await screenshot("mobile.png");

  const seriousErrors = consoleErrors.filter(text =>
    !text.includes("favicon.ico") && !text.includes("net::ERR_ABORTED")
  );
  const unexpectedResponses = failedResponses.filter(item =>
    !(item.status === 401 && item.url.endsWith("/api/auth/me")) &&
    !item.url.endsWith("/favicon.ico")
  );
  if (seriousErrors.length) throw new Error("浏览器控制台错误：" + seriousErrors.join(" | "));
  if (unexpectedResponses.length) throw new Error("浏览器请求错误：" + JSON.stringify(unexpectedResponses));
  console.log(JSON.stringify({
    title,
    desktopColumns: desktop.columns,
    conversations: desktop.conversations,
    jobDialog,
    mobile,
    markdown: { loaded: markdown.loaded },
    desktopScroll,
    mobileScroll,
    consoleErrors: seriousErrors.length,
    unexpectedResponses: unexpectedResponses.length,
    screenshots: outputDir,
  }));
} finally {
  if (socket) socket.close();
  browser.kill();
  const resolvedProfile = path.resolve(profile);
  const resolvedTemp = path.resolve(os.tmpdir());
  if (resolvedProfile.startsWith(resolvedTemp + path.sep)) {
    for (let attempt = 0; attempt < 20; attempt++) {
      try { fs.rmSync(resolvedProfile, { recursive: true, force: true }); break; }
      catch { await delay(100); }
    }
  }
}
