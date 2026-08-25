use tokio::sync::RwLock;

const KEYRING_SERVICE: &str = "com.mcfeedback.viewer";
const KEYRING_USER: &str = "netease-session";

#[derive(Debug)]
pub(crate) struct SessionStore {
    token: RwLock<Option<String>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            token: RwLock::new(load_keyring_token()),
        }
    }

    pub fn for_user(_user_id: &str) -> Self {
        Self::new()
    }

    pub async fn get(&self) -> Option<String> {
        self.token.read().await.clone()
    }

    pub async fn install(&self, token: String) -> bool {
        *self.token.write().await = Some(token.clone());
        save_keyring_token(&token).is_ok()
    }

    pub async fn clear(&self) {
        *self.token.write().await = None;
        let _ = delete_keyring_token();
    }
}

fn entry() -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
}

fn load_keyring_token() -> Option<String> {
    entry()
        .ok()?
        .get_password()
        .ok()
        .filter(|value| !value.is_empty())
}

fn save_keyring_token(token: &str) -> Result<(), keyring::Error> {
    entry()?.set_password(token)
}

fn delete_keyring_token() -> Result<(), keyring::Error> {
    entry()?.delete_credential()
}
