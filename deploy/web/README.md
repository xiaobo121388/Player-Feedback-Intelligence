# 自研 Web 服务部署

网页端是 `mc-feedback-web` 单一 Rust 进程。运行时不需要 Node.js、n8n、容器、Redis 或单独的 MCP 网关。

## 所需环境变量

部署脚本从当前进程读取以下变量，不把密码或密钥写入仓库：

- `MC_DEPLOY_HOST`：目标服务器主机名或 IP，没有默认值。
- `MC_DEPLOY_USER`：SSH 用户，默认 `root`。
- `MC_DEPLOY_PASSWORD`：可选；未设置时使用 SSH Agent 或本机 SSH 密钥。
- `MC_WEB_DOMAIN`：网站域名，用于渲染 nginx 配置和 HTTPS 健康检查。
- `MC_DESKTOP_HELPER`：已经构建的 Windows 桌面程序绝对路径，供网站下载并完成本机配对。
- `MC_WEB_ADMIN_EMAIL`、`MC_WEB_ADMIN_PASSWORD`：首位管理员；密码至少 12 个字符。
- `MC_WEB_MODEL_BASE_URL`、`MC_WEB_MODEL`、`MC_WEB_MODEL_API_KEY`：一套 OpenAI 兼容模型配置。
- 首次从旧版升级时可设置 `MC_WEB_LEGACY_NETEASE_ACCOUNT`，用于标记首位管理员原先绑定的网易账号；会话和历史数据自动归属该管理员。

执行：

```powershell
python deploy/web/deploy.py --all
```

变量名称示例见 [`deploy.env.example`](deploy.env.example)。示例文件只包含占位值；请在当前 Shell、CI Secret 或服务器私有配置中设置真实值，不要编辑后提交。

脚本先在 `127.0.0.1:15678` 启动暂存服务并验证健康、管理员角色、网易会话、模型文本调用和 function calling；验证通过后才切换 nginx 与生产端口 `127.0.0.1:5678`。旧 n8n 与 HTTP MCP 网关不会被 `--all` 自动删除，只有明确执行 `--purge-old` 才会清除部署脚本列出的旧服务和目录。

可用阶段：

```powershell
python deploy/web/deploy.py --inspect
python deploy/web/deploy.py --stage
python deploy/web/deploy.py --resume-stage
python deploy/web/deploy.py --cutover
python deploy/web/deploy.py --reclaim
python deploy/web/deploy.py --promote
python deploy/web/deploy.py --purge-old
```

旧单管理员版本升级后可运行 `--reclaim` 再次认领迁移窗口内产生的旧数据，并确认对话、数据集、文件、任务和执行记录不存在未归属行。

`--purge-old` 是不可恢复操作，仅删除明确列出的旧路径：`/opt/feedback-ai`、`/var/lib/feedback-ai`、`/etc/feedback-ai` 以及两项旧 systemd 服务。项目源码和部署脚本不包含默认 AI 工作，新任务列表为空。

## 手工运维

```sh
systemctl status mc-feedback-web
journalctl -u mc-feedback-web -n 100 --no-pager
systemctl restart mc-feedback-web
curl -fsS http://127.0.0.1:5678/healthz
```

数据目录是 `/var/lib/mc-feedback-web`，主密钥是 `/etc/mc-feedback-web/master.key`。主密钥不在数据库中，丢失后无法恢复模型密钥、SMTP 密码和网易会话。
