# BlockEcho — Player Feedback Intelligence

BlockEcho（中文界面名“MC反馈查看器”）是一个面向网易《我的世界》开发者的轻量只读反馈工具。仓库包含 Tauri 桌面查看器、stdio MCP，以及可独立部署的单进程 Rust AI 工作台。三种入口复用同一套只读网易查询核心。

> 本项目是非官方第三方个人工具，与网易没有隶属或授权关系。网易接口可能随时变化。

## 功能

- 网易账号密码登录，密码使用 RSA-PKCS1v1.5 加密后参与登录流程，不写入磁盘或日志。
- 登录遇到验证码、账号管家或风控时，可改用 `NTES_SESS` Cookie。
- 登录态仅保存到 Windows Credential Manager、macOS Keychain 或 Linux Secret Service。
- 分页查看组件评论，支持关键词、组件标签和日期筛选。
- 分页查看玩家反馈，支持关键词、反馈类型和回复状态筛选。
- 查看反馈截图、冲突组件、已有开发者回复与日志入口。
- 三个只读 MCP 工具；没有回复、提交、删除或通用网络请求工具。
- 自研中文 Web 工作台支持 AI 查询、CSV/DOCX/Markdown 导出、定时 AI 工作、执行记录、私有文件和固定收件地址邮件。

## 运行与构建

开发环境需要 Rust 1.88 或更高版本。桌面端使用系统 WebView：Windows WebView2、macOS WKWebView、Linux WebKitGTK；运行时不需要 Node.js。

```powershell
cargo run -p mc-feedback-viewer
```

构建当前平台的安装包：

```powershell
# Windows（NSIS）
npx -y @tauri-apps/cli@latest build --bundles nsis
```

macOS 使用 `--bundles dmg`，Linux 使用 `--bundles appimage,deb`。

Linux 需要先安装 Tauri 的 WebKitGTK 系统依赖；Debian/Ubuntu 示例：

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf
```

构建产物位于 `target/release/bundle/`。首版产物不包含代码签名或 macOS 公证。

## 登录说明

1. 正常情况下输入网易开发者账号与密码登录。
2. 软件完成网易登录安全计算后，只保留返回的 `NTES_SESS` 会话。
3. 如果网易要求额外安全验证，先在可信浏览器完成官方登录，再展开“使用 Cookie 登录”，粘贴完整 Cookie 或仅粘贴 `NTES_SESS` 的值。
4. 不要把 Cookie 发给任何人，也不要粘贴到 AI 对话中；它等同于当前登录会话。

退出登录会同时清除内存与系统钥匙串中的会话。Linux 若没有可用的 Secret Service，会退化为仅本次进程有效，并在界面提示。

若旧版显示“登录服务返回了无法识别的数据”，请升级到 0.1.1 或更高版本。网易登录接口的返回码可能同时使用数字和数字字符串，新版本已兼容这两种格式。
若 0.1.1 显示“网易服务返回了无法识别的数据”且只有组件评论无法查看，请升级到 0.1.2。该版本兼容评论接口把 `publish_time` 返回为数字字符串的情况。
0.1.3 的密码登录会在安全计算重试时保留同一 Cookie 会话，并为每次尝试重新生成 RSA 随机密文；桌面端还增加了“连接 AI 网站”入口。

## MCP 配置

请先在桌面端成功登录一次。MCP 模式不会接受账号、密码或 Cookie，只读取当前系统用户钥匙串中的登录态。

### Codex

在 Codex 的 `config.toml` 中添加：

```toml
[mcp_servers.mc_feedback]
command = "D:\\absolute\\path\\to\\mc-feedback-viewer.exe"
args = ["--mcp"]
```

macOS/Linux 将 `command` 改为实际可执行文件的绝对路径。

### 无桌面 Linux 服务器

安装 `gnome-keyring` 与 `dbus-run-session` 后，可以使用仓库中的包装脚本启动 Secret Service。首次只需在服务器终端执行一次：

```sh
/opt/mc-feedback-viewer/login
```

密码输入不回显；密码仍只驻留内存，不会写入钥匙串、配置文件或日志。该管理员入口与 `--mcp` 模式分离，MCP 工具仍不接受任何登录凭据。
如果服务器 IP 触发网易安全验证，可在本机浏览器完成验证后，在服务器终端执行 `/opt/mc-feedback-viewer/login-cookie`，仅粘贴 `NTES_SESS` 的值。Cookie 输入也不回显，不应粘贴到 AI 对话或 OpenCode 配置中。

OpenCode 稳定版的 `~/.config/opencode/opencode.json` 示例：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "mc-feedback": {
      "type": "local",
      "command": ["/opt/mc-feedback-viewer/run-mcp"],
      "enabled": true
    }
  }
}
```

### Claude Desktop 等 JSON 配置客户端

```json
{
  "mcpServers": {
    "mc-feedback": {
      "command": "/absolute/path/to/mc-feedback-viewer",
      "args": ["--mcp"]
    }
  }
}
```

提供的工具：

- `get_account_status`：检查本地登录状态，只返回昵称、等级和在售组件数量。
- `list_player_comments`：分页查询评论；支持 `keyword`、`tag`、`start_date`、`end_date`。
- `list_player_feedback`：分页查询问题反馈；支持 `keyword`、`type`、`replied`。

所有工具都标注为只读、非破坏、幂等。MCP 标准输出只包含 JSON-RPC 消息，诊断信息只写入标准错误。

## 自研网页 AI 工作台

网页由 `mc-feedback-web` 单一 Rust 进程提供服务，前端使用原生 HTML/CSS/ES Modules；不依赖 Node.js、n8n、容器、Redis、消息队列或独立 HTTP MCP 网关。仓库不包含任何预设服务器地址、平台账号、模型密钥或网易开发者凭据。

- 平台支持管理员创建多个普通用户。每个用户只能访问自己的网易会话、评论、反馈、对话、任务、执行记录和文件；管理员原有网易绑定不会被其他用户登录覆盖。
- 网易可能拒绝云服务器出口 IP 的密码登录并返回安全验证。此时在网页设置中生成一次性连接码，下载 Windows 本机程序，在本机登录网易后点击“连接 AI 网站”并粘贴连接码。密码不会上传，连接码 10 分钟内失效且使用后立即删除。
- “AI 查询”通过 OpenAI 兼容的 `/v1/chat/completions` 和 function calling 按需读取账号状态、评论和反馈。网页只显示工具摘要，不向模型开放网易登录、发信、Shell、任意 URL 或写入接口。
- 查询可生成 UTF-8 BOM CSV、Word 和 Markdown。CSV 会按原筛选条件重新分页读取；评论正文不作为通用离线缓存保存。
- “AI 工作”支持每天、每周、每月和五段 Cron，默认时区 `Asia/Shanghai`；停机后仅补跑最近错过的一次，同一工作重叠时留下跳过记录。
- 邮件仅供定时工作使用，收件地址固定在工作配置中，每次执行最多一封，并通过执行 ID 防止重复投递。
- SQLite 保存平台用户、聊天、工作、执行记录和文件元数据。管理员可创建普通平台账号；每位用户独立登录一个网易开发者账号，网易会话、评论/反馈查询、聊天、工作、执行记录和文件均按用户隔离。模型密钥、SMTP 密码和各用户网易会话使用 XChaCha20-Poly1305 加密，主密钥独立存放。

服务器构建不包含 Tauri：

```sh
cargo build -p mc-feedback-web --release
```

完整部署、暂存验收、回滚和运维说明见 [deploy/web/README.md](deploy/web/README.md)。仓库不预置任何定时工作，新任务列表从空开始。

桌面端的“连接 AI 网站”功能需要在构建时设置自托管网站地址：

```powershell
$env:MC_FEEDBACK_WEB_URL="https://feedback.example.com"
npx -y @tauri-apps/cli@latest build --bundles nsis
```

未设置时，桌面查看和 stdio MCP 仍可正常使用，仅网站配对入口会提示当前构建未配置网站地址。

## 安全边界

- API 客户端只定义网易登录端点和 `GET /users/me`、`GET /items/comment/pe/`、`GET /items/feedback/pe/`。
- 项目没有评论回复、反馈回复、提交反馈、网易数据删除、遥测、自动更新或云端 MCP 功能。Web 导出只生成当前登录用户可下载的私有报告文件。
- 前端只调用本站 API；Markdown 渲染禁用原始 HTML 和图片，并仅允许 HTTP/HTTPS 外部链接，避免把反馈内容当作可执行页面内容。
- 外部浏览器入口只允许打开 `163.com` 与 `netease.com` 域名下的 HTTP/HTTPS 链接。
- 明确收到 `no_login` 才清除会话；普通网络故障不会误删钥匙串内容。
- 日志不得记录密码、Cookie、Cookie 请求头或完整登录请求体。
- Web 模型 Base URL 仅允许 HTTPS 公网地址，禁用重定向并拒绝本机、内网、链路本地和特殊用途 IP。

## 测试

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace --all-targets
```

连接网易在线登录服务的回归测试默认忽略；仅在明确提供专用测试账号时手动执行，且完成后会退出并清除会话。测试不会输出密码、Cookie 或 Ticket。

详细人工验证步骤见 [docs/TESTING.md](docs/TESTING.md)。真实账号测试始终只执行读取请求。

## 参考与许可说明

产品范围和网易接口行为参考了 [BitterLemonn/MCDevManager](https://github.com/BitterLemonn/MCDevManager)；该项目使用 BNCL-1.0 非商业 Copyleft 许可证。当前项目从零独立实现，没有复制其 Kotlin 源码、界面、名称、图标或其他品牌资源。

本仓库公开源代码供审阅，但当前没有主动授予复制、修改或再分发许可证。若需要开放源代码许可或商业分发，请先重新确认网易平台条款、参考项目许可、第三方依赖许可证、源码发布义务及平台签名要求。第三方组件信息见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
