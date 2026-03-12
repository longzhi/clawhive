use axum::http::HeaderMap;
use rand::{rngs::OsRng, Rng};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const KEY_PREFIX: &str = "whk_";
const KEY_RANDOM_BYTES: usize = 32;
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Generate a new API key: `whk_` + 32 crypto-random Base62 chars.
pub fn generate_api_key() -> String {
    let mut rng = OsRng;
    let mut random_part = String::with_capacity(KEY_RANDOM_BYTES);
    for _ in 0..KEY_RANDOM_BYTES {
        let idx = rng.gen_range(0..BASE62.len());
        random_part.push(BASE62[idx] as char);
    }
    format!("{KEY_PREFIX}{random_part}")
}

/// Hash an API key: returns `sha256:<hex>`.
pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    format!("sha256:{}", hex::encode(digest))
}

/// Verify API key against stored hash (sha256:xxx) or plaintext.
/// Uses constant-time comparison to prevent timing attacks.
pub fn verify_api_key(provided_key: &str, stored: &str) -> bool {
    if let Some(expected_hash) = stored.strip_prefix("sha256:") {
        let calculated_hash = hash_api_key(provided_key);
        let Some(calculated_hash_hex) = calculated_hash.strip_prefix("sha256:") else {
            return false;
        };

        return bool::from(
            expected_hash
                .as_bytes()
                .ct_eq(calculated_hash_hex.as_bytes()),
        );
    }

    bool::from(provided_key.as_bytes().ct_eq(stored.as_bytes()))
}

/// Extract API key from headers: `Authorization: Bearer <key>` or `X-API-Key: <key>`.
pub fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("authorization") {
        if let Ok(bearer) = value.to_str() {
            let mut parts = bearer.split_whitespace();
            if let (Some(scheme), Some(token)) = (parts.next(), parts.next()) {
                if scheme.eq_ignore_ascii_case("bearer") && parts.next().is_none() {
                    return Some(token.to_string());
                }
            }
        }
    }

    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{
        extract_api_key, generate_api_key, hash_api_key, verify_api_key, KEY_PREFIX,
        KEY_RANDOM_BYTES,
    };

    #[test]
    fn generate_api_key_has_whk_prefix() {
        let key = generate_api_key();
        assert!(key.starts_with(KEY_PREFIX));
        assert_eq!(key.len(), KEY_PREFIX.len() + KEY_RANDOM_BYTES);
    }

    #[test]
    fn generate_api_key_unique() {
        let key1 = generate_api_key();
        let key2 = generate_api_key();
        assert_ne!(key1, key2);
    }

    #[test]
    fn hash_api_key_produces_sha256_prefix() {
        let hash = hash_api_key("whk_test");
        assert!(hash.starts_with("sha256:"));
    }

    #[test]
    fn verify_api_key_correct_key_passes() {
        let key = format!("{}{}", KEY_PREFIX, "A".repeat(KEY_RANDOM_BYTES));
        let hashed = hash_api_key(&key);
        assert!(verify_api_key(&key, &hashed));
    }

    #[test]
    fn verify_api_key_wrong_key_fails() {
        let key = format!("{}{}", KEY_PREFIX, "A".repeat(KEY_RANDOM_BYTES));
        let other_key = format!("{}{}", KEY_PREFIX, "B".repeat(KEY_RANDOM_BYTES));
        let hashed = hash_api_key(&key);
        assert!(!verify_api_key(&other_key, &hashed));
    }

    #[test]
    fn verify_api_key_plaintext_mode() {
        let key = format!("{}{}", KEY_PREFIX, "A".repeat(KEY_RANDOM_BYTES));
        assert!(verify_api_key(&key, &key));
        assert!(!verify_api_key("whk_other", &key));
    }

    #[test]
    fn extract_key_from_bearer_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer whk_abc123"),
        );

        assert_eq!(extract_api_key(&headers).as_deref(), Some("whk_abc123"));
    }

    #[test]
    fn extract_key_from_x_api_key_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("whk_abc123"));

        assert_eq!(extract_api_key(&headers).as_deref(), Some("whk_abc123"));
    }

    #[test]
    fn extract_key_prefers_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer whk_primary"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("whk_secondary"));

        assert_eq!(extract_api_key(&headers).as_deref(), Some("whk_primary"));
    }

    #[test]
    fn extract_key_returns_none_when_missing() {
        let headers = HeaderMap::new();

        assert_eq!(extract_api_key(&headers), None);
    }
}
