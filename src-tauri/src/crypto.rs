use std::time::Instant;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ecb::cipher::{BlockModeEncrypt, KeyInit, block_padding::Pkcs7};
use num_bigint::BigUint;
use rand::rngs::OsRng;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey, pkcs8::DecodePublicKey};
use serde::{Deserialize, Serialize};
use sm4::Sm4;

use crate::error::{AppError, ErrorCode};

const NETEASE_RSA_PUBLIC_KEY: &str = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQC5gsH+AA4XWONB5TDcUd+xCz7ejOFHZKlcZDx+pF1i7Gsvi1vjyJoQhRtRSn950x498VUkx7rUxg1/ScBVfrRxQOZ8xFBye3pjAzfb22+RCuYApSVpJ3OO3KsEuKExftz9oFBv3ejxPlYc5yq7YiBO8XlTnQN0Sa4R4qhPO3I2MQIDAQAB";
const NETEASE_SM4_KEY: &str = "BC60B8B9E4FFEFFA219E5AD77F11F9E2";

type Sm4EcbEncryptor = ecb::Encryptor<Sm4>;

pub(crate) fn rsa_encrypt_password(password: &str) -> Result<String, AppError> {
    rsa_encrypt_with_key(password.as_bytes(), NETEASE_RSA_PUBLIC_KEY)
}

fn rsa_encrypt_with_key(input: &[u8], public_key_base64: &str) -> Result<String, AppError> {
    let der = STANDARD
        .decode(public_key_base64)
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "登录公钥格式无效"))?;
    let public_key = RsaPublicKey::from_public_key_der(&der)
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "无法载入登录公钥"))?;
    let encrypted = public_key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, input)
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "密码加密失败"))?;
    Ok(STANDARD.encode(encrypted))
}

pub(crate) fn sm4_encrypt_json(input: &str) -> Result<String, AppError> {
    let key = hex::decode(NETEASE_SM4_KEY)
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "登录密钥格式无效"))?;
    let encrypted = Sm4EcbEncryptor::new_from_slice(&key)
        .map_err(|_| AppError::new(ErrorCode::RemoteApiError, "登录密钥长度无效"))?
        .encrypt_padded_vec::<Pkcs7>(input.as_bytes());
    Ok(hex::encode(encrypted))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PvInfo {
    #[serde(default)]
    pub sid: String,
    #[serde(default)]
    pub args: PvArgs,
    #[serde(default)]
    pub max_time: i32,
    #[serde(default)]
    pub min_time: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct PvArgs {
    #[serde(default, rename = "mod")]
    pub modulus: String,
    #[serde(default)]
    pub t: i32,
    #[serde(default)]
    pub puzzle: String,
    #[serde(default)]
    pub x: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PvResult {
    pub max_time: i32,
    pub puzzle: String,
    pub spend_time: i32,
    pub run_times: i32,
    pub sid: String,
    pub args: String,
}

pub(crate) fn compute_vdf(info: PvInfo) -> Result<PvResult, AppError> {
    let modulus = BigUint::parse_bytes(info.args.modulus.as_bytes(), 16)
        .filter(|value| *value != BigUint::from(0u8))
        .ok_or_else(|| AppError::new(ErrorCode::RemoteApiError, "安全验证模数无效"))?;
    let mut x = BigUint::parse_bytes(info.args.x.as_bytes(), 16)
        .ok_or_else(|| AppError::new(ErrorCode::RemoteApiError, "安全验证参数无效"))?;
    let target_rounds = info.args.t.max(0) as u32;
    let min_time = info.min_time.max(0) as u128;
    let max_time = info.max_time.max(0) as u128;
    let started = Instant::now();
    let mut rounds = 0u32;

    while rounds < target_rounds || started.elapsed().as_millis() < min_time {
        x = (&x * &x) % &modulus;
        rounds = rounds.saturating_add(1);
        if max_time > 0 && started.elapsed().as_millis() > max_time {
            break;
        }
    }

    let spend_time = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
    let x_hex = x.to_str_radix(16);
    let signing = format!("runTimes={rounds}&spendTime={spend_time}&t={rounds}&x={x_hex}");
    let sign = murmur_hash3_x86_32(signing.as_bytes(), rounds);

    Ok(PvResult {
        max_time: info.max_time,
        puzzle: info.args.puzzle,
        spend_time,
        run_times: rounds.min(i32::MAX as u32) as i32,
        sid: info.sid,
        args: format!(r#"{{"x":"{x_hex}","t":{rounds},"sign":"{sign}"}}"#),
    })
}

pub(crate) fn murmur_hash3_x86_32(bytes: &[u8], seed: u32) -> u32 {
    const C1: u32 = 0xcc9e2d51;
    const C2: u32 = 0x1b873593;
    let mut hash = seed;
    let chunks = bytes.chunks_exact(4);
    let remainder = chunks.remainder();

    for chunk in chunks {
        let mut value = u32::from_le_bytes(chunk.try_into().expect("four byte chunk"));
        value = value.wrapping_mul(C1);
        value = value.rotate_left(15);
        value = value.wrapping_mul(C2);
        hash ^= value;
        hash = hash.rotate_left(13);
        hash = hash.wrapping_mul(5).wrapping_add(0xe6546b64);
    }

    let mut tail = 0u32;
    for (index, byte) in remainder.iter().enumerate() {
        tail |= (*byte as u32) << (index * 8);
    }
    if !remainder.is_empty() {
        tail = tail.wrapping_mul(C1);
        tail = tail.rotate_left(15);
        tail = tail.wrapping_mul(C2);
        hash ^= tail;
    }

    hash ^= bytes.len() as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85ebca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2ae35);
    hash ^= hash >> 16;
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use ecb::cipher::{Block, BlockCipherEncrypt};
    use pretty_assertions::assert_eq;
    use rand::{SeedableRng, rngs::StdRng};
    use rsa::{RsaPrivateKey, pkcs8::EncodePublicKey};

    #[test]
    fn sm4_matches_the_standard_single_block_vector() {
        let key = hex::decode("0123456789abcdeffedcba9876543210").unwrap();
        let input: [u8; 16] = hex::decode("0123456789abcdeffedcba9876543210")
            .unwrap()
            .try_into()
            .unwrap();
        let mut block = Block::<Sm4>::from(input);
        let cipher = Sm4::new_from_slice(&key).unwrap();
        cipher.encrypt_block(&mut block);
        assert_eq!(hex::encode(block), "681edf34d206965e86b3e94f536e4246");
    }

    #[test]
    fn murmur_hash_matches_known_vector() {
        assert_eq!(murmur_hash3_x86_32(b"hello", 0), 0x248bfa47);
    }

    #[test]
    fn rsa_pkcs1v15_ciphertext_round_trips() {
        let mut rng = StdRng::seed_from_u64(42);
        let private_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let public_key = RsaPublicKey::from(&private_key);
        let encoded = STANDARD.encode(public_key.to_public_key_der().unwrap().as_bytes());
        let ciphertext = STANDARD
            .decode(rsa_encrypt_with_key(b"secret", &encoded).unwrap())
            .unwrap();
        assert_eq!(
            private_key.decrypt(Pkcs1v15Encrypt, &ciphertext).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn vdf_runs_the_requested_rounds() {
        let result = compute_vdf(PvInfo {
            sid: "sid".into(),
            max_time: 10_000,
            min_time: 0,
            args: PvArgs {
                modulus: "11".into(),
                t: 3,
                puzzle: "puzzle".into(),
                x: "2".into(),
            },
        })
        .unwrap();
        assert_eq!(result.run_times, 3);
        assert!(result.args.contains(r#""x":"1""#));
    }
}
