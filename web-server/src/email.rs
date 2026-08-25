use anyhow::{Context, Result, bail};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Attachment, Mailbox, MultiPart, SinglePart, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use serde::{Deserialize, Serialize};

use crate::db::Database;

pub struct PublicSendError {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmtpView {
    pub host: String,
    pub port: u16,
    pub security: String,
    pub username: String,
    pub from_email: String,
    pub from_name: String,
    pub password_configured: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmtpInput {
    pub host: String,
    pub port: u16,
    pub security: String,
    pub username: String,
    pub password: Option<String>,
    pub from_email: String,
    pub from_name: String,
}

#[derive(Debug, Clone)]
struct SmtpConfig {
    host: String,
    port: u16,
    security: String,
    username: String,
    password: String,
    from_email: String,
    from_name: String,
}

pub fn view(database: &Database) -> Result<SmtpView> {
    Ok(SmtpView {
        host: database.setting("smtp_host")?.unwrap_or_default(),
        port: database
            .setting("smtp_port")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(465),
        security: database
            .setting("smtp_security")?
            .unwrap_or_else(|| "smtps".into()),
        username: database.setting("smtp_username")?.unwrap_or_default(),
        from_email: database.setting("smtp_from_email")?.unwrap_or_default(),
        from_name: database
            .setting("smtp_from_name")?
            .unwrap_or_else(|| "MC 玩家反馈助手".into()),
        password_configured: database.secret("smtp_password")?.is_some(),
    })
}

pub fn save(database: &Database, input: &SmtpInput) -> Result<SmtpView> {
    validate(input)?;
    database.set_setting("smtp_host", input.host.trim())?;
    database.set_setting("smtp_port", &input.port.to_string())?;
    database.set_setting("smtp_security", &input.security)?;
    database.set_setting("smtp_username", input.username.trim())?;
    database.set_setting("smtp_from_email", input.from_email.trim())?;
    database.set_setting("smtp_from_name", input.from_name.trim())?;
    if let Some(password) = input.password.as_deref().filter(|value| !value.is_empty()) {
        database.set_secret("smtp_password", password)?;
    }
    view(database)
}

pub async fn send(
    database: &Database,
    owner_id: Option<&str>,
    recipient: &str,
    subject: &str,
    body: &str,
    artifact_ids: &[String],
) -> Result<()> {
    if !looks_like_email(recipient)
        || subject.contains(['\r', '\n'])
        || subject.chars().count() > 200
    {
        bail!("邮件地址或主题无效");
    }
    if body.chars().count() > 200_000 || artifact_ids.len() > 5 {
        bail!("邮件正文或附件数量超过限制");
    }
    let config = load(database)?;
    let from = Mailbox::new(
        (!config.from_name.is_empty()).then_some(config.from_name.clone()),
        config.from_email.parse().context("发件地址无效")?,
    );
    let mut multipart = MultiPart::mixed().singlepart(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string()),
    );
    let mut total = 0usize;
    for id in artifact_ids {
        let owner_id = owner_id.context("邮件附件缺少所属用户")?;
        let artifact = database
            .artifact(owner_id, id)?
            .context("邮件附件不存在或已过期")?;
        let bytes = std::fs::read(&artifact.path)?;
        total = total.saturating_add(bytes.len());
        if total > 10 * 1024 * 1024 {
            bail!("邮件附件合计超过 10MB");
        }
        multipart = multipart.singlepart(
            Attachment::new(artifact.filename)
                .body(bytes, ContentType::parse("application/octet-stream")?),
        );
    }
    let message = Message::builder()
        .from(from)
        .to(recipient.parse().context("收件地址无效")?)
        .subject(subject)
        .multipart(multipart)?;
    let credentials = Credentials::new(config.username, config.password);
    let builder = match config.security.as_str() {
        "smtps" => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host),
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host),
        _ => bail!("只允许 SMTPS 或 STARTTLS"),
    }
    .context("SMTP 服务器配置无效")?;
    builder
        .port(config.port)
        .credentials(credentials)
        .timeout(Some(std::time::Duration::from_secs(30)))
        .build()
        .send(message)
        .await
        .context("SMTP 发送失败")?;
    Ok(())
}

fn load(database: &Database) -> Result<SmtpConfig> {
    let view = view(database)?;
    if view.host.is_empty() || view.username.is_empty() || view.from_email.is_empty() {
        bail!("请先配置 SMTP");
    }
    Ok(SmtpConfig {
        host: view.host,
        port: view.port,
        security: view.security,
        username: view.username,
        password: database
            .secret("smtp_password")?
            .context("SMTP 密码未配置")?,
        from_email: view.from_email,
        from_name: view.from_name,
    })
}

fn validate(input: &SmtpInput) -> Result<()> {
    let host = input.host.trim();
    let username = input.username.trim();
    let from_email = input.from_email.trim();
    if input.host.trim().is_empty()
        || input.host.len() > 253
        || !matches!(input.security.as_str(), "smtps" | "starttls")
        || username.is_empty()
        || !looks_like_email(from_email)
        || input.username.len() > 320
        || input.from_name.len() > 100
    {
        bail!("SMTP 配置无效");
    }
    if matches!(
        host.to_ascii_lowercase().as_str(),
        "smtp.qq.com" | "smtp.163.com"
    ) && !username.eq_ignore_ascii_case(from_email)
    {
        bail!("QQ、163 邮箱的邮箱账号必须与发件地址一致");
    }
    Ok(())
}

pub fn public_send_error(error: &anyhow::Error) -> PublicSendError {
    let details = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
        .to_ascii_lowercase();
    if details.contains("535")
        || details.contains("authentication")
        || details.contains("login fail")
        || details.contains("credentials")
    {
        return PublicSendError {
            code: "SMTP_AUTH_FAILED",
            message: "SMTP 登录失败：请确认邮箱账号正确、已开启 SMTP 服务，并填写邮箱生成的 SMTP 授权码（不是网站或邮箱登录密码）".into(),
        };
    }
    if details.contains("timed out") || details.contains("timeout") {
        return PublicSendError {
            code: "SMTP_TIMEOUT",
            message: "连接 SMTP 服务器超时，请检查服务器地址、端口和网络".into(),
        };
    }
    if details.contains("certificate") || details.contains("tls") {
        return PublicSendError {
            code: "SMTP_TLS_FAILED",
            message: "SMTP 加密连接失败，请检查端口与连接加密方式是否匹配".into(),
        };
    }
    if details.contains("请先配置 smtp") || details.contains("smtp 密码未配置") {
        return PublicSendError {
            code: "SMTP_NOT_CONFIGURED",
            message: "请先完整填写并保存发信设置".into(),
        };
    }
    PublicSendError {
        code: "SMTP_SEND_FAILED",
        message: "邮件发送失败，请检查 SMTP 服务器、端口、加密方式和发件地址".into(),
    }
}

fn looks_like_email(value: &str) -> bool {
    let value = value.trim();
    value.len() <= 320 && value.contains('@') && !value.contains(['\r', '\n', ',', ';'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_mail_providers_require_sender_as_username() {
        let input = SmtpInput {
            host: "smtp.qq.com".into(),
            port: 465,
            security: "smtps".into(),
            username: "website-admin@example.test".into(),
            password: None,
            from_email: "123456@qq.com".into(),
            from_name: "测试".into(),
        };
        assert_eq!(
            validate(&input).unwrap_err().to_string(),
            "QQ、163 邮箱的邮箱账号必须与发件地址一致"
        );
    }

    #[test]
    fn smtp_auth_error_has_actionable_safe_message() {
        let result = public_send_error(&anyhow::anyhow!("server returned 535 Login fail"));
        assert_eq!(result.code, "SMTP_AUTH_FAILED");
        assert!(result.message.contains("SMTP 授权码"));
    }
}
