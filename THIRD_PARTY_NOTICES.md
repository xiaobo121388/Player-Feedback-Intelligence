# 第三方与参考说明

## 参考项目

- MCDevManager：<https://github.com/BitterLemonn/MCDevManager>
- 版权所有：BitterLemon，2024–2026
- 许可证：BitterLemon Noncommercial Copyleft License 1.0（BNCL-1.0）
- 使用方式：仅参考公开产品范围和网易接口行为；本项目没有复制其 Kotlin 源码、界面或品牌素材。

## 主要依赖

- Tauri：Apache-2.0 / MIT
- RMCP（官方 Rust MCP SDK）：Apache-2.0 / MIT
- reqwest、tokio、serde、RustCrypto、keyring-rs：各自的 Apache-2.0 / MIT 或兼容许可证
- Axum、rusqlite、chrono、cron、lettre、docx-rs、csv：各自的 Apache-2.0 / MIT 或兼容许可证
- markdown-it 14.1.0：MIT，用于在浏览器中安全渲染 AI 返回的 Markdown；原始 HTML 已禁用

服务器版是仓库内的独立 Rust 实现，不分发或运行 n8n、supergateway、Node.js 或第三方中文前端包。

完整、精确的传递依赖及版本以 `Cargo.lock` 为准。生成公开安装包前，请使用依赖许可证审计工具生成随包清单，并逐项遵守其许可证文本要求。
