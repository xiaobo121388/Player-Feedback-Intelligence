use std::fmt;
use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};

#[derive(Clone)]
pub struct SecretBox {
    cipher: XChaCha20Poly1305,
}

impl fmt::Debug for SecretBox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBox([protected])")
    }
}

impl SecretBox {
    pub fn from_file(path: &Path) -> Result<Self> {
        let key = fs::read(path).with_context(|| format!("无法读取主密钥：{}", path.display()))?;
        if key.len() != 32 {
            bail!("主密钥必须恰好为 32 字节");
        }
        Ok(Self {
            cipher: XChaCha20Poly1305::new_from_slice(&key)
                .map_err(|_| anyhow::anyhow!("主密钥无效"))?,
        })
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut nonce_bytes = [0u8; 24];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce_bytes), plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("秘密加密失败"))?;
        let mut packed = nonce_bytes.to_vec();
        packed.extend(ciphertext);
        Ok(STANDARD.encode(packed))
    }

    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        let packed = STANDARD.decode(encoded).context("秘密编码无效")?;
        if packed.len() < 25 {
            bail!("秘密数据无效");
        }
        let plaintext = self
            .cipher
            .decrypt(XNonce::from_slice(&packed[..24]), &packed[24..])
            .map_err(|_| anyhow::anyhow!("秘密解密失败"))?;
        String::from_utf8(plaintext).context("秘密不是 UTF-8 文本")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_values_round_trip_without_plaintext() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
        fs::write(&path, [7u8; 32]).unwrap();
        let secrets = SecretBox::from_file(&path).unwrap();
        let encrypted = secrets.encrypt("敏感值").unwrap();
        assert!(!encrypted.contains("敏感值"));
        assert_eq!(secrets.decrypt(&encrypted).unwrap(), "敏感值");
    }
}
