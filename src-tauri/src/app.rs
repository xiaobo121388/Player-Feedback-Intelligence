use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tauri::State;
use zeroize::Zeroize;

use crate::{
    api::{CommentQuery, FeedbackQuery, Service},
    error::AppError,
    models::{AccountStatus, LoginOutcome, Page, PlayerComment, PlayerFeedback},
};

pub struct AppState {
    service: Arc<Service>,
}

impl AppState {
    pub fn new(service: Arc<Service>) -> Self {
        Self { service }
    }
}

#[tauri::command]
async fn login_password(
    state: State<'_, AppState>,
    account: String,
    password: String,
) -> Result<LoginOutcome, AppError> {
    state.service.login_password(account, password).await
}

#[tauri::command]
async fn login_cookie(
    state: State<'_, AppState>,
    cookie: String,
) -> Result<LoginOutcome, AppError> {
    state.service.login_cookie(cookie).await
}

#[tauri::command]
async fn logout(state: State<'_, AppState>) -> Result<(), AppError> {
    state.service.logout().await;
    Ok(())
}

#[tauri::command]
async fn account_status(state: State<'_, AppState>) -> Result<AccountStatus, AppError> {
    state.service.account_status().await
}

#[tauri::command]
async fn list_comments(
    state: State<'_, AppState>,
    query: CommentQuery,
) -> Result<Page<PlayerComment>, AppError> {
    state.service.list_comments(query).await
}

#[tauri::command]
async fn list_feedback(
    state: State<'_, AppState>,
    query: FeedbackQuery,
) -> Result<Page<PlayerFeedback>, AppError> {
    state.service.list_feedback(query).await
}

#[derive(Serialize)]
struct PairingBody<'a> {
    code: &'a str,
    cookie: &'a str,
}

#[tauri::command]
async fn pair_with_website(state: State<'_, AppState>, code: String) -> Result<Value, AppError> {
    let code = code.trim().to_ascii_lowercase();
    if code.len() != 32 || !code.bytes().all(|value| value.is_ascii_hexdigit()) {
        return Err(AppError::invalid("请输入网站生成的 32 位连接码"));
    }

    let endpoint = pairing_endpoint()?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("MCFeedbackViewer/0.1")
        .build()
        .map_err(AppError::from_reqwest)?;
    let mut token = state.service.session_for_pairing().await?;
    let response = client
        .post(endpoint)
        .json(&PairingBody {
            code: &code,
            cookie: &token,
        })
        .send()
        .await;
    token.zeroize();
    let response = response.map_err(AppError::from_reqwest)?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|_| {
        AppError::new(
            crate::error::ErrorCode::RemoteApiError,
            "网站返回了无法识别的数据",
        )
    })?;
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("连接网站失败，请重新生成连接码");
        return Err(AppError::new(
            crate::error::ErrorCode::RemoteApiError,
            message,
        ));
    }
    Ok(body)
}

fn pairing_endpoint() -> Result<url::Url, AppError> {
    let configured = std::env::var("MC_FEEDBACK_WEB_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| option_env!("MC_FEEDBACK_WEB_URL").map(str::to_string))
        .ok_or_else(|| AppError::invalid("当前构建未配置 AI 网站地址"))?;
    let mut url =
        url::Url::parse(configured.trim()).map_err(|_| AppError::invalid("AI 网站地址格式无效"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::invalid("AI 网站地址必须是无凭据的 HTTPS 地址"));
    }
    url.set_path("/api/developer/pairing/complete");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

#[tauri::command]
fn open_external(url: String) -> Result<(), AppError> {
    let parsed = url::Url::parse(&url).map_err(|_| AppError::invalid("链接格式无效"))?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if !matches!(parsed.scheme(), "http" | "https")
        || !(host == "163.com"
            || host.ends_with(".163.com")
            || host == "netease.com"
            || host.ends_with(".netease.com"))
    {
        return Err(AppError::invalid("仅允许打开网易域名下的 HTTP/HTTPS 链接"));
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(&url);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(&url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(&url);
        command
    };

    command.spawn().map(|_| ()).map_err(|_| {
        AppError::new(
            crate::error::ErrorCode::RemoteApiError,
            "无法打开默认浏览器",
        )
    })
}

pub fn run(service: Arc<Service>) -> Result<(), String> {
    tauri::Builder::default()
        .manage(AppState::new(service))
        .invoke_handler(tauri::generate_handler![
            login_password,
            login_cookie,
            logout,
            account_status,
            list_comments,
            list_feedback,
            pair_with_website,
            open_external
        ])
        .run(tauri::generate_context!())
        .map_err(|error| format!("应用启动失败：{error}"))
}
