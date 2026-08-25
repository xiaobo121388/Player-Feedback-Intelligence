use std::{env, path::PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::RwLock;

use crate::crypto_store::SecretBox;

#[derive(Debug)]
pub(crate) struct SessionStore {
    token: RwLock<Option<String>>,
    database_path: PathBuf,
    secrets: Option<SecretBox>,
    secret_key: String,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::with_secret_key("netease_session".into())
    }

    pub fn for_user(user_id: &str) -> Self {
        Self::with_secret_key(format!("netease_session:{user_id}"))
    }

    fn with_secret_key(secret_key: String) -> Self {
        let database_path = env::var_os("MC_WEB_DATABASE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("mc-feedback-web.sqlite"));
        let secrets = env::var_os("MC_WEB_MASTER_KEY_FILE")
            .map(PathBuf::from)
            .and_then(|path| SecretBox::from_file(&path).ok());
        let token = secrets
            .as_ref()
            .and_then(|secret_box| load_token(&database_path, secret_box, &secret_key));
        Self {
            token: RwLock::new(token),
            database_path,
            secrets,
            secret_key,
        }
    }

    pub async fn get(&self) -> Option<String> {
        self.token.read().await.clone()
    }

    pub async fn install(&self, token: String) -> bool {
        *self.token.write().await = Some(token.clone());
        let Some(secrets) = self.secrets.as_ref() else {
            return false;
        };
        save_token(&self.database_path, secrets, &self.secret_key, &token).is_ok()
    }

    pub async fn clear(&self) {
        *self.token.write().await = None;
        if let Ok(connection) = Connection::open(&self.database_path) {
            let _ = connection.execute("DELETE FROM secrets WHERE key=?1", [&self.secret_key]);
        }
    }
}

fn load_token(path: &PathBuf, secrets: &SecretBox, secret_key: &str) -> Option<String> {
    let connection = Connection::open(path).ok()?;
    let encrypted = connection
        .query_row(
            "SELECT value FROM secrets WHERE key=?1",
            [secret_key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()??;
    secrets
        .decrypt(&encrypted)
        .ok()
        .filter(|value| !value.is_empty())
}

fn save_token(
    path: &PathBuf,
    secrets: &SecretBox,
    secret_key: &str,
    token: &str,
) -> anyhow::Result<()> {
    let encrypted = secrets.encrypt(token)?;
    let connection = Connection::open(path)?;
    connection.execute(
        "INSERT INTO secrets(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![secret_key, encrypted],
    )?;
    Ok(())
}
