#[path = "../../src-tauri/src/api.rs"]
mod api;
#[path = "../../src-tauri/src/crypto.rs"]
mod crypto;
#[path = "../../src-tauri/src/error.rs"]
mod error;
#[path = "../../src-tauri/src/models.rs"]
mod models;

mod ai;
mod artifacts;
mod crypto_store;
mod db;
mod email;
mod scheduler;
mod session;

use std::{
    collections::HashMap,
    env,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize;

use ai::{AiContext, AiEngine, ModelInput};
use api::{CommentQuery, Service};
use db::{Database, JobInput, WebSession};
use scheduler::Scheduler;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLES_CSS: &str = include_str!("../static/styles.css");
const MARKDOWN_IT_JS: &str = include_str!("../static/vendor/markdown-it.min.js");

#[derive(Clone)]
struct AppState {
    database: Database,
    ai: AiEngine,
    scheduler: Scheduler,
    active_runs: Arc<Mutex<HashMap<String, (String, CancellationToken)>>>,
    completed_runs: Arc<Mutex<HashMap<String, CompletedChatRun>>>,
    interactive: Arc<Semaphore>,
    developer_logins: Arc<Semaphore>,
    developer_pairings: Arc<Mutex<HashMap<String, DeveloperPairing>>>,
    login_attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

#[derive(Clone)]
struct CompletedChatRun {
    owner_id: String,
    state: &'static str,
    code: Option<String>,
    message: Option<String>,
    total: Option<usize>,
    completed_at: Instant,
}

#[derive(Clone)]
struct DeveloperPairing {
    owner_id: String,
    account: String,
    expires_at: Instant,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "INVALID_ARGUMENT",
            message: message.into(),
        }
    }
    fn auth() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "AUTH_REQUIRED",
            message: "请先登录".into(),
        }
    }
    fn csrf() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "CSRF_INVALID",
            message: "页面安全令牌已失效，请刷新页面".into(),
        }
    }
    fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "FORBIDDEN",
            message: "只有平台管理员可以执行此操作".into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND",
            message: message.into(),
        }
    }
    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR",
            message: redact(&error.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"code":self.code,"message":self.message}})),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let command = env::args().nth(1);
    let database_path = PathBuf::from(
        env::var("MC_WEB_DATABASE")
            .unwrap_or_else(|_| "/var/lib/mc-feedback-web/data.sqlite".into()),
    );
    let key_path = PathBuf::from(
        env::var("MC_WEB_MASTER_KEY_FILE")
            .unwrap_or_else(|_| "/etc/mc-feedback-web/master.key".into()),
    );
    let artifact_root = PathBuf::from(
        env::var("MC_WEB_ARTIFACT_ROOT")
            .unwrap_or_else(|_| "/var/lib/mc-feedback-web/artifacts".into()),
    );
    let bind: SocketAddr = env::var("MC_WEB_BIND")
        .unwrap_or_else(|_| "127.0.0.1:5678".into())
        .parse()
        .context("MC_WEB_BIND 无效")?;
    let database = Database::open(&database_path, &key_path, &artifact_root)?;
    if command.as_deref() == Some("--import-session") {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        let mut token = parse_session_input(&input)?;
        input.zeroize();
        database.set_secret("netease_session", &token)?;
        token.zeroize();
        eprintln!("网易会话已加密导入");
        return Ok(());
    }
    if command.as_deref() == Some("--bootstrap-stdin") {
        let mut input = String::new();
        std::io::stdin()
            .take(32 * 1024)
            .read_to_string(&mut input)?;
        let mut values: BootstrapInput = serde_json::from_str(&input).context("初始化输入无效")?;
        input.zeroize();
        let admin = database.ensure_admin(&values.admin_email, &mut values.admin_password)?;
        database.claim_legacy_data(&admin.id, values.legacy_netease_account.as_deref())?;
        let mut model_input = ModelInput {
            base_url: values.model_base_url,
            model: values.model,
            api_key: Some(std::mem::take(&mut values.model_api_key)),
        };
        ai::save_model_settings(&database, &model_input)?;
        if let Some(key) = model_input.api_key.as_mut() {
            key.zeroize();
        }
        eprintln!("管理员和模型设置已加密初始化");
        return Ok(());
    }
    if command.as_deref() == Some("--diagnose-login-stdin") {
        diagnose_login(&database).await?;
        return Ok(());
    }
    bootstrap(&database)?;
    let ai = AiEngine::new(database.clone())?;
    let scheduler = Scheduler::new(database.clone(), ai.clone());
    let state = AppState {
        database,
        ai,
        scheduler: scheduler.clone(),
        active_runs: Arc::new(Mutex::new(HashMap::new())),
        completed_runs: Arc::new(Mutex::new(HashMap::new())),
        interactive: Arc::new(Semaphore::new(1)),
        developer_logins: Arc::new(Semaphore::new(1)),
        developer_pairings: Arc::new(Mutex::new(HashMap::new())),
        login_attempts: Arc::new(Mutex::new(HashMap::new())),
    };
    scheduler.start();

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/styles.css", get(styles_css))
        .route("/vendor/markdown-it.min.js", get(markdown_it_js))
        .route(
            "/downloads/mc-feedback-viewer-windows-x64.exe",
            get(download_desktop_helper),
        )
        .route("/healthz", get(health))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/admin/users", get(list_users).post(create_user))
        .route("/api/settings/model", get(get_model).put(save_model))
        .route("/api/settings/model/test", post(test_model))
        .route("/api/settings/smtp", get(get_smtp).put(save_smtp))
        .route("/api/settings/smtp/test", post(test_smtp))
        .route("/api/developer/status", get(developer_status))
        .route("/api/developer/login-password", post(developer_password))
        .route("/api/developer/login-cookie", post(developer_cookie))
        .route("/api/developer/pairing", post(create_developer_pairing))
        .route(
            "/api/developer/pairing/complete",
            post(complete_developer_pairing),
        )
        .route("/api/developer/logout", post(developer_logout))
        .route(
            "/api/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route("/api/conversations/{id}", delete(delete_conversation))
        .route(
            "/api/conversations/{id}/messages",
            get(list_messages).post(send_message),
        )
        .route("/api/chat/runs/{id}", get(chat_run_status))
        .route("/api/chat/runs/{id}/cancel", post(cancel_run))
        .route("/api/artifacts", get(list_artifacts))
        .route("/api/artifacts/export", post(export_artifact))
        .route("/api/artifacts/{id}", delete(delete_artifact))
        .route("/api/artifacts/{id}/download", get(download_artifact))
        .route("/api/jobs", get(list_jobs).post(create_job))
        .route("/api/jobs/preview", post(preview_schedule))
        .route("/api/jobs/{id}", put(update_job).delete(delete_job))
        .route("/api/jobs/{id}/run", post(run_job))
        .route("/api/runs", get(list_runs))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("mc-feedback-web listening on {bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

fn bootstrap(database: &Database) -> Result<()> {
    if let (Ok(email), Ok(mut password)) = (
        env::var("MC_WEB_ADMIN_EMAIL"),
        env::var("MC_WEB_ADMIN_PASSWORD"),
    ) {
        let admin = database.ensure_admin(&email, &mut password)?;
        database.claim_legacy_data(
            &admin.id,
            env::var("MC_WEB_LEGACY_NETEASE_ACCOUNT").ok().as_deref(),
        )?;
    }
    if database.setting("model_base_url")?.is_none() {
        if let (Ok(base_url), Ok(model), Ok(api_key)) = (
            env::var("MC_WEB_MODEL_BASE_URL"),
            env::var("MC_WEB_MODEL"),
            env::var("MC_WEB_MODEL_API_KEY"),
        ) {
            database.set_setting("model_base_url", base_url.trim_end_matches('/'))?;
            database.set_setting("model_name", &model)?;
            database.set_secret("model_api_key", &api_key)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct BootstrapInput {
    admin_email: String,
    admin_password: String,
    model_base_url: String,
    model: String,
    model_api_key: String,
    #[serde(default)]
    legacy_netease_account: Option<String>,
}

#[derive(Deserialize)]
struct DiagnosticLoginInput {
    account: String,
    password: String,
}

async fn diagnose_login(database: &Database) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().take(4 * 1024).read_to_string(&mut input)?;
    let mut values: DiagnosticLoginInput =
        serde_json::from_str(&input).context("诊断登录输入无效")?;
    input.zeroize();

    let diagnostic_id = format!("diagnostic-{}", Uuid::new_v4());
    let service = Service::new_for_user(&diagnostic_id)?;
    let result = service
        .login_password(
            std::mem::take(&mut values.account),
            std::mem::take(&mut values.password),
        )
        .await;
    values.account.zeroize();
    values.password.zeroize();

    let output = match result {
        Ok(outcome) => {
            let comments = service
                .list_comments(CommentQuery {
                    limit: 1,
                    ..CommentQuery::default()
                })
                .await;
            service.logout().await;
            match comments {
                Ok(page) => json!({
                    "ok": true,
                    "session_state": outcome.account.session_state,
                    "nickname": outcome.account.nickname,
                    "comment_total": page.total,
                    "comment_probe_items": page.items.len(),
                    "isolated_session_key": true
                }),
                Err(error) => json!({
                    "ok": false,
                    "stage": "comment_probe",
                    "code": error_code_name(error.code),
                    "remote_code": error.remote_code,
                    "message": error.message,
                    "isolated_session_key": true
                }),
            }
        }
        Err(error) => {
            service.logout().await;
            json!({
                "ok": false,
                "stage": "password_login",
                "code": error_code_name(error.code),
                "remote_code": error.remote_code,
                "message": error.message,
                "isolated_session_key": true
            })
        }
    };

    let admin_session_still_valid = if let Some(admin) = database.primary_admin()? {
        let admin_service = Service::new_for_user(&admin.id)?;
        matches!(
            admin_service.account_status().await,
            Ok(models::AccountStatus {
                session_state: models::SessionState::Valid,
                ..
            })
        )
    } else {
        false
    };
    let mut output = output;
    if let Some(object) = output.as_object_mut() {
        object.insert(
            "admin_session_still_valid".into(),
            Value::Bool(admin_session_still_valid),
        );
    }
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {_=ctrl_c=>{},_=terminate=>{}}
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; img-src 'self' data: https:; style-src 'self'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
}

async fn index() -> impl IntoResponse {
    html(INDEX_HTML)
}
async fn app_js() -> impl IntoResponse {
    asset("application/javascript; charset=utf-8", APP_JS)
}
async fn styles_css() -> impl IntoResponse {
    asset("text/css; charset=utf-8", STYLES_CSS)
}
async fn markdown_it_js() -> impl IntoResponse {
    asset("application/javascript; charset=utf-8", MARKDOWN_IT_JS)
}
async fn health() -> impl IntoResponse {
    Json(json!({"status":"ok","service":"mc-feedback-web"}))
}
fn html(value: &'static str) -> Response {
    asset("text/html; charset=utf-8", value)
}
fn asset(content_type: &'static str, value: &'static str) -> Response {
    let mut response = Response::new(Body::from(value));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

async fn download_desktop_helper() -> Result<Response, ApiError> {
    let path = env::var_os("MC_WEB_DESKTOP_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/opt/mc-feedback-web/mc-feedback-viewer-windows-x64.exe")
        });
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| ApiError::not_found("Windows 本机连接程序尚未发布"))?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=mc-feedback-viewer-windows-x64.exe"),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct LoginInput {
    email: String,
    password: String,
}
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<LoginInput>,
) -> Result<Response, ApiError> {
    let key = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("local")
        .to_string();
    {
        let mut attempts = state.login_attempts.lock().map_err(ApiError::internal)?;
        let now = Instant::now();
        attempts.retain(|_, values| {
            values.retain(|value| now.duration_since(*value) < Duration::from_secs(900));
            !values.is_empty()
        });
        if attempts.get(&key).is_some_and(|values| values.len() >= 5) {
            return Err(ApiError {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "RATE_LIMITED",
                message: "登录尝试过多，请 15 分钟后再试".into(),
            });
        }
    }
    let admin = state
        .database
        .verify_admin(&input.email, &input.password)
        .map_err(ApiError::internal)?;
    let Some(admin) = admin else {
        state
            .login_attempts
            .lock()
            .map_err(ApiError::internal)?
            .entry(key)
            .or_default()
            .push(Instant::now());
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "INVALID_CREDENTIALS",
            message: "邮箱或密码错误".into(),
        });
    };
    state
        .login_attempts
        .lock()
        .map_err(ApiError::internal)?
        .remove(&key);
    let (token, csrf) = state
        .database
        .create_session(&admin.id)
        .map_err(ApiError::internal)?;
    let mut response = Json(json!({"admin":admin,"csrf":csrf})).into_response();
    response.headers_mut().insert(header::SET_COOKIE,HeaderValue::from_str(&format!("mc_feedback_session={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=2592000")).map_err(ApiError::internal)?);
    Ok(response)
}
async fn me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    Ok(Json(json!({"admin":session.admin,"csrf":session.csrf})))
}
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let _ = authorize(&state, &headers, true)?;
    if let Some(token) = cookie(&headers, "mc_feedback_session") {
        state
            .database
            .delete_session(&token)
            .map_err(ApiError::internal)?;
    }
    let mut response = Json(json!({"ok":true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "mc_feedback_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        ),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct CreateUserInput {
    email: String,
    password: String,
}

async fn list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize_admin(&state, &headers, false)?;
    Ok(Json(json!({
        "items": state.database.users().map_err(ApiError::internal)?
    })))
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<CreateUserInput>,
) -> Result<Json<Value>, ApiError> {
    authorize_admin(&state, &headers, true)?;
    let user = state
        .database
        .create_user(&input.email, &mut input.password)
        .map_err(|error| ApiError::bad(error.to_string()))?;
    Ok(Json(json!({"user":user})))
}

async fn get_model(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ai::ModelView>, ApiError> {
    authorize_admin(&state, &headers, false)?;
    Ok(Json(state.ai.model_view().map_err(ApiError::internal)?))
}
async fn save_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ModelInput>,
) -> Result<Json<ai::ModelView>, ApiError> {
    authorize_admin(&state, &headers, true)?;
    Ok(Json(
        state
            .ai
            .save_model(&input)
            .map_err(|e| ApiError::bad(e.to_string()))?,
    ))
}
async fn test_model(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize_admin(&state, &headers, true)?;
    state
        .ai
        .test_model(&CancellationToken::new())
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok":true,"tools":true})))
}

async fn get_smtp(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<email::SmtpView>, ApiError> {
    authorize_admin(&state, &headers, false)?;
    Ok(Json(
        email::view(&state.database).map_err(ApiError::internal)?,
    ))
}
async fn save_smtp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<email::SmtpInput>,
) -> Result<Json<email::SmtpView>, ApiError> {
    authorize_admin(&state, &headers, true)?;
    Ok(Json(
        email::save(&state.database, &input).map_err(|e| ApiError::bad(e.to_string()))?,
    ))
}
#[derive(Deserialize)]
struct SmtpTestInput {
    recipient: String,
}
async fn test_smtp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SmtpTestInput>,
) -> Result<Json<Value>, ApiError> {
    authorize_admin(&state, &headers, true)?;
    if let Err(error) = email::send(
        &state.database,
        None,
        &input.recipient,
        "MC 玩家反馈助手测试邮件",
        "SMTP 配置测试成功。",
        &[],
    )
    .await
    {
        let public = email::public_send_error(&error);
        eprintln!("SMTP 测试失败：{}", public.code);
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: public.code,
            message: public.message,
        });
    }
    Ok(Json(json!({"ok":true})))
}

async fn developer_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    let status = service_for_user(&session.admin.id)?
        .account_status()
        .await
        .map_err(core_error)?;
    let mut value = serde_json::to_value(status).map_err(ApiError::internal)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "account_hint".into(),
            state
                .database
                .netease_account(&session.admin.id)
                .map_err(ApiError::internal)?
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    Ok(Json(value))
}
#[derive(Deserialize)]
struct PasswordLogin {
    account: String,
    password: String,
}
async fn developer_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PasswordLogin>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let account = input.account.trim().to_string();
    let _login_permit = state
        .developer_logins
        .acquire()
        .await
        .map_err(ApiError::internal)?;
    let outcome = service_for_user(&session.admin.id)?
        .login_password(input.account, input.password)
        .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!(
                "网易密码登录失败：code={}, remote_code={:?}",
                error_code_name(error.code),
                error.remote_code
            );
            return Err(core_error(error));
        }
    };
    state
        .database
        .set_netease_account(&session.admin.id, &account)
        .map_err(ApiError::internal)?;
    Ok(Json(
        serde_json::to_value(outcome).map_err(ApiError::internal)?,
    ))
}

#[derive(Deserialize)]
struct CreatePairingInput {
    account: String,
}

async fn create_developer_pairing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreatePairingInput>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let account = input.account.trim();
    if account.is_empty() || account.len() > 320 {
        return Err(ApiError::bad("请输入有效的网易账号"));
    }

    let code = Uuid::new_v4().simple().to_string();
    let expires_at = Instant::now() + Duration::from_secs(10 * 60);
    let mut pairings = state
        .developer_pairings
        .lock()
        .map_err(ApiError::internal)?;
    let now = Instant::now();
    pairings.retain(|_, pairing| pairing.expires_at > now && pairing.owner_id != session.admin.id);
    pairings.insert(
        code.clone(),
        DeveloperPairing {
            owner_id: session.admin.id,
            account: account.to_string(),
            expires_at,
        },
    );
    Ok(Json(json!({"code":code,"expires_in":600})))
}

#[derive(Deserialize)]
struct CompletePairingInput {
    code: String,
    cookie: String,
}

async fn complete_developer_pairing(
    State(state): State<AppState>,
    Json(mut input): Json<CompletePairingInput>,
) -> Result<Json<Value>, ApiError> {
    let code = input.code.trim();
    if !valid_pairing_code(code) || input.cookie.len() > 8192 {
        input.cookie.zeroize();
        return Err(ApiError::bad("连接码无效或已过期"));
    }
    let pairing = {
        let mut pairings = state
            .developer_pairings
            .lock()
            .map_err(ApiError::internal)?;
        let now = Instant::now();
        pairings.retain(|_, pairing| pairing.expires_at > now);
        pairings.remove(code)
    }
    .ok_or_else(|| ApiError::bad("连接码无效或已过期"))?;

    let cookie = std::mem::take(&mut input.cookie);
    let outcome = service_for_user(&pairing.owner_id)?
        .login_cookie(cookie)
        .await
        .map_err(core_error)?;
    input.cookie.zeroize();
    state
        .database
        .set_netease_account(&pairing.owner_id, &pairing.account)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "nickname": outcome.account.nickname,
        "session_state": outcome.account.session_state
    })))
}

fn valid_pairing_code(code: &str) -> bool {
    code.len() == 32 && code.bytes().all(|value| value.is_ascii_hexdigit())
}
#[derive(Deserialize)]
struct CookieLogin {
    cookie: String,
}
async fn developer_cookie(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CookieLogin>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let outcome = service_for_user(&session.admin.id)?
        .login_cookie(input.cookie)
        .await
        .map_err(core_error)?;
    Ok(Json(
        serde_json::to_value(outcome).map_err(ApiError::internal)?,
    ))
}
async fn developer_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    service_for_user(&session.admin.id)?.logout().await;
    Ok(Json(json!({"ok":true})))
}

async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    Ok(Json(
        json!({"items":state.database.conversations(&session.admin.id).map_err(ApiError::internal)?}),
    ))
}
#[derive(Deserialize)]
struct ConversationInput {
    title: Option<String>,
}
async fn create_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ConversationInput>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let item = state
        .database
        .create_conversation(
            &session.admin.id,
            input.title.as_deref().unwrap_or("新对话"),
        )
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"item":item})))
}
async fn delete_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let deleted = state
        .database
        .delete_conversation(&session.admin.id, &id)
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::not_found("对话不存在"));
    }
    Ok(Json(json!({"ok":true})))
}
async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    let owner_id = &session.admin.id;
    if state
        .database
        .conversations(owner_id)
        .map_err(ApiError::internal)?
        .iter()
        .all(|item| item.id != id)
    {
        return Err(ApiError::not_found("对话不存在"));
    }
    Ok(Json(
        json!({"items":state.database.messages(owner_id, &id).map_err(ApiError::internal)?,"datasets":state.database.datasets_for_conversation(owner_id, &id).map_err(ApiError::internal)?}),
    ))
}

#[derive(Deserialize)]
struct ChatInput {
    run_id: String,
    content: String,
    #[serde(default)]
    allow_large: bool,
    #[serde(default)]
    resume: bool,
}
async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Json(input): Json<ChatInput>,
) -> Result<impl IntoResponse, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let owner_id = session.admin.id.clone();
    Uuid::parse_str(&input.run_id).map_err(|_| ApiError::bad("run_id 无效"))?;
    if input.content.trim().is_empty() || input.content.chars().count() > 20_000 {
        return Err(ApiError::bad("消息必须为 1 到 20000 个字符"));
    }
    if state
        .database
        .messages(&owner_id, &conversation_id)
        .map_err(ApiError::internal)?
        .is_empty()
        && state
            .database
            .conversations(&owner_id)
            .map_err(ApiError::internal)?
            .iter()
            .all(|item| item.id != conversation_id)
    {
        return Err(ApiError::not_found("对话不存在"));
    }
    if !input.resume {
        state
            .database
            .add_message(
                &owner_id,
                &conversation_id,
                "user",
                input.content.trim(),
                None,
            )
            .map_err(ApiError::internal)?;
    }
    let history = state
        .database
        .messages(&owner_id, &conversation_id)
        .map_err(ApiError::internal)?;
    let cancel = CancellationToken::new();
    {
        let mut runs = state.active_runs.lock().map_err(ApiError::internal)?;
        if runs.contains_key(&input.run_id) {
            return Err(ApiError::bad("run_id 已在使用"));
        }
        runs.insert(input.run_id.clone(), (owner_id.clone(), cancel.clone()));
    }
    state
        .completed_runs
        .lock()
        .map_err(ApiError::internal)?
        .remove(&input.run_id);
    let (sender, receiver) = mpsc::channel(16);
    let run_id = input.run_id.clone();
    let task_state = state.clone();
    tokio::spawn(async move {
        let run_started = Instant::now();
        let _ = send_event(
            &sender,
            "status",
            json!({"run_id":run_id,"message":"正在等待分析…"}),
        )
        .await;
        let progress_stop = CancellationToken::new();
        let progress_task = {
            let sender = sender.clone();
            let run_id = run_id.clone();
            let stop = progress_stop.clone();
            tokio::spawn(async move {
                let mut elapsed = 0u64;
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            elapsed += 30;
                            send_event(
                                &sender,
                                "status",
                                json!({
                                    "run_id": run_id,
                                    "message": format!("正在分批归纳并生成报告…已用时 {} 分 {} 秒；无总时长上限，断线会自动重连", elapsed / 60, elapsed % 60)
                                }),
                            )
                            .await;
                        }
                    }
                }
            })
        };
        let result = async {
            let _permit = task_state
                .interactive
                .acquire()
                .await
                .map_err(anyhow::Error::from)?;
            send_event(
                &sender,
                "status",
                json!({"run_id":run_id,"message":"AI 正在查询并分析…"}),
            )
            .await;
            task_state
                .ai
                .run(
                    &history,
                    AiContext {
                        user_id: owner_id.clone(),
                        conversation_id: Some(conversation_id.clone()),
                        run_id: None,
                        allowed_tools: Vec::new(),
                        allow_large: input.allow_large,
                        email_to: None,
                    },
                    cancel,
                )
                .await
        }
        .await;
        progress_stop.cancel();
        let _ = progress_task.await;
        let completed = match result {
            Ok(outcome) => {
                let summary = serde_json::to_string(&outcome.tool_summary).unwrap_or_default();
                if let Err(error) = task_state.database.add_message(
                    &owner_id,
                    &conversation_id,
                    "assistant",
                    &outcome.text,
                    Some(&summary),
                ) {
                    let message = redact(&error.to_string());
                    eprintln!(
                        "chat run {} failed to persist result after {}s: {}",
                        &run_id[..run_id.len().min(8)],
                        run_started.elapsed().as_secs(),
                        message
                    );
                    send_event(
                        &sender,
                        "error",
                        json!({"code":"INTERNAL_ERROR","message":message,"run_id":run_id}),
                    )
                    .await;
                    CompletedChatRun {
                        owner_id: owner_id.clone(),
                        state: "error",
                        code: Some("INTERNAL_ERROR".into()),
                        message: Some(message),
                        total: None,
                        completed_at: Instant::now(),
                    }
                } else {
                    let tool_count = outcome.tool_count;
                    let _ = send_event(
                        &sender,
                        "tool",
                        json!({"items":outcome.tool_summary,"dataset_ids":outcome.dataset_ids}),
                    )
                    .await;
                    for artifact in outcome.artifacts {
                        let _ = send_event(
                            &sender,
                            "artifact",
                            serde_json::to_value(artifact).unwrap_or_default(),
                        )
                        .await;
                    }
                    let _ = send_event(&sender, "text", json!({"content":outcome.text})).await;
                    let _ = send_event(&sender, "done", json!({"run_id":run_id})).await;
                    eprintln!(
                        "chat run {} completed after {}s with {} tools",
                        &run_id[..run_id.len().min(8)],
                        run_started.elapsed().as_secs(),
                        tool_count
                    );
                    CompletedChatRun {
                        owner_id: owner_id.clone(),
                        state: "done",
                        code: None,
                        message: None,
                        total: None,
                        completed_at: Instant::now(),
                    }
                }
            }
            Err(error) => {
                let message = redact(&error.to_string());
                let (code, total) = large_error(&message);
                let public_message = if code == "LARGE_CONFIRMATION_REQUIRED" {
                    "匹配数据较多，确认后将分批分析全部记录".to_string()
                } else {
                    message.clone()
                };
                eprintln!(
                    "chat run {} failed after {}s: {}:{}",
                    &run_id[..run_id.len().min(8)],
                    run_started.elapsed().as_secs(),
                    code,
                    message
                );
                let _ = send_event(
                    &sender,
                    "error",
                    json!({"code":code,"message":public_message,"total":total,"run_id":run_id}),
                )
                .await;
                CompletedChatRun {
                    owner_id: owner_id.clone(),
                    state: "error",
                    code: Some(code.into()),
                    message: Some(public_message),
                    total,
                    completed_at: Instant::now(),
                }
            }
        };
        remember_completed_run(&task_state, run_id.clone(), completed);
        let _ = task_state.active_runs.lock().map(|mut values| {
            if values
                .get(&run_id)
                .is_some_and(|(stored_owner, _)| stored_owner == &owner_id)
            {
                values.remove(&run_id);
            }
        });
    });
    Ok(Sse::new(ReceiverStream::new(receiver)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("保持连接"),
    ))
}
async fn send_event(
    sender: &mpsc::Sender<Result<Event, std::convert::Infallible>>,
    name: &str,
    data: Value,
) {
    let event = Event::default()
        .event(name)
        .json_data(data)
        .unwrap_or_else(|_| Event::default().event("error").data("{}"));
    let _ = sender.send(Ok(event)).await;
}

fn remember_completed_run(state: &AppState, run_id: String, completed: CompletedChatRun) {
    let Ok(mut values) = state.completed_runs.lock() else {
        return;
    };
    values.retain(|_, value| value.completed_at.elapsed() < Duration::from_secs(24 * 60 * 60));
    while values.len() >= 100 {
        let Some(oldest) = values
            .iter()
            .min_by_key(|(_, value)| value.completed_at)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        values.remove(&oldest);
    }
    values.insert(run_id, completed);
}

async fn cancel_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let mut runs = state.active_runs.lock().map_err(ApiError::internal)?;
    if runs
        .get(&id)
        .is_some_and(|(owner_id, _)| owner_id == &session.admin.id)
    {
        if let Some((_, cancel)) = runs.remove(&id) {
            cancel.cancel();
        }
    }
    Ok(Json(json!({"ok":true})))
}

async fn chat_run_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    let active = state
        .active_runs
        .lock()
        .map_err(ApiError::internal)?
        .get(&id)
        .is_some_and(|(owner_id, _)| owner_id == &session.admin.id);
    if active {
        return Ok(Json(json!({"active":true,"state":"running"})));
    }
    let completed = state
        .completed_runs
        .lock()
        .map_err(ApiError::internal)?
        .get(&id)
        .filter(|value| value.owner_id == session.admin.id)
        .cloned();
    Ok(Json(match completed {
        Some(value) => json!({
            "active":false,
            "state":value.state,
            "code":value.code,
            "message":value.message,
            "total":value.total
        }),
        None => json!({"active":false,"state":"unknown"}),
    }))
}

async fn list_artifacts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    Ok(Json(
        json!({"items":state.database.artifacts(&session.admin.id).map_err(ApiError::internal)?}),
    ))
}
async fn download_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let session = authorize(&state, &headers, false)?;
    let artifact = state
        .database
        .artifact(&session.admin.id, &id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("文件不存在或已过期"))?;
    let path = Path::new(&artifact.path)
        .canonicalize()
        .map_err(ApiError::internal)?;
    let root = state
        .database
        .artifact_root
        .canonicalize()
        .map_err(ApiError::internal)?;
    if !path.starts_with(root) {
        return Err(ApiError::not_found("文件路径无效"));
    }
    let bytes = std::fs::read(path).map_err(ApiError::internal)?;
    let content_type = match artifact.kind.as_str() {
        "csv" => "text/csv; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    };
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{}",
            percent_encode(&artifact.filename)
        ))
        .map_err(ApiError::internal)?,
    );
    Ok(response)
}
async fn delete_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let deleted = state
        .database
        .delete_artifact(&session.admin.id, &id)
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::not_found("文件不存在或已过期"));
    }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct ExportInput {
    conversation_id: String,
    format: String,
    dataset_id: Option<String>,
}
async fn export_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ExportInput>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let owner_id = &session.admin.id;
    let conversation = state
        .database
        .conversations(owner_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|item| item.id == input.conversation_id)
        .ok_or_else(|| ApiError::not_found("对话不存在"))?;
    let artifact = match input.format.as_str() {
        "csv" => {
            let dataset = state
                .database
                .dataset(
                    owner_id,
                    input
                        .dataset_id
                        .as_deref()
                        .ok_or_else(|| ApiError::bad("CSV 导出需要数据集"))?,
                )
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found("数据集不存在"))?;
            if dataset.conversation_id.as_deref() != Some(&conversation.id) {
                return Err(ApiError::bad("数据集不属于当前对话"));
            }
            artifacts::create_csv(
                &state.database,
                service_for_user(owner_id)?,
                &dataset,
                owner_id,
                Some(&conversation.id),
                None,
            )
            .await
            .map_err(ApiError::internal)?
        }
        "docx" | "md" => {
            let messages = state
                .database
                .messages(owner_id, &conversation.id)
                .map_err(ApiError::internal)?;
            let content = messages
                .iter()
                .rev()
                .find(|item| item.role == "assistant")
                .map(|item| item.content.as_str())
                .ok_or_else(|| ApiError::bad("当前对话还没有 AI 回答"))?;
            let datasets = state
                .database
                .datasets_for_conversation(owner_id, &conversation.id)
                .map_err(ApiError::internal)?;
            let report = artifacts::report_content(content, &datasets);
            if input.format == "docx" {
                artifacts::create_docx(
                    &state.database,
                    owner_id,
                    Some(&conversation.id),
                    None,
                    &conversation.title,
                    &report,
                )
                .map_err(ApiError::internal)?
            } else {
                artifacts::create_markdown(
                    &state.database,
                    owner_id,
                    Some(&conversation.id),
                    None,
                    &conversation.title,
                    &report,
                )
                .map_err(ApiError::internal)?
            }
        }
        _ => return Err(ApiError::bad("仅支持 csv、docx、md")),
    };
    Ok(Json(json!({"artifact":artifact})))
}

async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    Ok(Json(
        json!({"items":state.database.jobs(&session.admin.id).map_err(ApiError::internal)?}),
    ))
}
async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<JobInput>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let next = if input.enabled {
        scheduler::next_run_at(
            &input.schedule_value,
            &input.timezone,
            chrono::Utc::now().timestamp(),
        )
        .map_err(|e| ApiError::bad(e.to_string()))?
    } else {
        None
    };
    let job = state
        .database
        .save_job(&session.admin.id, None, &input, next)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(json!({"job":job})))
}
async fn update_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(input): Json<JobInput>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    if state
        .database
        .job(&session.admin.id, &id)
        .map_err(ApiError::internal)?
        .is_none()
    {
        return Err(ApiError::not_found("任务不存在"));
    }
    let next = if input.enabled {
        scheduler::next_run_at(
            &input.schedule_value,
            &input.timezone,
            chrono::Utc::now().timestamp(),
        )
        .map_err(|e| ApiError::bad(e.to_string()))?
    } else {
        None
    };
    let job = state
        .database
        .save_job(&session.admin.id, Some(&id), &input, next)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(json!({"job":job})))
}
async fn delete_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    state
        .database
        .delete_job(&session.admin.id, &id)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct PreviewInput {
    expression: String,
    #[serde(default = "default_tz")]
    timezone: String,
}
fn default_tz() -> String {
    "Asia/Shanghai".into()
}
async fn preview_schedule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreviewInput>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers, true)?;
    let values = scheduler::preview(&input.expression, &input.timezone)
        .map_err(|e| ApiError::bad(e.to_string()))?;
    Ok(Json(json!({"items":values})))
}
async fn run_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, true)?;
    let job = state
        .database
        .job(&session.admin.id, &id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("任务不存在"))?;
    let next = job.next_run_at;
    let run = state
        .database
        .mark_job_running(&job, None, next)
        .map_err(|error| ApiError::bad(error.to_string()))?;
    let run_id = run.id.clone();
    let scheduler = state.scheduler.clone();
    tokio::spawn(async move {
        if let Err(error) = scheduler.run_marked(job, run, next).await {
            eprintln!("manual job failed: {}", redact(&error.to_string()));
        }
    });
    Ok(Json(json!({"queued":true,"job_id":id,"run_id":run_id})))
}
async fn list_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = authorize(&state, &headers, false)?;
    Ok(Json(
        json!({"items":state.database.runs(&session.admin.id).map_err(ApiError::internal)?}),
    ))
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    csrf_required: bool,
) -> Result<WebSession, ApiError> {
    let token = cookie(headers, "mc_feedback_session").ok_or_else(ApiError::auth)?;
    let session = state
        .database
        .session(&token)
        .map_err(ApiError::internal)?
        .ok_or_else(ApiError::auth)?;
    if csrf_required
        && headers.get("x-csrf-token").and_then(|v| v.to_str().ok()) != Some(session.csrf.as_str())
    {
        return Err(ApiError::csrf());
    }
    Ok(session)
}

fn authorize_admin(
    state: &AppState,
    headers: &HeaderMap,
    csrf_required: bool,
) -> Result<WebSession, ApiError> {
    let session = authorize(state, headers, csrf_required)?;
    if !session.admin.is_admin() {
        return Err(ApiError::forbidden());
    }
    Ok(session)
}

fn service_for_user(user_id: &str) -> Result<Arc<Service>, ApiError> {
    Service::new_for_user(user_id)
        .map(Arc::new)
        .map_err(ApiError::internal)
}
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_string()))
}
fn core_error(error: error::AppError) -> ApiError {
    let status = match error.code {
        error::ErrorCode::AuthRequired | error::ErrorCode::SessionExpired => {
            StatusCode::UNAUTHORIZED
        }
        error::ErrorCode::SecurityVerificationRequired => StatusCode::CONFLICT,
        error::ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        error::ErrorCode::InvalidArgument => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    let code = error_code_name(error.code);
    let message = match error.remote_code {
        Some(remote_code) => format!("{}（网易代码 {}）", error.message, remote_code),
        None => error.message,
    };
    ApiError {
        status,
        code,
        message,
    }
}

fn error_code_name(code: error::ErrorCode) -> &'static str {
    match code {
        error::ErrorCode::AuthRequired => "AUTH_REQUIRED",
        error::ErrorCode::SessionExpired => "SESSION_EXPIRED",
        error::ErrorCode::SecurityVerificationRequired => "SECURITY_VERIFICATION_REQUIRED",
        error::ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
        error::ErrorCode::RateLimited => "RATE_LIMITED",
        error::ErrorCode::NetworkError => "NETWORK_ERROR",
        error::ErrorCode::RemoteApiError => "REMOTE_API_ERROR",
    }
}
fn large_error(message: &str) -> (&'static str, Option<usize>) {
    message
        .strip_prefix("LARGE_CONFIRMATION_REQUIRED:")
        .and_then(|v| v.parse().ok())
        .map(|total| ("LARGE_CONFIRMATION_REQUIRED", Some(total)))
        .unwrap_or(("AI_ERROR", None))
}
fn redact(value: &str) -> String {
    let mut result = value.replace(['\r', '\n'], " ");
    if let Some(index) = result.find("sk-") {
        result.replace_range(index..result.len().min(index + 80), "[secret]");
    }
    result.chars().take(1000).collect()
}
fn percent_encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.') {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn parse_session_input(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let token = if trimmed.contains("NTES_SESS=") {
        trimmed
            .split(';')
            .filter_map(|part| part.trim().split_once('='))
            .find_map(|(name, value)| (name == "NTES_SESS").then_some(value))
            .context("输入中没有 NTES_SESS")?
    } else {
        trimmed
    };
    if token.is_empty() || token.len() > 8192 || token.chars().any(char::is_whitespace) {
        anyhow::bail!("NTES_SESS 格式无效");
    }
    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cookie_parser_is_exact() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; mc_feedback_session=token; b=2"),
        );
        assert_eq!(
            cookie(&headers, "mc_feedback_session").as_deref(),
            Some("token")
        );
    }
    #[test]
    fn filenames_are_header_encoded() {
        assert_eq!(percent_encode("报告.docx"), "%E6%8A%A5%E5%91%8A.docx");
    }
    #[test]
    fn session_import_accepts_cookie_or_value() {
        assert_eq!(
            parse_session_input("a=1; NTES_SESS=token==; b=2").unwrap(),
            "token=="
        );
        assert_eq!(parse_session_input("token==").unwrap(), "token==");
        assert!(parse_session_input("  ").is_err());
    }

    #[test]
    fn core_errors_keep_machine_readable_netease_codes() {
        let error = core_error(error::AppError::remote(
            error::ErrorCode::SecurityVerificationRequired,
            421,
            "需要安全验证",
        ));
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "SECURITY_VERIFICATION_REQUIRED");
        assert!(error.message.contains("网易代码 421"));
    }

    #[test]
    fn pairing_codes_require_full_random_hex_tokens() {
        assert!(valid_pairing_code("0123456789abcdef0123456789abcdef"));
        assert!(!valid_pairing_code("01234567"));
        assert!(!valid_pairing_code("0123456789abcdef0123456789abcdeg"));
    }
}
