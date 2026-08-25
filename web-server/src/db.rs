use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use base64::Engine;
use chrono::Utc;
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::crypto_store::SecretBox;

const SESSION_IDLE_SECONDS: i64 = 7 * 24 * 60 * 60;
const SESSION_MAX_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
    secrets: SecretBox,
    pub artifact_root: Arc<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Admin {
    pub id: String,
    pub email: String,
    pub role: String,
    pub active: bool,
}

impl Admin {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedUser {
    pub id: String,
    pub email: String,
    pub role: String,
    pub active: bool,
    pub netease_account: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct WebSession {
    pub admin: Admin,
    pub csrf: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub tool_summary: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetSpec {
    pub id: String,
    #[serde(skip_serializing)]
    pub owner_id: String,
    pub conversation_id: Option<String>,
    pub run_id: Option<String>,
    pub kind: String,
    pub query: serde_json::Value,
    pub total: usize,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub id: String,
    #[serde(skip_serializing)]
    pub owner_id: String,
    pub conversation_id: Option<String>,
    pub run_id: Option<String>,
    pub kind: String,
    pub filename: String,
    #[serde(skip_serializing)]
    pub path: String,
    pub size: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    #[serde(skip_serializing)]
    pub owner_id: String,
    pub name: String,
    pub prompt: String,
    pub allowed_tools: Vec<String>,
    pub formats: Vec<String>,
    pub schedule_kind: String,
    pub schedule_value: String,
    pub timezone: String,
    pub enabled: bool,
    pub email_to: Option<String>,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub running: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobInput {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub formats: Vec<String>,
    pub schedule_kind: String,
    pub schedule_value: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub enabled: bool,
    pub email_to: Option<String>,
}

fn default_timezone() -> String {
    "Asia/Shanghai".to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRun {
    pub id: String,
    #[serde(skip_serializing)]
    pub owner_id: String,
    pub job_id: String,
    pub job_name: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub tool_count: i64,
    pub email_status: Option<String>,
    pub scheduled_for: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

impl Database {
    pub fn open(path: &Path, key_path: &Path, artifact_root: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(artifact_root)?;
        let connection = Connection::open(path)
            .with_context(|| format!("无法打开数据库：{}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let database = Self {
            connection: Arc::new(Mutex::new(connection)),
            secrets: SecretBox::from_file(key_path)?,
            artifact_root: Arc::new(artifact_root.to_path_buf()),
        };
        database.migrate()?;
        Ok(database)
    }

    pub fn with_conn<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("数据库锁已损坏"))?;
        operation(&guard)
    }

    fn migrate(&self) -> Result<()> {
        self.with_conn(|connection| {
            connection.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS admins (
                    id TEXT PRIMARY KEY,
                    email TEXT NOT NULL UNIQUE,
                    password_hash TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'user',
                    active INTEGER NOT NULL DEFAULT 1,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS web_sessions (
                    token_hash TEXT PRIMARY KEY,
                    admin_id TEXT NOT NULL REFERENCES admins(id) ON DELETE CASCADE,
                    csrf TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    last_seen INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS secrets (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS conversations (
                    id TEXT PRIMARY KEY,
                    owner_id TEXT REFERENCES admins(id) ON DELETE CASCADE,
                    title TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS messages (
                    id TEXT PRIMARY KEY,
                    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    tool_summary TEXT,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS datasets (
                    id TEXT PRIMARY KEY,
                    owner_id TEXT REFERENCES admins(id) ON DELETE CASCADE,
                    conversation_id TEXT,
                    run_id TEXT,
                    kind TEXT NOT NULL,
                    query_json TEXT NOT NULL,
                    total INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS artifacts (
                    id TEXT PRIMARY KEY,
                    owner_id TEXT REFERENCES admins(id) ON DELETE CASCADE,
                    conversation_id TEXT,
                    run_id TEXT,
                    kind TEXT NOT NULL,
                    filename TEXT NOT NULL,
                    path TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS jobs (
                    id TEXT PRIMARY KEY,
                    owner_id TEXT REFERENCES admins(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    allowed_tools TEXT NOT NULL,
                    formats TEXT NOT NULL,
                    schedule_kind TEXT NOT NULL,
                    schedule_value TEXT NOT NULL,
                    timezone TEXT NOT NULL,
                    enabled INTEGER NOT NULL,
                    email_to TEXT,
                    next_run_at INTEGER,
                    last_run_at INTEGER,
                    running INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS job_runs (
                    id TEXT PRIMARY KEY,
                    owner_id TEXT REFERENCES admins(id) ON DELETE CASCADE,
                    job_id TEXT NOT NULL,
                    job_name TEXT NOT NULL,
                    status TEXT NOT NULL,
                    result TEXT,
                    error TEXT,
                    tool_count INTEGER NOT NULL DEFAULT 0,
                    email_status TEXT,
                    scheduled_for INTEGER,
                    started_at INTEGER,
                    finished_at INTEGER
                );
                CREATE TABLE IF NOT EXISTS email_deliveries (
                    run_id TEXT PRIMARY KEY,
                    status TEXT NOT NULL,
                    error TEXT,
                    sent_at INTEGER
                );
                CREATE TABLE IF NOT EXISTS user_bindings (
                    user_id TEXT PRIMARY KEY REFERENCES admins(id) ON DELETE CASCADE,
                    netease_account TEXT,
                    updated_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_jobs_due ON jobs(enabled, next_run_at);
                CREATE INDEX IF NOT EXISTS idx_runs_started ON job_runs(started_at DESC);
                "#,
            )?;
            add_column_if_missing(connection, "admins", "role", "TEXT NOT NULL DEFAULT 'user'")?;
            add_column_if_missing(connection, "admins", "active", "INTEGER NOT NULL DEFAULT 1")?;
            add_column_if_missing(connection, "conversations", "owner_id", "TEXT")?;
            add_column_if_missing(connection, "datasets", "owner_id", "TEXT")?;
            add_column_if_missing(connection, "artifacts", "owner_id", "TEXT")?;
            add_column_if_missing(connection, "jobs", "owner_id", "TEXT")?;
            add_column_if_missing(connection, "job_runs", "owner_id", "TEXT")?;
            connection.execute(
                "UPDATE admins SET role='admin' WHERE id=(SELECT id FROM admins ORDER BY created_at,id LIMIT 1)",
                [],
            )?;
            connection.execute_batch(
                r#"
                CREATE INDEX IF NOT EXISTS idx_conversations_owner ON conversations(owner_id,updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_datasets_owner ON datasets(owner_id,created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_artifacts_owner ON artifacts(owner_id,created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_jobs_owner ON jobs(owner_id,created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_runs_owner ON job_runs(owner_id,started_at DESC);
                "#,
            )?;
            Ok(())
        })
    }

    pub fn ensure_admin(&self, email: &str, password: &mut String) -> Result<Admin> {
        let email = email.trim().to_ascii_lowercase();
        if email.is_empty() || password.len() < 12 {
            bail!("管理员邮箱无效或密码少于 12 个字符");
        }
        if let Some(admin) = self.primary_admin()? {
            password.zeroize();
            return Ok(admin);
        }
        let mut salt_bytes = [0u8; 16];
        OsRng.fill_bytes(&mut salt_bytes);
        let salt =
            SaltString::encode_b64(&salt_bytes).map_err(|_| anyhow::anyhow!("密码盐生成失败"))?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| anyhow::anyhow!("密码哈希失败"))?
            .to_string();
        password.zeroize();
        let now = Utc::now().timestamp();
        let admin = Admin {
            id: Uuid::new_v4().to_string(),
            email: email.clone(),
            role: "admin".into(),
            active: true,
        };
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO admins(id,email,password_hash,role,active,created_at) VALUES(?1,?2,?3,'admin',1,?4)",
                params![admin.id, admin.email, hash, now],
            )?;
            Ok(())
        })?;
        Ok(admin)
    }

    pub fn primary_admin(&self) -> Result<Option<Admin>> {
        self.with_conn(|connection| {
            Ok(connection.query_row(
                "SELECT id,email,role,active FROM admins WHERE role='admin' ORDER BY created_at,id LIMIT 1",
                [],
                admin_from_row,
            ).optional()?)
        })
    }

    pub fn create_user(&self, email: &str, password: &mut String) -> Result<ManagedUser> {
        let email = email.trim().to_ascii_lowercase();
        if !looks_like_email(&email) || password.len() < 12 || password.len() > 1024 {
            password.zeroize();
            bail!("平台邮箱无效或密码少于 12 个字符");
        }
        let hash = hash_password(password)?;
        password.zeroize();
        let user = ManagedUser {
            id: Uuid::new_v4().to_string(),
            email,
            role: "user".into(),
            active: true,
            netease_account: None,
            created_at: Utc::now().timestamp(),
        };
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO admins(id,email,password_hash,role,active,created_at) VALUES(?1,?2,?3,'user',1,?4)",
                params![user.id,user.email,hash,user.created_at],
            ).context("平台账号已存在")?;
            Ok(())
        })?;
        Ok(user)
    }

    pub fn users(&self) -> Result<Vec<ManagedUser>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare(
                "SELECT a.id,a.email,a.role,a.active,b.netease_account,a.created_at FROM admins a LEFT JOIN user_bindings b ON b.user_id=a.id ORDER BY a.created_at,a.id",
            )?;
            let rows = statement.query_map([], |row| Ok(ManagedUser {
                id: row.get(0)?, email: row.get(1)?, role: row.get(2)?, active: row.get::<_,i64>(3)? != 0,
                netease_account: row.get(4)?, created_at: row.get(5)?,
            }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_netease_account(&self, user_id: &str, account: &str) -> Result<()> {
        let account = account.trim();
        if account.is_empty() || account.len() > 320 {
            bail!("网易账号无效");
        }
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO user_bindings(user_id,netease_account,updated_at) VALUES(?1,?2,?3) ON CONFLICT(user_id) DO UPDATE SET netease_account=excluded.netease_account,updated_at=excluded.updated_at",
                params![user_id,account,Utc::now().timestamp()],
            )?;
            Ok(())
        })
    }

    pub fn netease_account(&self, user_id: &str) -> Result<Option<String>> {
        self.with_conn(|connection| {
            Ok(connection
                .query_row(
                    "SELECT netease_account FROM user_bindings WHERE user_id=?1",
                    [user_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten())
        })
    }

    pub fn claim_legacy_data(&self, admin_id: &str, account_hint: Option<&str>) -> Result<()> {
        self.with_conn(|connection| {
            for table in ["conversations", "datasets", "artifacts", "jobs", "job_runs"] {
                connection.execute(
                    &format!("UPDATE {table} SET owner_id=?1 WHERE owner_id IS NULL OR owner_id=''"),
                    [admin_id],
                )?;
            }
            let user_key = format!("netease_session:{admin_id}");
            connection.execute(
                "INSERT OR IGNORE INTO secrets(key,value) SELECT ?1,value FROM secrets WHERE key='netease_session'",
                [user_key],
            )?;
            if let Some(account) = account_hint.filter(|value| !value.trim().is_empty()) {
                connection.execute(
                    "INSERT INTO user_bindings(user_id,netease_account,updated_at) VALUES(?1,?2,?3) ON CONFLICT(user_id) DO UPDATE SET netease_account=COALESCE(user_bindings.netease_account,excluded.netease_account),updated_at=excluded.updated_at",
                    params![admin_id,account.trim(),Utc::now().timestamp()],
                )?;
            }
            Ok(())
        })
    }

    pub fn verify_admin(&self, email: &str, password: &str) -> Result<Option<Admin>> {
        let row = self.with_conn(|connection| {
            Ok(connection
                .query_row(
                    "SELECT id,email,role,active,password_hash FROM admins WHERE email=?1 AND active=1",
                    [email.trim().to_ascii_lowercase()],
                    |row| {
                        Ok((
                            Admin {
                                id: row.get(0)?,
                                email: row.get(1)?,
                                role: row.get(2)?,
                                active: row.get::<_,i64>(3)? != 0,
                            },
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?)
        })?;
        let Some((admin, encoded)) = row else {
            return Ok(None);
        };
        let hash = PasswordHash::new(&encoded).map_err(|_| anyhow::anyhow!("密码哈希无效"))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
            .then_some(admin))
    }

    pub fn create_session(&self, admin_id: &str) -> Result<(String, String)> {
        let token = random_token();
        let csrf = random_token();
        let now = Utc::now().timestamp();
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO web_sessions(token_hash,admin_id,csrf,created_at,last_seen,expires_at) VALUES(?1,?2,?3,?4,?4,?5)",
                params![token_hash(&token), admin_id, csrf, now, now + SESSION_MAX_SECONDS],
            )?;
            Ok(())
        })?;
        Ok((token, csrf))
    }

    pub fn session(&self, token: &str) -> Result<Option<WebSession>> {
        let now = Utc::now().timestamp();
        let token_hash = token_hash(token);
        self.with_conn(|connection| {
            let session = connection
                .query_row(
                    "SELECT a.id,a.email,a.role,a.active,s.csrf FROM web_sessions s JOIN admins a ON a.id=s.admin_id WHERE s.token_hash=?1 AND s.expires_at>?2 AND s.last_seen>?3 AND a.active=1",
                    params![token_hash, now, now - SESSION_IDLE_SECONDS],
                    |row| Ok(WebSession {
                        admin: Admin { id: row.get(0)?, email: row.get(1)?, role: row.get(2)?, active: row.get::<_,i64>(3)? != 0 },
                        csrf: row.get(4)?,
                    }),
                )
                .optional()?;
            if session.is_some() {
                connection.execute(
                    "UPDATE web_sessions SET last_seen=?2 WHERE token_hash=?1",
                    params![token_hash, now],
                )?;
            }
            Ok(session)
        })
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        self.with_conn(|connection| {
            connection.execute(
                "DELETE FROM web_sessions WHERE token_hash=?1",
                [token_hash(token)],
            )?;
            Ok(())
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|connection| {
            Ok(connection
                .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                    row.get(0)
                })
                .optional()?)
        })
    }

    pub fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        let encrypted = self.secrets.encrypt(value)?;
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO secrets(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, encrypted],
            )?;
            Ok(())
        })
    }

    pub fn secret(&self, key: &str) -> Result<Option<String>> {
        let encrypted = self.with_conn(|connection| {
            Ok(connection
                .query_row("SELECT value FROM secrets WHERE key=?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?)
        })?;
        encrypted
            .map(|value| self.secrets.decrypt(&value))
            .transpose()
    }

    pub fn conversations(&self, owner_id: &str) -> Result<Vec<Conversation>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare("SELECT id,title,created_at,updated_at FROM conversations WHERE owner_id=?1 ORDER BY updated_at DESC LIMIT 100")?;
            let rows = statement.query_map([owner_id], |row| Ok(Conversation {
                id: row.get(0)?, title: row.get(1)?, created_at: row.get(2)?, updated_at: row.get(3)?,
            }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn create_conversation(&self, owner_id: &str, title: &str) -> Result<Conversation> {
        let now = Utc::now().timestamp();
        let conversation = Conversation {
            id: Uuid::new_v4().to_string(),
            title: clean_title(title),
            created_at: now,
            updated_at: now,
        };
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO conversations(id,owner_id,title,created_at,updated_at) VALUES(?1,?2,?3,?4,?5)",
                params![conversation.id, owner_id, conversation.title, now, now],
            )?;
            Ok(())
        })?;
        Ok(conversation)
    }

    pub fn delete_conversation(&self, owner_id: &str, id: &str) -> Result<bool> {
        self.with_conn(|connection| {
            Ok(connection.execute(
                "DELETE FROM conversations WHERE id=?1 AND owner_id=?2",
                params![id, owner_id],
            )? == 1)
        })
    }

    pub fn messages(&self, owner_id: &str, conversation_id: &str) -> Result<Vec<Message>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare("SELECT m.id,m.conversation_id,m.role,m.content,m.tool_summary,m.created_at FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE m.conversation_id=?1 AND c.owner_id=?2 ORDER BY m.created_at,m.id")?;
            let rows = statement.query_map(params![conversation_id,owner_id], |row| Ok(Message {
                id: row.get(0)?, conversation_id: row.get(1)?, role: row.get(2)?, content: row.get(3)?, tool_summary: row.get(4)?, created_at: row.get(5)?,
            }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn add_message(
        &self,
        owner_id: &str,
        conversation_id: &str,
        role: &str,
        content: &str,
        tool_summary: Option<&str>,
    ) -> Result<Message> {
        let now = Utc::now().timestamp();
        let message = Message {
            id: Uuid::new_v4().to_string(),
            conversation_id: conversation_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            tool_summary: tool_summary.map(str::to_string),
            created_at: now,
        };
        self.with_conn(|connection| {
            let owned = connection.query_row(
                "SELECT 1 FROM conversations WHERE id=?1 AND owner_id=?2",
                params![conversation_id,owner_id],
                |_| Ok(()),
            ).optional()?.is_some();
            if !owned { bail!("对话不存在"); }
            connection.execute("INSERT INTO messages(id,conversation_id,role,content,tool_summary,created_at) VALUES(?1,?2,?3,?4,?5,?6)", params![message.id,message.conversation_id,message.role,message.content,message.tool_summary,now])?;
            connection.execute("UPDATE conversations SET updated_at=?2 WHERE id=?1 AND owner_id=?3", params![conversation_id,now,owner_id])?;
            if role == "user" {
                let title = clean_title(content);
                connection.execute(
                    "UPDATE conversations SET title=?2 WHERE id=?1 AND title='新对话' AND owner_id=?3",
                    params![conversation_id, title, owner_id],
                )?;
            }
            Ok(())
        })?;
        Ok(message)
    }

    pub fn create_dataset(
        &self,
        owner_id: &str,
        conversation_id: Option<&str>,
        run_id: Option<&str>,
        kind: &str,
        query: &serde_json::Value,
        total: usize,
    ) -> Result<DatasetSpec> {
        let dataset = DatasetSpec {
            id: Uuid::new_v4().to_string(),
            owner_id: owner_id.to_string(),
            conversation_id: conversation_id.map(str::to_string),
            run_id: run_id.map(str::to_string),
            kind: kind.to_string(),
            query: query.clone(),
            total,
            created_at: Utc::now().timestamp(),
        };
        self.with_conn(|connection| {
            if let Some(conversation_id) = conversation_id {
                let owned = connection
                    .query_row(
                        "SELECT 1 FROM conversations WHERE id=?1 AND owner_id=?2",
                        params![conversation_id, owner_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !owned {
                    bail!("对话不存在");
                }
            }
            if let Some(run_id) = run_id {
                let owned = connection
                    .query_row(
                        "SELECT 1 FROM job_runs WHERE id=?1 AND owner_id=?2",
                        params![run_id, owner_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !owned {
                    bail!("执行记录不存在");
                }
            }
            connection.execute("INSERT INTO datasets(id,owner_id,conversation_id,run_id,kind,query_json,total,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![dataset.id,dataset.owner_id,dataset.conversation_id,dataset.run_id,dataset.kind,serde_json::to_string(query)?,total as i64,dataset.created_at])?;
            Ok(())
        })?;
        Ok(dataset)
    }

    pub fn dataset(&self, owner_id: &str, id: &str) -> Result<Option<DatasetSpec>> {
        self.with_conn(|connection| {
            Ok(connection.query_row("SELECT id,owner_id,conversation_id,run_id,kind,query_json,total,created_at FROM datasets WHERE id=?1 AND owner_id=?2", params![id,owner_id], |row| {
                let query: String = row.get(5)?;
                Ok(DatasetSpec { id: row.get(0)?, owner_id: row.get(1)?, conversation_id: row.get(2)?, run_id: row.get(3)?, kind: row.get(4)?, query: serde_json::from_str(&query).unwrap_or_default(), total: row.get::<_,i64>(6)? as usize, created_at: row.get(7)? })
            }).optional()?)
        })
    }

    pub fn datasets_for_conversation(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<DatasetSpec>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare(
                "SELECT id,owner_id,conversation_id,run_id,kind,query_json,total,created_at FROM datasets WHERE conversation_id=?1 AND owner_id=?2 ORDER BY created_at DESC LIMIT 100",
            )?;
            let rows = statement.query_map(params![conversation_id,owner_id], |row| {
                let query: String = row.get(5)?;
                Ok(DatasetSpec {
                    id: row.get(0)?,
                    owner_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    run_id: row.get(3)?,
                    kind: row.get(4)?,
                    query: serde_json::from_str(&query).unwrap_or_default(),
                    total: row.get::<_, i64>(6)? as usize,
                    created_at: row.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn add_artifact(
        &self,
        owner_id: &str,
        conversation_id: Option<&str>,
        run_id: Option<&str>,
        kind: &str,
        filename: &str,
        path: &Path,
    ) -> Result<Artifact> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > 10 * 1024 * 1024 {
            bail!("生成文件超过 10MB 限制");
        }
        let now = Utc::now().timestamp();
        let artifact = Artifact {
            id: Uuid::new_v4().to_string(),
            owner_id: owner_id.to_string(),
            conversation_id: conversation_id.map(str::to_string),
            run_id: run_id.map(str::to_string),
            kind: kind.to_string(),
            filename: safe_filename(filename),
            path: path.to_string_lossy().into_owned(),
            size: metadata.len() as i64,
            created_at: now,
            expires_at: now + 30 * 24 * 60 * 60,
            download_url: String::new(),
        };
        self.with_conn(|connection| {
            if let Some(conversation_id) = conversation_id {
                let owned = connection
                    .query_row(
                        "SELECT 1 FROM conversations WHERE id=?1 AND owner_id=?2",
                        params![conversation_id, owner_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !owned {
                    bail!("对话不存在");
                }
            }
            if let Some(run_id) = run_id {
                let owned = connection
                    .query_row(
                        "SELECT 1 FROM job_runs WHERE id=?1 AND owner_id=?2",
                        params![run_id, owner_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !owned {
                    bail!("执行记录不存在");
                }
            }
            connection.execute("INSERT INTO artifacts(id,owner_id,conversation_id,run_id,kind,filename,path,size,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![artifact.id,artifact.owner_id,artifact.conversation_id,artifact.run_id,artifact.kind,artifact.filename,artifact.path,artifact.size,artifact.created_at,artifact.expires_at])?;
            Ok(())
        })?;
        Ok(with_download_url(artifact))
    }

    pub fn artifact(&self, owner_id: &str, id: &str) -> Result<Option<Artifact>> {
        self.with_conn(|connection| Ok(connection.query_row("SELECT id,owner_id,conversation_id,run_id,kind,filename,path,size,created_at,expires_at FROM artifacts WHERE id=?1 AND owner_id=?2", params![id,owner_id], artifact_from_row).optional()?.map(with_download_url)))
    }

    pub fn artifacts(&self, owner_id: &str) -> Result<Vec<Artifact>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare("SELECT id,owner_id,conversation_id,run_id,kind,filename,path,size,created_at,expires_at FROM artifacts WHERE owner_id=?1 AND expires_at>?2 ORDER BY created_at DESC LIMIT 200")?;
            let rows = statement.query_map(params![owner_id,Utc::now().timestamp()], artifact_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(with_download_url).collect())
        })
    }

    pub fn delete_artifact(&self, owner_id: &str, id: &str) -> Result<bool> {
        if let Some(artifact) = self.artifact(owner_id, id)? {
            let _ = fs::remove_file(&artifact.path);
        } else {
            return Ok(false);
        }
        self.with_conn(|connection| {
            Ok(connection.execute(
                "DELETE FROM artifacts WHERE id=?1 AND owner_id=?2",
                params![id, owner_id],
            )? == 1)
        })
    }

    pub fn cleanup(&self) -> Result<()> {
        let now = Utc::now().timestamp();
        let paths = self.with_conn(|connection| {
            let mut statement =
                connection.prepare("SELECT path FROM artifacts WHERE expires_at<=?1")?;
            Ok(statement
                .query_map([now], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for path in paths {
            let _ = fs::remove_file(path);
        }
        self.with_conn(|connection| {
            connection.execute("DELETE FROM artifacts WHERE expires_at<=?1", [now])?;
            connection.execute(
                "DELETE FROM messages WHERE created_at<=?1",
                [now - 90 * 24 * 60 * 60],
            )?;
            connection.execute(
                "DELETE FROM datasets WHERE created_at<=?1",
                [now - 90 * 24 * 60 * 60],
            )?;
            connection.execute(
                "DELETE FROM job_runs WHERE COALESCE(finished_at,started_at,0)<=?1",
                [now - 90 * 24 * 60 * 60],
            )?;
            connection.execute(
                "DELETE FROM web_sessions WHERE expires_at<=?1 OR last_seen<=?2",
                params![now, now - SESSION_IDLE_SECONDS],
            )?;
            Ok(())
        })
    }

    pub fn jobs(&self, owner_id: &str) -> Result<Vec<Job>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare("SELECT id,owner_id,name,prompt,allowed_tools,formats,schedule_kind,schedule_value,timezone,enabled,email_to,next_run_at,last_run_at,running,created_at,updated_at FROM jobs WHERE owner_id=?1 ORDER BY created_at DESC")?;
            let rows = statement.query_map([owner_id], job_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn job(&self, owner_id: &str, id: &str) -> Result<Option<Job>> {
        self.with_conn(|connection| Ok(connection.query_row("SELECT id,owner_id,name,prompt,allowed_tools,formats,schedule_kind,schedule_value,timezone,enabled,email_to,next_run_at,last_run_at,running,created_at,updated_at FROM jobs WHERE id=?1 AND owner_id=?2", params![id,owner_id], job_from_row).optional()?))
    }

    pub fn save_job(
        &self,
        owner_id: &str,
        id: Option<&str>,
        input: &JobInput,
        next_run_at: Option<i64>,
    ) -> Result<Job> {
        validate_job(input)?;
        let now = Utc::now().timestamp();
        let id = id
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.with_conn(|connection| {
            connection.execute(
                "INSERT INTO jobs(id,owner_id,name,prompt,allowed_tools,formats,schedule_kind,schedule_value,timezone,enabled,email_to,next_run_at,last_run_at,running,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,0,?13,?13) ON CONFLICT(id) DO UPDATE SET name=excluded.name,prompt=excluded.prompt,allowed_tools=excluded.allowed_tools,formats=excluded.formats,schedule_kind=excluded.schedule_kind,schedule_value=excluded.schedule_value,timezone=excluded.timezone,enabled=excluded.enabled,email_to=excluded.email_to,next_run_at=excluded.next_run_at,updated_at=excluded.updated_at WHERE jobs.owner_id=excluded.owner_id",
                params![id,owner_id,input.name.trim(),input.prompt.trim(),serde_json::to_string(&input.allowed_tools)?,serde_json::to_string(&input.formats)?,input.schedule_kind,input.schedule_value,input.timezone,input.enabled as i32,input.email_to.as_ref().map(|v|v.trim()),next_run_at,now],
            )?;
            Ok(())
        })?;
        self.job(owner_id, &id)?.context("任务保存后不存在")
    }

    pub fn delete_job(&self, owner_id: &str, id: &str) -> Result<()> {
        self.with_conn(|connection| {
            let changed = connection.execute(
                "DELETE FROM jobs WHERE id=?1 AND owner_id=?2 AND running=0",
                params![id, owner_id],
            )?;
            if changed != 1 {
                bail!("任务不存在或正在运行");
            }
            Ok(())
        })
    }

    pub fn due_jobs(&self, now: i64) -> Result<Vec<Job>> {
        self.with_conn(|connection| {
            let mut statement = connection.prepare("SELECT id,owner_id,name,prompt,allowed_tools,formats,schedule_kind,schedule_value,timezone,enabled,email_to,next_run_at,last_run_at,running,created_at,updated_at FROM jobs WHERE enabled=1 AND next_run_at IS NOT NULL AND next_run_at<=?1 ORDER BY next_run_at")?;
            let rows = statement.query_map([now], job_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn skip_overlapping_job(
        &self,
        job: &Job,
        scheduled_for: i64,
        next_run_at: Option<i64>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|connection| {
            connection.execute("INSERT INTO job_runs(id,owner_id,job_id,job_name,status,error,scheduled_for,started_at,finished_at) VALUES(?1,?2,?3,?4,'skipped','上一次运行尚未结束',?5,?6,?6)",params![Uuid::new_v4().to_string(),job.owner_id,job.id,job.name,scheduled_for,now])?;
            connection.execute("UPDATE jobs SET next_run_at=?2,updated_at=?3 WHERE id=?1",params![job.id,next_run_at,now])?;
            Ok(())
        })
    }

    pub fn mark_job_running(
        &self,
        job: &Job,
        scheduled_for: Option<i64>,
        next_run_at: Option<i64>,
    ) -> Result<JobRun> {
        let run = JobRun {
            id: Uuid::new_v4().to_string(),
            owner_id: job.owner_id.clone(),
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            status: "running".into(),
            result: None,
            error: None,
            tool_count: 0,
            email_status: None,
            scheduled_for,
            started_at: Some(Utc::now().timestamp()),
            finished_at: None,
        };
        self.with_conn(|connection| {
            let changed = connection.execute("UPDATE jobs SET running=1,last_run_at=?2,next_run_at=?3 WHERE id=?1 AND running=0", params![job.id,run.started_at,next_run_at])?;
            if changed != 1 { bail!("任务已在运行"); }
            connection.execute("INSERT INTO job_runs(id,owner_id,job_id,job_name,status,scheduled_for,started_at) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![run.id,run.owner_id,run.job_id,run.job_name,run.status,run.scheduled_for,run.started_at])?;
            Ok(())
        })?;
        Ok(run)
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        job_id: &str,
        status: &str,
        result: Option<&str>,
        error: Option<&str>,
        tool_count: usize,
        email_status: Option<&str>,
        next_run_at: Option<i64>,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|connection| {
            connection.execute("UPDATE job_runs SET status=?2,result=?3,error=?4,tool_count=MAX(tool_count,?5),email_status=?6,finished_at=?7 WHERE id=?1", params![run_id,status,result,error,tool_count as i64,email_status,now])?;
            connection.execute("UPDATE jobs SET running=0,next_run_at=?2,updated_at=?3 WHERE id=?1", params![job_id,next_run_at,now])?;
            Ok(())
        })
    }

    pub fn update_run_progress(&self, run_id: &str, tool_count: usize) -> Result<()> {
        self.with_conn(|connection| {
            connection.execute(
                "UPDATE job_runs SET tool_count=MAX(tool_count,?2) WHERE id=?1 AND status='running'",
                params![run_id, tool_count as i64],
            )?;
            Ok(())
        })
    }

    pub fn runs(&self, owner_id: &str) -> Result<Vec<JobRun>> {
        self.with_conn(|connection| {
            let mut statement=connection.prepare("SELECT id,owner_id,job_id,job_name,status,result,error,tool_count,email_status,scheduled_for,started_at,finished_at FROM job_runs WHERE owner_id=?1 ORDER BY COALESCE(started_at,0) DESC LIMIT 200")?;
            let rows=statement.query_map([owner_id], |row| Ok(JobRun { id:row.get(0)?,owner_id:row.get(1)?,job_id:row.get(2)?,job_name:row.get(3)?,status:row.get(4)?,result:row.get(5)?,error:row.get(6)?,tool_count:row.get(7)?,email_status:row.get(8)?,scheduled_for:row.get(9)?,started_at:row.get(10)?,finished_at:row.get(11)? }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn recover_interrupted_jobs(&self) -> Result<()> {
        let now = Utc::now().timestamp();
        self.with_conn(|connection| {
            connection.execute(
                "UPDATE job_runs SET status='failed',error='服务重启，运行已中断',finished_at=?1 WHERE status='running'",
                [now],
            )?;
            connection.execute("UPDATE jobs SET running=0 WHERE running=1", [])?;
            Ok(())
        })
    }

    pub fn reserve_email(&self, run_id: &str) -> Result<bool> {
        self.with_conn(|connection| {
            Ok(connection.execute(
                "INSERT OR IGNORE INTO email_deliveries(run_id,status) VALUES(?1,'sending')",
                [run_id],
            )? == 1)
        })
    }

    pub fn finish_email(&self, run_id: &str, status: &str, error: Option<&str>) -> Result<()> {
        self.with_conn(|connection| {
            connection.execute(
                "UPDATE email_deliveries SET status=?2,error=?3,sent_at=?4 WHERE run_id=?1",
                params![run_id, status, error, Utc::now().timestamp()],
            )?;
            Ok(())
        })
    }

    pub fn email_status(&self, run_id: &str) -> Result<Option<String>> {
        self.with_conn(|connection| {
            Ok(connection
                .query_row(
                    "SELECT status FROM email_deliveries WHERE run_id=?1",
                    [run_id],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        conversation_id: row.get(2)?,
        run_id: row.get(3)?,
        kind: row.get(4)?,
        filename: row.get(5)?,
        path: row.get(6)?,
        size: row.get(7)?,
        created_at: row.get(8)?,
        expires_at: row.get(9)?,
        download_url: String::new(),
    })
}

fn admin_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Admin> {
    Ok(Admin {
        id: row.get(0)?,
        email: row.get(1)?,
        role: row.get(2)?,
        active: row.get::<_, i64>(3)? != 0,
    })
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !names.iter().any(|name| name == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    let mut salt_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut salt_bytes);
    let salt =
        SaltString::encode_b64(&salt_bytes).map_err(|_| anyhow::anyhow!("密码盐生成失败"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| anyhow::anyhow!("密码哈希失败"))?
        .to_string())
}

fn with_download_url(mut artifact: Artifact) -> Artifact {
    artifact.download_url = format!("/api/artifacts/{}/download", artifact.id);
    artifact
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let tools: String = row.get(4)?;
    let formats: String = row.get(5)?;
    Ok(Job {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        name: row.get(2)?,
        prompt: row.get(3)?,
        allowed_tools: serde_json::from_str(&tools).unwrap_or_default(),
        formats: serde_json::from_str(&formats).unwrap_or_default(),
        schedule_kind: row.get(6)?,
        schedule_value: row.get(7)?,
        timezone: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        email_to: row.get(10)?,
        next_run_at: row.get(11)?,
        last_run_at: row.get(12)?,
        running: row.get::<_, i64>(13)? != 0,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn validate_job(input: &JobInput) -> Result<()> {
    if input.name.trim().is_empty() || input.name.chars().count() > 100 {
        bail!("任务名称必须为 1 到 100 个字符");
    }
    if input.prompt.trim().is_empty() || input.prompt.chars().count() > 20_000 {
        bail!("提示词必须为 1 到 20000 个字符");
    }
    if !matches!(
        input.schedule_kind.as_str(),
        "daily" | "weekly" | "monthly" | "cron"
    ) {
        bail!("定时类型无效");
    }
    if input.schedule_value.len() > 120 || input.timezone.len() > 80 {
        bail!("定时配置过长");
    }
    if !input.allowed_tools.iter().all(|value| {
        matches!(
            value.as_str(),
            "get_account_status" | "list_player_comments" | "list_player_feedback"
        )
    }) {
        bail!("任务包含不允许的数据工具");
    }
    if !input
        .formats
        .iter()
        .all(|value| matches!(value.as_str(), "csv" | "docx" | "md"))
    {
        bail!("任务包含不支持的文件格式");
    }
    if input
        .email_to
        .as_ref()
        .is_some_and(|value| !looks_like_email(value))
    {
        bail!("收件地址无效");
    }
    Ok(())
}

fn looks_like_email(value: &str) -> bool {
    let value = value.trim();
    value.len() <= 320 && value.contains('@') && !value.contains(['\r', '\n', ',', ';'])
}
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
fn clean_title(value: &str) -> String {
    let value = value.trim();
    let title: String = value.chars().take(40).collect();
    if title.is_empty() {
        "新对话".into()
    } else {
        title
    }
}
pub fn safe_filename(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(100)
        .collect();
    if cleaned.trim_matches('.').is_empty() {
        "报告".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_secrets_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("key"), [5u8; 32]).unwrap();
        let db = Database::open(
            &directory.path().join("db.sqlite"),
            &directory.path().join("key"),
            &directory.path().join("files"),
        )
        .unwrap();
        let mut password = "a-very-long-test-password".to_string();
        db.ensure_admin("admin@example.test", &mut password)
            .unwrap();
        let admin = db
            .verify_admin("admin@example.test", "a-very-long-test-password")
            .unwrap()
            .unwrap();
        let (token, csrf) = db.create_session(&admin.id).unwrap();
        assert_eq!(db.session(&token).unwrap().unwrap().csrf, csrf);
        db.set_secret("api", "secret").unwrap();
        assert_eq!(db.secret("api").unwrap().as_deref(), Some("secret"));
    }

    #[test]
    fn user_owned_records_are_strictly_isolated() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("key"), [7u8; 32]).unwrap();
        let db = Database::open(
            &directory.path().join("db.sqlite"),
            &directory.path().join("key"),
            &directory.path().join("files"),
        )
        .unwrap();
        let mut admin_password = "admin-test-password".to_string();
        let admin = db
            .ensure_admin("admin@example.test", &mut admin_password)
            .unwrap();
        let mut user_password = "normal-test-password".to_string();
        let user = db
            .create_user("user@example.test", &mut user_password)
            .unwrap();

        let admin_conversation = db.create_conversation(&admin.id, "管理员对话").unwrap();
        let user_conversation = db.create_conversation(&user.id, "用户对话").unwrap();
        db.add_message(
            &admin.id,
            &admin_conversation.id,
            "assistant",
            "管理员私有评论摘要",
            None,
        )
        .unwrap();
        db.add_message(
            &user.id,
            &user_conversation.id,
            "assistant",
            "用户私有评论摘要",
            None,
        )
        .unwrap();
        let admin_dataset = db
            .create_dataset(
                &admin.id,
                Some(&admin_conversation.id),
                None,
                "comments",
                &serde_json::json!({"keyword":"admin-only"}),
                1,
            )
            .unwrap();

        assert_eq!(db.conversations(&admin.id).unwrap().len(), 1);
        assert_eq!(db.conversations(&user.id).unwrap().len(), 1);
        assert!(
            db.messages(&user.id, &admin_conversation.id)
                .unwrap()
                .is_empty()
        );
        assert!(db.dataset(&user.id, &admin_dataset.id).unwrap().is_none());
        assert!(
            !db.delete_conversation(&user.id, &admin_conversation.id)
                .unwrap()
        );
        assert_eq!(
            db.messages(&admin.id, &admin_conversation.id).unwrap()[0].content,
            "管理员私有评论摘要"
        );
    }

    #[test]
    fn legacy_rows_and_session_are_claimed_by_primary_admin_only() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("key"), [8u8; 32]).unwrap();
        let db = Database::open(
            &directory.path().join("db.sqlite"),
            &directory.path().join("key"),
            &directory.path().join("files"),
        )
        .unwrap();
        let mut admin_password = "admin-test-password".to_string();
        let admin = db
            .ensure_admin("admin@example.test", &mut admin_password)
            .unwrap();
        let mut user_password = "normal-test-password".to_string();
        let user = db
            .create_user("user@example.test", &mut user_password)
            .unwrap();
        db.set_secret("netease_session", "legacy-cookie").unwrap();
        db.with_conn(|connection| {
            connection.execute(
                "INSERT INTO conversations(id,owner_id,title,created_at,updated_at) VALUES('legacy',NULL,'旧对话',1,1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        db.claim_legacy_data(&admin.id, Some("developer@example.test"))
            .unwrap();

        assert_eq!(db.conversations(&admin.id).unwrap()[0].id, "legacy");
        assert!(db.conversations(&user.id).unwrap().is_empty());
        assert_eq!(
            db.secret(&format!("netease_session:{}", admin.id))
                .unwrap()
                .as_deref(),
            Some("legacy-cookie")
        );
        assert!(
            db.secret(&format!("netease_session:{}", user.id))
                .unwrap()
                .is_none()
        );
        let users = db.users().unwrap();
        assert_eq!(
            users
                .iter()
                .find(|value| value.id == admin.id)
                .and_then(|value| value.netease_account.as_deref()),
            Some("developer@example.test")
        );
    }

    #[test]
    fn old_single_admin_schema_upgrades_without_losing_rows() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db.sqlite");
        fs::write(directory.path().join("key"), [6u8; 32]).unwrap();
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE admins (
                    id TEXT PRIMARY KEY,
                    email TEXT NOT NULL UNIQUE,
                    password_hash TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE conversations (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO admins(id,email,password_hash,created_at)
                    VALUES('legacy-admin','admin@example.test','unused',1);
                INSERT INTO conversations(id,title,created_at,updated_at)
                    VALUES('legacy-conversation','旧对话',1,1);
                "#,
            )
            .unwrap();
        drop(connection);

        let db = Database::open(
            &database_path,
            &directory.path().join("key"),
            &directory.path().join("files"),
        )
        .unwrap();
        let admin = db.primary_admin().unwrap().unwrap();
        assert_eq!(admin.id, "legacy-admin");
        assert!(admin.is_admin());
        db.claim_legacy_data(&admin.id, Some("developer@example.test"))
            .unwrap();
        assert_eq!(
            db.conversations(&admin.id).unwrap()[0].id,
            "legacy-conversation"
        );
    }
}
