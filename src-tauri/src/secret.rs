//! 速达资源「备注」等本机敏感字段的落盘加密。
//!
//! - Windows：DPAPI（用户作用域 + 应用熵），落盘格式 `dp1:<base64>`。
//! - macOS：AES-256-GCM，主密钥（32 字节随机）存放在**数据根之外**的用户锚点
//!   `~/Library/Application Support/x-hub/.remark-key`（0600），落盘格式
//!   `mac1:<base64(nonce‖密文)>`。
//!
//! 两平台的语义一致：SQLite 与备份文件里不存在明文；密钥不随数据目录/备份迁移——
//! 把数据目录挪到其它机器或其它用户账户后解不开，读取时按无备注处理，绝不毒化整表加载。
//! 无密文前缀的历史/手工值按明文原样透传（见 unprotect）；带**异平台**前缀的值
//! （如 macOS 读到 `dp1:`）恒返回 None，与「跨机器恢复的残值」同路径。

use base64::Engine as _;

/// 落盘前缀：标识 DPAPI 密文（Windows 写入；双平台读取识别）
const PREFIX_DP: &str = "dp1:";
/// 落盘前缀：标识 AES-GCM 密文（macOS 写入；双平台读取识别）
const PREFIX_MAC: &str = "mac1:";
/// 当前平台写入用的前缀
#[cfg(target_os = "windows")]
const PREFIX: &str = PREFIX_DP;
#[cfg(target_os = "macos")]
const PREFIX: &str = PREFIX_MAC;
/// 应用熵 / AAD：阻止其它程序按「无熵」直接解开本应用库里的备注
const ENTROPY: &[u8] = b"x-hub/resource-remark/v1";

/// 加密明文 → `<prefix><base64>`；空串由调用方归一为 NULL，不走这里。
pub fn protect(plain: &str) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    let cipher = dpapi_protect(plain.as_bytes())?;
    #[cfg(target_os = "macos")]
    let cipher = mac_protect(plain.as_bytes())?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let cipher: Vec<u8> = return Err("备注加密仅支持 Windows/macOS".into());
    Ok(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(cipher)
    ))
}

/// 解密落盘值 → 明文。
/// - 带密文前缀：解不开（跨机器/跨用户/跨平台/数据损坏）返回 None
/// - 无前缀值按明文透传（兼容手工改库）
pub fn unprotect(stored: &str) -> Option<String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    if let Some(blob) = stored.strip_prefix(PREFIX_DP) {
        let bytes = b64.decode(blob).ok()?;
        let plain = dpapi_unprotect(&bytes)?;
        return String::from_utf8(plain).ok();
    }
    if let Some(blob) = stored.strip_prefix(PREFIX_MAC) {
        let bytes = b64.decode(blob).ok()?;
        let plain = mac_unprotect(&bytes)?;
        return String::from_utf8(plain).ok();
    }
    (!stored.is_empty()).then(|| stored.to_string())
}

#[cfg(target_os = "windows")]
fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let src = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &src,
            PCWSTR::null(),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| format!("DPAPI 加密失败: {e}"))?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        Ok(bytes)
    }
}

#[cfg(target_os = "windows")]
fn dpapi_unprotect(cipher: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let src = CRYPT_INTEGER_BLOB {
        cbData: cipher.len() as u32,
        pbData: cipher.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&src, None, Some(&entropy), None, None, 0, &mut out).ok()?;
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        Some(bytes)
    }
}

// macOS 上读取 Windows 来源数据（dp1: 前缀）恒解不开，按无备注处理
#[cfg(not(target_os = "windows"))]
fn dpapi_unprotect(_cipher: &[u8]) -> Option<Vec<u8>> {
    None
}

// Windows 上读取 macOS 来源数据（mac1: 前缀）恒解不开，按无备注处理
#[cfg(not(target_os = "macos"))]
fn mac_unprotect(_blob: &[u8]) -> Option<Vec<u8>> {
    None
}

// ---- macOS：AES-256-GCM + 用户锚点主密钥 ----

#[cfg(target_os = "macos")]
fn mac_protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    let key = master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("密钥初始化失败: {e}"))?;
    let mut nonce_bytes = [0u8; 12];
    getrandom::fill(&mut nonce_bytes).map_err(|e| format!("随机数生成失败: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plain,
                aad: ENTROPY,
            },
        )
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

#[cfg(target_os = "macos")]
fn mac_unprotect(blob: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    if blob.len() < 12 {
        return None;
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let key = master_key().ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ct,
                aad: ENTROPY,
            },
        )
        .ok()
}

/// macOS 主密钥：`~/Library/Application Support/x-hub/.remark-key`（base64，0600）。
/// 首次使用时生成；密钥锚点在数据根之外——改数据目录/拷备份到别的机器都带不走密钥，
/// 与 DPAPI 的「绑定用户账户」语义对齐（同机同用户可解，跨机/跨用户按无备注处理）。
#[cfg(target_os = "macos")]
fn master_key() -> Result<[u8; 32], String> {
    use std::io::Write as _;

    let dir = dirs::data_dir()
        .ok_or_else(|| "无法定位用户目录".to_string())?
        .join("x-hub");
    let path = dir.join(".remark-key");
    let b64 = base64::engine::general_purpose::STANDARD;

    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(bytes) = b64.decode(content.trim()) {
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
        }
        // 存在但损坏：报错而非静默重建——重建会让旧密文全部失效且无提示
        return Err("备注主密钥文件损坏（.remark-key）".into());
    }

    // 生成新密钥并落盘（0600：仅本用户可读）
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| format!("随机数生成失败: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建密钥目录失败: {e}"))?;
    #[allow(unused_mut)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("创建密钥文件失败: {e}"))?
    };
    file.write_all(b64.encode(key).as_bytes())
        .map_err(|e| format!("写入密钥失败: {e}"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_stores_only_ciphertext() {
        let stored = protect("p@ssw0rd 密码123").unwrap();
        assert!(stored.starts_with(PREFIX));
        assert!(!stored.contains("p@ssw0rd"), "落盘值不得含明文");
        assert_eq!(unprotect(&stored).as_deref(), Some("p@ssw0rd 密码123"));
    }

    #[test]
    fn same_plaintext_encrypts_differently() {
        // 每次取随机 nonce/会话密钥：同明文两次落盘密文不同（防密文比对推断）
        let a = protect("same").unwrap();
        let b = protect("same").unwrap();
        assert_ne!(a, b);
        assert_eq!(unprotect(&a).as_deref(), Some("same"));
        assert_eq!(unprotect(&b).as_deref(), Some("same"));
    }

    #[test]
    fn undecryptable_and_plain_values() {
        // 跨机器/跨用户恢复的残值解不开 → None（按无备注处理，不报错）
        let foreign = format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(b"short")
        );
        assert_eq!(unprotect(&foreign), None);
        assert_eq!(unprotect("dp1:!!!not-base64!!!"), None);
        // 异平台密文前缀恒 None（macOS 读 Windows 数据 / 反之）
        assert_eq!(unprotect("mac1:AAAA"), None);
        assert_eq!(unprotect("dp1:AAAA"), None);
        // 无前缀 = 明文透传（手工改库兼容）；空串按无备注
        assert_eq!(unprotect("plain-text").as_deref(), Some("plain-text"));
        assert_eq!(unprotect(""), None);
    }
}
