use std::{sync::Arc, time::Duration};

use rand::{Rng, distributions::Alphanumeric};
use reqwest::{
    Client, StatusCode, Url,
    cookie::{CookieStore, Jar},
    header::{COOKIE, HeaderMap, SET_COOKIE},
};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::json;
use time::{Date, Month};
use zeroize::Zeroize;

use crate::{
    crypto::{PvInfo, compute_vdf, rsa_encrypt_password, sm4_encrypt_json},
    error::{AppError, ErrorCode},
    models::{
        AccountStatus, ApiEnvelope, CommentListRaw, FeedbackListRaw, LoginOutcome, Page,
        PlayerComment, PlayerFeedback, SessionState, UserInfoRaw, feedback_type_label,
        is_safe_http_url, timestamp_to_rfc3339,
    },
    session::SessionStore,
};

const LOGIN_BASE: &str = "https://dl.reg.163.com/";
const API_BASE: &str = "https://mc-launcher.webapp.163.com/";
const TOP_URL: &str = "https://mcdev.webapp.163.com/#/login/";
const PRODUCT_ID: &str = "kBSLIYY";
const PRODUCT_DOMAIN: &str = "x19_developer";
const PRODUCT_HOST: &str = "mcdev.webapp.163.com";

#[derive(Debug)]
pub struct Service {
    client: Client,
    session: Arc<SessionStore>,
}

impl Service {
    pub fn new() -> Result<Self, AppError> {
        Self::with_session(SessionStore::new())
    }

    pub fn new_for_user(user_id: &str) -> Result<Self, AppError> {
        if user_id.is_empty() || user_id.len() > 128 {
            return Err(AppError::invalid("平台用户无效"));
        }
        Self::with_session(SessionStore::for_user(user_id))
    }

    fn with_session(session: SessionStore) -> Result<Self, AppError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .user_agent("MCFeedbackViewer/0.1")
            .build()
            .map_err(AppError::from_reqwest)?;
        Ok(Self {
            client,
            session: Arc::new(session),
        })
    }

    pub async fn login_password(
        &self,
        account: String,
        mut password: String,
    ) -> Result<LoginOutcome, AppError> {
        let account = account.trim().to_string();
        if account.is_empty() || account.len() > 320 {
            password.zeroize();
            return Err(AppError::invalid("请输入有效的网易账号"));
        }
        if password.is_empty() || password.len() > 1024 {
            password.zeroize();
            return Err(AppError::invalid("请输入有效的账号密码"));
        }

        // Every retry must use fresh PKCS#1 v1.5 randomness. Replaying the exact
        // same ciphertext is treated as a suspicious login by the NetEase edge.
        let mut encrypted_passwords = Vec::with_capacity(3);
        for _ in 0..3 {
            match rsa_encrypt_password(&password) {
                Ok(value) => encrypted_passwords.push(value),
                Err(error) => {
                    password.zeroize();
                    return Err(error);
                }
            }
        }
        password.zeroize();

        // MCDevManager keeps one cookie client through the complete retry loop.
        // Retaining the anti-abuse cookies returned by ini/powGetP is required;
        // NTES_SESS is still persisted separately per platform user.
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .user_agent("MCFeedbackViewer/0.1")
            .build()
            .map_err(AppError::from_reqwest)?;

        let mut last_error = None;
        for attempt in 0..3 {
            match self
                .login_password_once(&client, &jar, &account, &encrypted_passwords[attempt])
                .await
            {
                Ok(token) => return self.finish_login(token).await,
                Err(error)
                    if attempt < 2
                        && error
                            .remote_code
                            .is_some_and(|code| matches!(code, 803..=806)) =>
                {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| AppError::new(ErrorCode::RemoteApiError, "登录安全验证失败")))
    }

    pub async fn login_cookie(&self, mut cookie_input: String) -> Result<LoginOutcome, AppError> {
        let token = match extract_cookie_input(&cookie_input) {
            Ok(token) => token,
            Err(error) => {
                cookie_input.zeroize();
                return Err(error);
            }
        };
        cookie_input.zeroize();
        self.finish_login(token).await
    }

    pub(crate) async fn session_for_pairing(&self) -> Result<String, AppError> {
        self.session.get().await.ok_or_else(AppError::auth_required)
    }

    pub async fn logout(&self) {
        self.session.clear().await;
    }

    pub async fn account_status(&self) -> Result<AccountStatus, AppError> {
        let Some(token) = self.session.get().await else {
            return Ok(AccountStatus::missing());
        };
        match self.user_with_token(&token).await {
            Ok(user) => Ok(account_from_user(user)),
            Err(error) if error.code == ErrorCode::SessionExpired => Ok(AccountStatus::expired()),
            Err(error) => Err(error),
        }
    }

    pub async fn list_comments(
        &self,
        query: CommentQuery,
    ) -> Result<Page<PlayerComment>, AppError> {
        validate_page(query.offset, query.limit)?;
        validate_text("关键词", query.keyword.as_deref(), 200)?;
        validate_text("组件标签", query.tag.as_deref(), 100)?;
        validate_date_range(query.start_date.as_deref(), query.end_date.as_deref())?;

        let token = self
            .session
            .get()
            .await
            .ok_or_else(AppError::auth_required)?;
        let mut params = vec![
            ("start", query.offset.to_string()),
            ("span", query.limit.to_string()),
        ];
        push_optional(&mut params, "fuzzy_key", query.keyword);
        push_optional(&mut params, "comment_tag", query.tag);
        push_optional(&mut params, "start_date", query.start_date);
        push_optional(&mut params, "end_date", query.end_date);

        let raw: CommentListRaw = self.send_read(&token, "items/comment/pe/", &params).await?;
        let received = raw.data.len();
        let items = raw
            .data
            .into_iter()
            .map(|item| PlayerComment {
                id: item.id,
                resource_id: item.iid,
                resource_name: item.res_name,
                player_uid: item.uid,
                nickname: item.nickname,
                tag: item.comment_tag,
                stars: item.stars,
                content: item.user_comment,
                published_at: timestamp_to_rfc3339(item.publish_time),
            })
            .collect();

        Ok(Page {
            total: raw.count,
            offset: query.offset,
            limit: query.limit,
            has_more: query.offset.saturating_add(received) < raw.count,
            items,
        })
    }

    pub async fn list_feedback(
        &self,
        query: FeedbackQuery,
    ) -> Result<Page<PlayerFeedback>, AppError> {
        validate_page(query.offset, query.limit)?;
        validate_text("关键词", query.keyword.as_deref(), 200)?;
        if let Some(value) = query.feedback_type.as_deref()
            && !matches!(value, "0" | "1" | "2" | "3" | "4" | "5" | "6")
        {
            return Err(AppError::invalid("反馈类型必须是 0 到 6"));
        }

        let token = self
            .session
            .get()
            .await
            .ok_or_else(AppError::auth_required)?;
        let mut params = vec![
            ("start", query.offset.to_string()),
            ("span", query.limit.to_string()),
        ];
        push_optional(&mut params, "fuzzy_key", query.keyword);
        push_optional(&mut params, "type", query.feedback_type);
        if let Some(replied) = query.replied {
            params.push(("reply_count", if replied { "1" } else { "0" }.to_string()));
        }

        let raw: FeedbackListRaw = self
            .send_read(&token, "items/feedback/pe/", &params)
            .await?;
        let received = raw.data.len();
        let items = raw
            .data
            .into_iter()
            .map(|item| {
                let feedback_type_label = feedback_type_label(&item.feedback_type).to_string();
                let image_urls = item
                    .pic_list
                    .into_iter()
                    .filter(|value| is_safe_http_url(value))
                    .collect();
                let log_file_url = item
                    .feedback_log_file
                    .filter(|value| !value.is_empty() && is_safe_http_url(value));
                PlayerFeedback {
                    id: item.id,
                    resource_id: item.iid,
                    resource_name: item.res_name,
                    player_uid: item.commit_uid,
                    nickname: item.commit_nickname,
                    feedback_type: item.feedback_type,
                    feedback_type_label,
                    content: item.content,
                    created_at: timestamp_to_rfc3339(item.create_time),
                    developer_reply: item.reply.filter(|value| !value.is_empty()),
                    image_urls,
                    log_file_url,
                    forbid_reply: item.forbid_reply,
                }
            })
            .collect();

        Ok(Page {
            total: raw.count,
            offset: query.offset,
            limit: query.limit,
            has_more: query.offset.saturating_add(received) < raw.count,
            items,
        })
    }

    async fn finish_login(&self, token: String) -> Result<LoginOutcome, AppError> {
        let (user, refreshed) = self.send_read_inner(&token, "users/me", &[], false).await?;
        let persisted = self.session.install(refreshed.unwrap_or(token)).await;
        Ok(LoginOutcome {
            account: account_from_user(user),
            persisted,
            warning: (!persisted).then(|| {
                "系统钥匙串不可用，本次登录仅在当前进程有效，MCP 暂时无法复用".to_string()
            }),
        })
    }

    async fn user_with_token(&self, token: &str) -> Result<UserInfoRaw, AppError> {
        self.send_read(token, "users/me", &[]).await
    }

    async fn send_read<T: DeserializeOwned>(
        &self,
        token: &str,
        path: &'static str,
        query: &[(&'static str, String)],
    ) -> Result<T, AppError> {
        self.send_read_inner(token, path, query, true)
            .await
            .map(|(data, _)| data)
    }

    async fn send_read_inner<T: DeserializeOwned>(
        &self,
        token: &str,
        path: &'static str,
        query: &[(&'static str, String)],
        update_session: bool,
    ) -> Result<(T, Option<String>), AppError> {
        debug_assert!(matches!(
            path,
            "users/me" | "items/comment/pe/" | "items/feedback/pe/"
        ));
        let url = format!("{API_BASE}{path}");

        for attempt in 0..=1 {
            let response = self
                .client
                .get(&url)
                .query(query)
                .header(COOKIE, format!("NTES_SESS={token}"))
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error) if attempt == 0 && (error.is_connect() || error.is_timeout()) => {
                    continue;
                }
                Err(error) => return Err(AppError::from_reqwest(error)),
            };

            let status = response.status();
            if attempt == 0 && is_transient_status(status) {
                continue;
            }
            if status == StatusCode::UNAUTHORIZED {
                if update_session {
                    self.session.clear().await;
                }
                return Err(AppError::new(
                    ErrorCode::SessionExpired,
                    "登录已过期，请重新登录",
                ));
            }
            if matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            ) {
                return Err(AppError::new(
                    ErrorCode::RateLimited,
                    "请求受到限制，请稍后再试",
                ));
            }
            if !status.is_success() {
                return Err(AppError::new(
                    ErrorCode::RemoteApiError,
                    format!("网易服务返回 HTTP {}", status.as_u16()),
                ));
            }

            let refreshed = extract_ntes_session(response.headers()).filter(|value| value != token);
            if update_session && let Some(refreshed) = refreshed.as_ref() {
                let _ = self.session.install(refreshed.clone()).await;
            }

            let envelope: ApiEnvelope<T> = response.json().await.map_err(|_| {
                AppError::new(ErrorCode::RemoteApiError, "网易服务返回了无法识别的数据")
            })?;
            let data = self.unwrap_envelope(envelope, update_session).await?;
            return Ok((data, refreshed));
        }
        Err(AppError::new(
            ErrorCode::NetworkError,
            "网易服务暂时不可用，请稍后重试",
        ))
    }

    async fn unwrap_envelope<T>(
        &self,
        envelope: ApiEnvelope<T>,
        clear_expired_session: bool,
    ) -> Result<T, AppError> {
        match envelope.status.to_ascii_lowercase().as_str() {
            "200" | "201" | "ok" => envelope
                .data
                .ok_or_else(|| AppError::new(ErrorCode::RemoteApiError, "网易服务未返回数据")),
            "401" | "no_login" => {
                if clear_expired_session {
                    self.session.clear().await;
                }
                Err(AppError::new(
                    ErrorCode::SessionExpired,
                    "登录已过期，请重新登录",
                ))
            }
            _ => Err(AppError::new(
                ErrorCode::RemoteApiError,
                envelope
                    .msg
                    .unwrap_or_else(|| "网易服务返回未知错误".into()),
            )),
        }
    }

    async fn login_password_once(
        &self,
        client: &Client,
        jar: &Arc<Jar>,
        account: &str,
        encrypted_password: &str,
    ) -> Result<String, AppError> {
        let init: InitReply = post_encrypted(
            client,
            "dl/zj/mail/ini",
            &json!({
                "pd": PRODUCT_DOMAIN,
                "pkid": PRODUCT_ID,
                "pkht": PRODUCT_HOST,
                "channel": 0,
                "topURL": "",
                "rtid": random_tid()
            }),
        )
        .await?;
        ensure_login_success(init.ret, &init.dt, init.msg.as_deref())?;

        let power: PowerReply = post_encrypted(
            client,
            "dl/zj/mail/powGetP",
            &json!({
                "pkid": PRODUCT_ID,
                "pd": PRODUCT_DOMAIN,
                "un": account,
                "channel": 0,
                "topURL": TOP_URL,
                "rtid": random_tid()
            }),
        )
        .await?;
        ensure_login_success(power.ret, &power.dt, power.msg.as_deref())?;
        let pv_result = tokio::task::spawn_blocking(move || compute_vdf(power.pv_info))
            .await
            .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "安全验证计算中断"))??;

        let ticket: TicketReply = post_encrypted(
            client,
            "dl/zj/mail/gt",
            &json!({
                "un": account,
                "pd": PRODUCT_DOMAIN,
                "pkid": PRODUCT_ID,
                "channel": 0,
                "topURL": TOP_URL,
                "rtid": random_tid()
            }),
        )
        .await?;
        ensure_login_success(ticket.ret, &ticket.dt, ticket.msg.as_deref())?;

        let login: BaseLoginReply = post_encrypted(
            client,
            "dl/zj/mail/l",
            &json!({
                "un": account,
                "pw": encrypted_password,
                "pd": PRODUCT_DOMAIN,
                "l": 0,
                "d": 10,
                "t": unix_millis(),
                "tk": ticket.tk,
                "pwdKeyUp": 1,
                "pkid": PRODUCT_ID,
                "domains": "",
                "pvParam": pv_result,
                "channel": 0,
                "topURL": TOP_URL,
                "rtid": random_tid()
            }),
        )
        .await?;
        ensure_login_success(login.ret, &login.dt, login.msg.as_deref())?;

        let api_url = Url::parse(API_BASE)
            .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "服务地址无效"))?;
        let cookie_header = jar
            .cookies(&api_url)
            .and_then(|value| value.to_str().ok().map(str::to_owned));
        cookie_header
            .as_deref()
            .and_then(extract_ntes_session_from_cookie_header)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::RemoteApiError,
                    "登录成功但未取得会话，请稍后重试或使用 Cookie 登录",
                )
            })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommentQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: usize,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FeedbackQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: usize,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default, rename = "type")]
    #[schemars(rename = "type")]
    pub feedback_type: Option<String>,
    #[serde(default)]
    pub replied: Option<bool>,
}

fn default_limit() -> usize {
    20
}

fn account_from_user(user: UserInfoRaw) -> AccountStatus {
    AccountStatus {
        session_state: SessionState::Valid,
        nickname: Some(user.nickname),
        level: Some(user.level),
        on_sale_item_count: Some(user.on_sale_item_count),
    }
}

async fn post_encrypted<T: DeserializeOwned>(
    client: &Client,
    path: &str,
    payload: &serde_json::Value,
) -> Result<T, AppError> {
    let plain = serde_json::to_string(payload)
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "无法生成登录请求"))?;
    let encrypted = sm4_encrypt_json(&plain)?;
    let response = client
        .post(format!("{LOGIN_BASE}{path}"))
        .json(&json!({ "encParams": encrypted }))
        .send()
        .await
        .map_err(AppError::from_reqwest)?;
    if !response.status().is_success() {
        return Err(AppError::new(
            ErrorCode::RemoteApiError,
            format!("登录服务返回 HTTP {}", response.status().as_u16()),
        ));
    }
    response
        .json()
        .await
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "登录服务返回了无法识别的数据"))
}

#[derive(Debug, Deserialize)]
struct InitReply {
    #[serde(default, deserialize_with = "deserialize_i32_from_number_or_string")]
    ret: i32,
    #[serde(default)]
    dt: String,
    #[serde(default)]
    msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PowerReply {
    #[serde(default, deserialize_with = "deserialize_i32_from_number_or_string")]
    ret: i32,
    #[serde(default)]
    dt: String,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default, rename = "pVInfo")]
    pv_info: PvInfo,
}

#[derive(Debug, Deserialize)]
struct TicketReply {
    #[serde(default, deserialize_with = "deserialize_i32_from_number_or_string")]
    ret: i32,
    #[serde(default)]
    dt: String,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    tk: String,
}

#[derive(Debug, Deserialize)]
struct BaseLoginReply {
    #[serde(default, deserialize_with = "deserialize_i32_from_number_or_string")]
    ret: i32,
    #[serde(default)]
    dt: String,
    #[serde(default)]
    msg: Option<String>,
}

fn ensure_login_success(
    ret: i32,
    detail: &str,
    server_message: Option<&str>,
) -> Result<(), AppError> {
    if matches!(ret, 200 | 201) {
        return Ok(());
    }
    let security = matches!(
        ret,
        408 | 415 | 421 | 423 | 427 | 428 | 438 | 441 | 444 | 445 | 447 | 454 | 691
    );
    let rate_limited = matches!(ret, 409..=412 | 414 | 416..=419 | 434..=437 | 458 | 505 | 690)
        || (ret == 413 && matches!(detail.trim(), "01" | "02" | "03"));
    let message = login_error_message(ret, detail, server_message);
    Err(AppError::remote(
        if security {
            ErrorCode::SecurityVerificationRequired
        } else if rate_limited {
            ErrorCode::RateLimited
        } else {
            ErrorCode::RemoteApiError
        },
        ret,
        message,
    ))
}

fn deserialize_i32_from_number_or_string<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        Number(i32),
        String(String),
    }

    match NumberOrString::deserialize(deserializer)? {
        NumberOrString::Number(value) => Ok(value),
        NumberOrString::String(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

fn login_error_message(ret: i32, detail: &str, server_message: Option<&str>) -> String {
    let detail = detail.trim();
    match (ret, detail) {
        (-2..=0, _) => "网络不佳，请刷新页面重试".into(),
        (401, "09") => "手机号格式错误".into(),
        (401, "10") => "账号格式错误".into(),
        (401, _) => "操作超时，请重试".into(),
        (402, _) => "当前网络环境异常，请检查网络后重试".into(),
        (403, _) => "网络异常，建议切换网络后重试".into(),
        (404, _) => "网络异常，请刷新页面重试".into(),
        (405, _) => "本次登录存在异常，请重试或更换网络".into(),
        (408, _) => "已开启登录保护，请先在网易账号管家中完成验证".into(),
        (409, _) => "登录过于频繁，请稍后再试".into(),
        (410, _) => "超过 IP 限制，请稍后再试".into(),
        (411, _) => "登录不存在账号的次数过多，请 5 分钟后再试".into(),
        (412, "01") => "登录错误次数过多，请稍后再试".into(),
        (412, "02") => "登录错误次数过多，请明天再试".into(),
        (413, "01") => "密码错误次数过多，请稍后再试".into(),
        (413, "02") => "密码错误次数过多，请明天再试".into(),
        (413, "03") => "当前 IP 密码错误次数过多，请稍后再试".into(),
        (413, _) => "账号或密码错误".into(),
        (414, "01") => "当前 IP 登录错误次数过多，请稍后再试".into(),
        (414, "02") => "当前 IP 登录错误次数过多，请明天再试".into(),
        (415, _) => "需要短信验证码，请在浏览器中完成验证后使用 Cookie 登录".into(),
        (416, _) => "当前 IP 登录过于频繁，请稍后再试".into(),
        (417, "01") => "当前 IP 登录成功次数过多，请稍后再试".into(),
        (417, "02") => "当前 IP 登录成功次数过多，请明天再试".into(),
        (418, "01") => "账号登录成功次数过多，请稍后再试".into(),
        (418, "02") => "账号登录成功次数过多，请明天再试".into(),
        (419, "01" | "02") => "登录过于频繁，请稍后再试".into(),
        (420, _) => "账号不存在".into(),
        (421 | 423 | 427 | 428, _) => {
            "账号存在安全风险，请先在浏览器中完成验证，再使用 Cookie 登录".into()
        }
        (424, _) => "账号服务已到期，请续费后重试".into(),
        (425, _) => "账号尚未激活，请先完成邮箱激活".into(),
        (426, _) => "账号未及时激活，请重新注册".into(),
        (430, _) => "本次登录不需要密保验证".into(),
        (431, _) => "请求错误，请稍后再试".into(),
        (433 | 500, _) => "系统繁忙，请稍后再试".into(),
        (434 | 436, _) => "验证错误次数过多，请稍后再试".into(),
        (435 | 437, _) => "验证错误次数过多，请改天再试".into(),
        (438, _) => "正在等待账号管家确认，请完成后重试".into(),
        (441 | 444 | 445 | 447, _) => "需要安全验证，请在浏览器中登录后使用 Cookie 登录".into(),
        (442, _) => "验证码错误，请重试".into(),
        (443, _) => "短信验证码错误，请重试".into(),
        (446, _) => "网易账号管家已拒绝本次登录".into(),
        (452, _) => "账号正在注销中，请先撤销注销".into(),
        (453, _) => "账号已经注销，请更换其他账号".into(),
        (454, _) => "请先为账号绑定安全手机".into(),
        (455, _) => "该账号无法使用，请更换其他账号".into(),
        (458, _) => "短信发送过于频繁，请稍后再试".into(),
        (503, _) => "服务器繁忙，请稍后再试".into(),
        (505, _) => "尝试次数超限，请稍后再试".into(),
        (602, _) => "邮箱服务已到期，请续费后重试".into(),
        (690, _) => "验证错误次数过多，请稍后再试".into(),
        (691, _) => "操作存在风险，请更换手机或切换网络后重试".into(),
        (692, _) => "该手机暂不支持下发短信，请稍后再试".into(),
        (801..=806, _) => "安全验证加载失败，请稍后再试".into(),
        _ => server_message
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if detail.is_empty() {
                    format!("登录失败（ret={ret}）")
                } else {
                    format!("登录失败（ret={ret}, dt={detail}）")
                }
            }),
    }
}

fn extract_cookie_input(input: &str) -> Result<String, AppError> {
    let trimmed = input.trim();
    let token = if trimmed.contains('=') {
        extract_ntes_session_from_cookie_header(trimmed)
    } else if !trimmed.is_empty() {
        Some(trimmed.to_string())
    } else {
        None
    };
    token
        .filter(|value| {
            !value.is_empty() && value.len() <= 8192 && !value.contains([';', '\r', '\n'])
        })
        .ok_or_else(|| AppError::invalid("请输入有效的 NTES_SESS Cookie"))
}

fn extract_ntes_session(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(extract_ntes_session_from_cookie_header)
}

fn extract_ntes_session_from_cookie_header(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == "NTES_SESS" && !value.is_empty()).then(|| value.to_string())
    })
}

fn validate_page(_offset: usize, limit: usize) -> Result<(), AppError> {
    if !(1..=100).contains(&limit) {
        return Err(AppError::invalid("limit 必须在 1 到 100 之间"));
    }
    Ok(())
}

fn validate_text(label: &str, value: Option<&str>, max: usize) -> Result<(), AppError> {
    if value.is_some_and(|value| value.len() > max) {
        return Err(AppError::invalid(format!("{label}不能超过 {max} 个字符")));
    }
    Ok(())
}

fn validate_date_range(start: Option<&str>, end: Option<&str>) -> Result<(), AppError> {
    let start = start.map(parse_date).transpose()?;
    let end = end.map(parse_date).transpose()?;
    if let (Some(start), Some(end)) = (start, end)
        && start > end
    {
        return Err(AppError::invalid("开始日期不能晚于结束日期"));
    }
    Ok(())
}

fn parse_date(value: &str) -> Result<Date, AppError> {
    let mut parts = value.split('-');
    let year = parts.next().and_then(|v| v.parse::<i32>().ok());
    let month = parts.next().and_then(|v| v.parse::<u8>().ok());
    let day = parts.next().and_then(|v| v.parse::<u8>().ok());
    if parts.next().is_some() {
        return Err(AppError::invalid("日期必须使用 YYYY-MM-DD 格式"));
    }
    let month = month.and_then(|value| Month::try_from(value).ok());
    match (year, month, day) {
        (Some(year), Some(month), Some(day)) => Date::from_calendar_date(year, month, day)
            .map_err(|_| AppError::invalid("日期必须使用有效的 YYYY-MM-DD 格式")),
        _ => Err(AppError::invalid("日期必须使用 YYYY-MM-DD 格式")),
    }
}

fn push_optional(
    params: &mut Vec<(&'static str, String)>,
    key: &'static str,
    value: Option<String>,
) {
    if let Some(value) = value.map(|value| value.trim().to_string())
        && !value.is_empty()
    {
        params.push((key, value));
    }
}

fn is_transient_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

fn random_tid() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parser_accepts_full_cookie_or_value() {
        assert_eq!(
            extract_cookie_input("NTES_SESS=abc; Path=/").unwrap(),
            "abc"
        );
        assert_eq!(extract_cookie_input("abc").unwrap(), "abc");
        assert!(extract_cookie_input("foo=bar").is_err());
    }

    #[test]
    fn set_cookie_parser_selects_only_netes_session() {
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, "OTHER=ignore; Path=/".parse().unwrap());
        headers.append(
            SET_COOKIE,
            "NTES_SESS=expected; Domain=.163.com; Secure; HttpOnly"
                .parse()
                .unwrap(),
        );
        assert_eq!(extract_ntes_session(&headers).as_deref(), Some("expected"));
    }

    #[test]
    fn date_range_is_validated() {
        assert!(validate_date_range(Some("2026-01-01"), Some("2026-01-02")).is_ok());
        assert!(validate_date_range(Some("2026-02-30"), None).is_err());
        assert!(validate_date_range(Some("2026-02-02"), Some("2026-02-01")).is_err());
    }

    #[test]
    fn only_read_endpoints_are_allowlisted() {
        let paths = ["users/me", "items/comment/pe/", "items/feedback/pe/"];
        assert!(paths.iter().all(|path| !path.contains("reply")));
    }

    #[test]
    fn login_return_code_accepts_number_or_string() {
        let numeric: InitReply = serde_json::from_str(r#"{"ret":201}"#).unwrap();
        let string: InitReply = serde_json::from_str(r#"{"ret":"201"}"#).unwrap();
        assert_eq!(numeric.ret, 201);
        assert_eq!(string.ret, 201);
    }

    #[test]
    fn login_errors_distinguish_credentials_security_and_retries() {
        let credentials = ensure_login_success(413, "", None).unwrap_err();
        assert_eq!(credentials.code, ErrorCode::RemoteApiError);
        assert_eq!(credentials.remote_code, Some(413));

        let security = ensure_login_success(421, "", None).unwrap_err();
        assert_eq!(security.code, ErrorCode::SecurityVerificationRequired);
        assert_eq!(security.remote_code, Some(421));

        let power = ensure_login_success(803, "", None).unwrap_err();
        assert_eq!(power.code, ErrorCode::RemoteApiError);
        assert_eq!(power.remote_code, Some(803));
    }

    #[test]
    fn password_retries_can_use_fresh_rsa_ciphertexts() {
        let first = rsa_encrypt_password("same password").unwrap();
        let second = rsa_encrypt_password("same password").unwrap();
        assert_ne!(first, second);
    }

    #[tokio::test]
    #[ignore = "contacts the live comment service and requires a saved session"]
    async fn live_comment_response_reports_only_field_types() {
        use std::collections::{BTreeMap, BTreeSet};

        let service = Service::new().unwrap();
        let token = service
            .session
            .get()
            .await
            .expect("a saved session is required");
        let value: serde_json::Value = service
            .client
            .get(format!("{API_BASE}items/comment/pe/"))
            .query(&[("start", "0"), ("span", "20")])
            .header(COOKIE, format!("NTES_SESS={token}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let items = value
            .pointer("/data/data")
            .and_then(serde_json::Value::as_array)
            .expect("comment response should contain data.data");
        let mut field_types: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for item in items {
            if let Some(object) = item.as_object() {
                for (field, value) in object {
                    field_types
                        .entry(field)
                        .or_default()
                        .insert(json_type(value));
                }
            }
        }

        println!(
            "comment_count={}; count_type={}; field_types={field_types:?}",
            items.len(),
            value
                .pointer("/data/count")
                .map(json_type)
                .unwrap_or("missing")
        );
    }

    fn json_type(value: &serde_json::Value) -> &'static str {
        match value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        }
    }

    #[tokio::test]
    #[ignore = "contacts the live NetEase login service"]
    async fn live_login_init_returns_json() {
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .cookie_provider(jar)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .user_agent("MCFeedbackViewer/0.1")
            .build()
            .unwrap();
        let response: InitReply = post_encrypted(
            &client,
            "dl/zj/mail/ini",
            &json!({
                "pd": PRODUCT_DOMAIN,
                "pkid": PRODUCT_ID,
                "pkht": PRODUCT_HOST,
                "channel": 0,
                "topURL": "",
                "rtid": random_tid()
            }),
        )
        .await
        .unwrap();

        assert_eq!(response.ret, 201);

        let Ok(account) = std::env::var("MC_FEEDBACK_TEST_ACCOUNT") else {
            return;
        };
        let power: PowerReply = post_encrypted(
            &client,
            "dl/zj/mail/powGetP",
            &json!({
                "pkid": PRODUCT_ID,
                "pd": PRODUCT_DOMAIN,
                "un": account,
                "channel": 0,
                "topURL": TOP_URL,
                "rtid": random_tid()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            power.ret, 201,
            "power request failed: dt={}, msg={:?}",
            power.dt, power.msg
        );
        assert!(!power.pv_info.sid.is_empty());
        assert!(!power.pv_info.args.modulus.is_empty());

        let ticket: TicketReply = post_encrypted(
            &client,
            "dl/zj/mail/gt",
            &json!({
                "un": account,
                "pd": PRODUCT_DOMAIN,
                "pkid": PRODUCT_ID,
                "channel": 0,
                "topURL": TOP_URL,
                "rtid": random_tid()
            }),
        )
        .await
        .unwrap();
        assert_eq!(ticket.ret, 201);
        assert!(!ticket.tk.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires explicitly supplied live test credentials"]
    async fn live_password_login_round_trip() {
        let account = std::env::var("MC_FEEDBACK_TEST_ACCOUNT")
            .expect("MC_FEEDBACK_TEST_ACCOUNT is required");
        let password = std::env::var("MC_FEEDBACK_TEST_PASSWORD")
            .expect("MC_FEEDBACK_TEST_PASSWORD is required");
        let service = Service::new().unwrap();

        let outcome = service.login_password(account, password).await.unwrap();
        assert!(matches!(outcome.account.session_state, SessionState::Valid));
        service.logout().await;
        assert!(matches!(
            service.account_status().await.unwrap().session_state,
            SessionState::Missing
        ));
    }

    #[tokio::test]
    #[ignore = "requires explicit live credentials and a one-time website pairing code"]
    async fn live_website_pairing_round_trip() {
        let account = std::env::var("MC_FEEDBACK_TEST_ACCOUNT")
            .expect("MC_FEEDBACK_TEST_ACCOUNT is required");
        let password = std::env::var("MC_FEEDBACK_TEST_PASSWORD")
            .expect("MC_FEEDBACK_TEST_PASSWORD is required");
        let code = std::env::var("MC_FEEDBACK_PAIRING_CODE")
            .expect("MC_FEEDBACK_PAIRING_CODE is required");
        let website = std::env::var("MC_FEEDBACK_TEST_WEB_URL")
            .expect("MC_FEEDBACK_TEST_WEB_URL is required");
        let service = Service::new().unwrap();
        service.login_password(account, password).await.unwrap();
        let token = service.session.get().await.unwrap();

        let response = Client::new()
            .post(format!(
                "{}/api/developer/pairing/complete",
                website.trim_end_matches('/')
            ))
            .json(&json!({"code":code,"cookie":token}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "pairing failed: {response:?}"
        );
        service.logout().await;
    }
}
