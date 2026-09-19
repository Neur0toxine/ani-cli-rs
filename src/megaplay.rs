//! MegaPlay encrypted source payload support.
//!
//! MegaPlay embeds stopped returning plaintext `sources` from
//! `/stream/getSources(New)` and now ship an AES-256-CBC encrypted `enc`
//! field instead. The key material is published in the site player client
//! script (`/lib/newclient.min.js`); known values are bundled as defaults and
//! the script is re-fetched only when the defaults stop decrypting payloads.

use std::sync::Mutex;

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use regex::Regex;
use reqwest::{Client, header};
use serde_json::Value;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
#[cfg(test)]
type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;

const DEFAULT_SOURCE_KEY: &[u8] = b"i?LMTAx0Q6,:}50U";
const DEFAULT_SOURCE_IV: &[u8] = b"W0;27ToaUpl_P%'c";

/// Decrypts an `enc` payload and injects the decoded value as the `sources`
/// member of a copy of the provider payload, leaving other fields intact.
pub(crate) fn inflate_encrypted_sources(payload: &Value, key: &[u8], iv: &[u8]) -> Option<Value> {
    if payload.get("sources").is_some() || payload.get("source").is_some() {
        return None;
    }
    let enc = payload.get("enc")?.as_str()?;
    let bytes = decrypt_payload(enc, key, iv)?;
    let decrypted: Value = serde_json::from_slice(&bytes).ok()?;
    let mut merged = payload.clone();
    let object = merged.as_object_mut()?;
    object.insert("sources".into(), decrypted);
    Some(merged)
}

/// Restores playable sources for payloads that only carry an `enc` field.
/// Falls back to refreshing the key material from the player script when the
/// bundled defaults no longer decrypt.
pub(crate) async fn recover_encrypted_sources(
    http: &Client,
    base: &str,
    key_cache: &Mutex<Option<(Vec<u8>, Vec<u8>)>>,
    payload: Value,
) -> Value {
    if payload.get("enc").and_then(Value::as_str).is_none()
        || payload.get("sources").is_some()
        || payload.get("source").is_some()
    {
        return payload;
    }
    if let Some(inflated) =
        inflate_encrypted_sources(&payload, DEFAULT_SOURCE_KEY, DEFAULT_SOURCE_IV)
    {
        return inflated;
    }
    if let Some((key, iv)) = key_cache.lock().ok().and_then(|guard| guard.clone())
        && let Some(inflated) = inflate_encrypted_sources(&payload, &key, &iv)
    {
        return inflated;
    }
    if let Some((key, iv)) = discover_source_keys(http, base).await {
        let inflated = inflate_encrypted_sources(&payload, &key, &iv);
        if let Ok(mut guard) = key_cache.lock() {
            *guard = Some((key, iv));
        }
        if let Some(inflated) = inflated {
            return inflated;
        }
    }
    payload
}

fn decrypt_payload(enc: &str, key: &[u8], iv: &[u8]) -> Option<Vec<u8>> {
    if key.is_empty() || iv.len() != 16 {
        return None;
    }
    let mut key256 = [0u8; 32];
    let length = key.len().min(32);
    key256[..length].copy_from_slice(&key[..length]);
    let bytes = URL_SAFE_NO_PAD.decode(enc.trim_end_matches('=')).ok()?;
    let decryptor = Aes256CbcDec::new_from_slices(&key256, iv).ok()?;
    decryptor.decrypt_padded_vec_mut::<Pkcs7>(&bytes).ok()
}

async fn discover_source_keys(http: &Client, base: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/lib/newclient.min.js");
    let response = http
        .get(&url)
        .header(header::REFERER, format!("{base}/"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let script = response.text().await.ok()?;
    extract_source_keys(&script)
}

fn extract_source_keys(script: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let key = Regex::new(
        r#"(?s)importKey\(\s*"raw"\s*,[^)]*?TextEncoder\)\s*\.\s*encode\(\s*"((?:[^"\\]|\\.)*)""#,
    )
    .ok()?
    .captures(script)
    .and_then(|captures| captures.get(1).map(|value| value.as_str()))
    .map(unescape_js_string)?;
    let iv_variable = Regex::new(
        r#"decrypt\(\s*\{\s*name\s*:\s*"AES-CBC"\s*,\s*iv\s*:\s*([A-Za-z_$][A-Za-z0-9_$]*)"#,
    )
    .ok()?
    .captures(script)?
    .get(1)?
    .as_str()
    .to_owned();
    let iv_pattern = format!(
        r#"(?s)\b{iv_variable}\s*=\s*\(new TextEncoder\)\s*\.\s*encode\(\s*"((?:[^"\\]|\\.)*)""#
    );
    let iv = Regex::new(&iv_pattern)
        .ok()?
        .captures(script)
        .and_then(|captures| captures.get(1).map(|value| value.as_str()))
        .map(unescape_js_string)?;
    Some((key.into_bytes(), iv.into_bytes()))
}

fn unescape_js_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => result.push('\n'),
            Some('t') => result.push('\t'),
            Some('r') => result.push('\r'),
            Some('0') => result.push('\0'),
            Some(escaped @ ('\\' | '"' | '\'' | '/')) => result.push(escaped),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }
    result
}

#[cfg(test)]
pub(crate) fn encrypt_payload_with_default_keys(plain: &[u8]) -> Option<String> {
    use aes::cipher::BlockEncryptMut;

    let mut key256 = [0u8; 32];
    let length = DEFAULT_SOURCE_KEY.len().min(32);
    key256[..length].copy_from_slice(&DEFAULT_SOURCE_KEY[..length]);
    let encryptor = Aes256CbcEnc::new_from_slices(&key256, DEFAULT_SOURCE_IV).ok()?;
    let bytes = encryptor.encrypt_padded_vec_mut::<Pkcs7>(plain);
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAYER_CLIENT_SNIPPET: &str = r#"
        a||(a=t.crypto.subtle.importKey("raw",(e=(new TextEncoder).encode("i?LMTAx0Q6,:}50U"),(n=new Uint8Array(32)).set(e.subarray(0,Math.min(32,e.length))),n),{name:"AES-CBC"},!1,["decrypt"]))
        .then(function(n){var r=(new TextEncoder).encode("W0;27ToaUpl_P%'c"),o=function(t){}(e);return t.crypto.subtle.decrypt({name:"AES-CBC",iv:r},n,o)})
    "#;

    // Captured from a live MegaPlay /stream/getSourcesNew response.
    const LIVE_ENCRYPTED_PAYLOAD: &str = "wdeBruh3qqn_i5wUNnyaPcXqidp1UWP84FfPHzGyKXCX4d-FfosOZ5XguouVTuXHNOXK9anWxzzgrZDZa3P-ghZFl27M6Ki66QlJ51rURfg1ggpcZUJhIoTig6yI_eLQu-nzUmLhsZwB_RV9aUxsUWgJX8s-zs4C7MXCOCwk7nU";

    #[test]
    fn decrypts_live_encrypted_source_payloads() {
        let payload = json!({"enc": LIVE_ENCRYPTED_PAYLOAD, "t": 1, "server": 4});
        let inflated =
            inflate_encrypted_sources(&payload, DEFAULT_SOURCE_KEY, DEFAULT_SOURCE_IV).unwrap();
        assert_eq!(
            inflated.pointer("/sources/file").and_then(Value::as_str),
            Some(
                "https://fetch.nexabloom.top/anime/2f42ab7f8446d5ae93fd593619ca181a/79c2f67d0445d1af3cacd77a4dfd4336/master.m3u8"
            )
        );
        assert_eq!(
            inflated.get("server").and_then(Value::as_u64),
            payload.get("server").and_then(Value::as_u64)
        );
    }

    #[test]
    fn rejects_payloads_encrypted_with_other_keys() {
        let payload = json!({"enc": LIVE_ENCRYPTED_PAYLOAD});
        assert!(
            inflate_encrypted_sources(&payload, b"wrong-key-material", DEFAULT_SOURCE_IV).is_none()
        );
    }

    #[test]
    fn leaves_plaintext_payloads_untouched() {
        let payload = json!({"sources": {"file": "https://example.invalid/master.m3u8"}});
        assert!(
            inflate_encrypted_sources(&payload, DEFAULT_SOURCE_KEY, DEFAULT_SOURCE_IV).is_none()
        );
        assert!(
            inflate_encrypted_sources(&json!({}), DEFAULT_SOURCE_KEY, DEFAULT_SOURCE_IV).is_none()
        );
    }

    #[test]
    fn round_trips_payloads_encrypted_for_tests() {
        let plain = br#"{"file":"https://example.invalid/master.m3u8"}"#;
        let enc = encrypt_payload_with_default_keys(plain).unwrap();
        let payload = json!({"enc": enc, "tracks": []});
        let inflated =
            inflate_encrypted_sources(&payload, DEFAULT_SOURCE_KEY, DEFAULT_SOURCE_IV).unwrap();
        assert_eq!(
            inflated.pointer("/sources/file").and_then(Value::as_str),
            Some("https://example.invalid/master.m3u8")
        );
        assert!(inflated.get("tracks").is_some());
    }

    #[test]
    fn extracts_keys_from_the_player_client_script() {
        let (key, iv) = extract_source_keys(PLAYER_CLIENT_SNIPPET).unwrap();
        assert_eq!(key, DEFAULT_SOURCE_KEY.to_vec());
        assert_eq!(iv, DEFAULT_SOURCE_IV.to_vec());
        assert!(extract_source_keys("importKey('raw', blob)").is_none());
    }
}
