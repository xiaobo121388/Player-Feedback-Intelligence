use std::{error::Error, sync::Arc};

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    api::{CommentQuery, FeedbackQuery, Service},
    error::AppError,
};

#[allow(dead_code)]
pub const TOOL_NAMES: &[&str] = &[
    "get_account_status",
    "list_player_comments",
    "list_player_feedback",
];

#[derive(Clone)]
pub struct FeedbackMcp {
    service: Arc<Service>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl FeedbackMcp {
    pub fn new(service: Arc<Service>) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl FeedbackMcp {
    #[tool(
        description = "检查 MC 开发者反馈查看器的本地登录状态。不会返回 Cookie、密码或收益信息。",
        annotations(
            title = "检查开发者登录状态",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_account_status(&self) -> Result<CallToolResult, McpError> {
        match self.service.account_status().await {
            Ok(status) => {
                let summary = match status.session_state {
                    crate::models::SessionState::Valid => format!(
                        "已登录：{}（等级 {}）",
                        status.nickname.as_deref().unwrap_or("未知账号"),
                        status.level.unwrap_or_default()
                    ),
                    crate::models::SessionState::Missing => {
                        "尚未登录，请先打开 MC反馈查看器完成登录".to_string()
                    }
                    crate::models::SessionState::Expired => {
                        "登录已过期，请打开 MC反馈查看器重新登录".to_string()
                    }
                };
                Ok(structured_success(summary, &status))
            }
            Err(error) => Ok(structured_error(error)),
        }
    }

    #[tool(
        description = "只读查询网易 Minecraft 开发者账号收到的组件评论。日期使用 YYYY-MM-DD，limit 范围为 1 到 100。",
        annotations(
            title = "查询玩家评论",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_player_comments(
        &self,
        Parameters(query): Parameters<CommentQuery>,
    ) -> Result<CallToolResult, McpError> {
        match self.service.list_comments(query).await {
            Ok(page) => Ok(structured_success(
                format!(
                    "返回 {} 条评论，共 {} 条，offset={}，has_more={}",
                    page.items.len(),
                    page.total,
                    page.offset,
                    page.has_more
                ),
                &page,
            )),
            Err(error) => Ok(structured_error(error)),
        }
    }

    #[tool(
        description = "只读查询网易 Minecraft 开发者账号收到的问题反馈。type 可使用 0..6，依次表示故障、玩法建议、侵权、其他、组件冲突、我的山头、性能反馈。",
        annotations(
            title = "查询玩家问题反馈",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_player_feedback(
        &self,
        Parameters(query): Parameters<FeedbackQuery>,
    ) -> Result<CallToolResult, McpError> {
        match self.service.list_feedback(query).await {
            Ok(page) => Ok(structured_success(
                format!(
                    "返回 {} 条反馈，共 {} 条，offset={}，has_more={}",
                    page.items.len(),
                    page.total,
                    page.offset,
                    page.has_more
                ),
                &page,
            )),
            Err(error) => Ok(structured_error(error)),
        }
    }
}

#[tool_handler(
    name = "mc-feedback-viewer",
    version = "0.1.2",
    instructions = "只读访问当前桌面用户已登录的网易 Minecraft 开发者评论与问题反馈。没有任何回复或修改工具。"
)]
impl ServerHandler for FeedbackMcp {}

pub async fn run(service: Arc<Service>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let server = FeedbackMcp::new(service).serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

fn structured_success<T: Serialize>(summary: String, value: &T) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(value) => {
            let mut result = CallToolResult::success(vec![ContentBlock::text(summary)]);
            result.structured_content = Some(value);
            result
        }
        Err(_) => CallToolResult::structured_error(json!({
            "code": "REMOTE_API_ERROR",
            "message": "无法序列化工具结果"
        })),
    }
}

fn structured_error(error: AppError) -> CallToolResult {
    let code = match serde_json::to_value(error.code).ok() {
        Some(Value::String(code)) => code,
        _ => "REMOTE_API_ERROR".to_string(),
    };
    CallToolResult::structured_error(json!({
        "code": code,
        "message": error.message,
        "remote_code": error.remote_code
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_exposes_only_three_read_only_tools() {
        assert_eq!(TOOL_NAMES.len(), 3);
        assert!(TOOL_NAMES.iter().all(|name| !name.contains("reply")));
        assert!(TOOL_NAMES.iter().all(|name| !name.contains("login")));
    }
}
