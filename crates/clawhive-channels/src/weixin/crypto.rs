use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, Result};
use base64::Engine;

const BLOCK_SIZE: usize = 16;

/// AES-128-ECB encrypt with PKCS7 padding.
pub fn aes_ecb_encrypt(key: &[u8; 16], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes128::new(key.into());

    // PKCS7 padding
    let pad_len = BLOCK_SIZE - (plaintext.len() % BLOCK_SIZE);
    let mut padded = Vec::with_capacity(plaintext.len() + pad_len);
    padded.extend_from_slice(plaintext);
    padded.resize(plaintext.len() + pad_len, pad_len as u8);

    // Encrypt each block in-place
    for chunk in padded.chunks_exact_mut(BLOCK_SIZE) {
        let block = aes::Block::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }

    Ok(padded)
}

/// AES-128-ECB decrypt with PKCS7 unpadding.
pub fn aes_ecb_decrypt(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != BLOCK_SIZE {
        return Err(anyhow!("AES key must be 16 bytes, got {}", key.len()));
    }
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(BLOCK_SIZE) {
        return Err(anyhow!(
            "ciphertext length {} is not a multiple of block size",
            ciphertext.len()
        ));
    }

    let key_arr: [u8; 16] = key.try_into().map_err(|_| anyhow!("invalid key length"))?;
    let cipher = Aes128::new(&key_arr.into());

    let mut buf = ciphertext.to_vec();
    for chunk in buf.chunks_exact_mut(BLOCK_SIZE) {
        let block = aes::Block::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }

    // Remove PKCS7 padding
    let pad_byte = *buf.last().ok_or_else(|| anyhow!("empty decrypted data"))? as usize;
    if pad_byte == 0 || pad_byte > BLOCK_SIZE {
        return Err(anyhow!("invalid PKCS7 padding value: {pad_byte}"));
    }
    if buf.len() < pad_byte {
        return Err(anyhow!("padding exceeds data length"));
    }
    // Verify all padding bytes
    for &b in &buf[buf.len() - pad_byte..] {
        if b as usize != pad_byte {
            return Err(anyhow!("invalid PKCS7 padding"));
        }
    }
    buf.truncate(buf.len() - pad_byte);

    Ok(buf)
}

/// Calculate the encrypted (PKCS7-padded) size for a given plaintext size.
#[allow(dead_code)] // used by send_image and tests; will be re-exported when bot.rs is added
pub fn aes_ecb_padded_size(plaintext_size: u64) -> u64 {
    let bs = BLOCK_SIZE as u64;
    let pad = bs - (plaintext_size % bs);
    plaintext_size + pad
}

/// Parse an AES key from base64.
///
/// The iLink API uses two encodings:
/// 1. Raw 16 bytes directly base64-encoded.
/// 2. Hex-encoded 32 chars base64-encoded (decode base64 → hex string → decode hex → 16 bytes).
pub fn parse_aes_key(aes_key_b64: &str) -> Result<Vec<u8>> {
    let engine = base64::engine::general_purpose::STANDARD;
    let decoded = engine
        .decode(aes_key_b64)
        .map_err(|e| anyhow!("base64 decode failed: {e}"))?;

    if decoded.len() == BLOCK_SIZE {
        // Raw 16 bytes
        return Ok(decoded);
    }

    // Try hex-in-base64: decoded bytes are ASCII hex characters
    if decoded.len() == 32 {
        let hex_str = std::str::from_utf8(&decoded)
            .map_err(|_| anyhow!("base64-decoded 32 bytes are not valid UTF-8 hex"))?;
        let key = hex::decode(hex_str)
            .map_err(|e| anyhow!("hex decode of base64 payload failed: {e}"))?;
        if key.len() == BLOCK_SIZE {
            return Ok(key);
        }
    }

    Err(anyhow!(
        "unexpected AES key length after base64 decode: {} bytes",
        decoded.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key: [u8; 16] = [0x42; 16];
        let plaintext = b"hello, weixin ilink!";

        let ciphertext = aes_ecb_encrypt(&key, plaintext).unwrap();
        assert_ne!(&ciphertext[..], plaintext);
        assert_eq!(ciphertext.len() % BLOCK_SIZE, 0);

        let decrypted = aes_ecb_decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_exact_block_size() {
        let key: [u8; 16] = [0xAB; 16];
        // Exactly 16 bytes — PKCS7 adds a full block of padding
        let plaintext = b"0123456789abcdef";
        assert_eq!(plaintext.len(), BLOCK_SIZE);

        let ciphertext = aes_ecb_encrypt(&key, plaintext).unwrap();
        assert_eq!(ciphertext.len(), BLOCK_SIZE * 2);

        let decrypted = aes_ecb_decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn padded_size_calculation() {
        // Not a multiple of 16 → rounds up
        assert_eq!(aes_ecb_padded_size(0), 16);
        assert_eq!(aes_ecb_padded_size(1), 16);
        assert_eq!(aes_ecb_padded_size(15), 16);
        // Exact multiple → adds full block
        assert_eq!(aes_ecb_padded_size(16), 32);
        assert_eq!(aes_ecb_padded_size(32), 48);
        assert_eq!(aes_ecb_padded_size(33), 48);
    }

    #[test]
    fn parse_aes_key_raw_16_bytes() {
        let raw_key: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let engine = base64::engine::general_purpose::STANDARD;
        let encoded = engine.encode(raw_key);

        let parsed = parse_aes_key(&encoded).unwrap();
        assert_eq!(parsed, raw_key);
    }

    #[test]
    fn parse_aes_key_hex_in_base64() {
        // Hex-encode 16 bytes to get 32 hex chars, then base64-encode those chars
        let raw_key: [u8; 16] = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC,
            0xBA, 0x98,
        ];
        let hex_str = hex::encode(raw_key);
        assert_eq!(hex_str.len(), 32);

        let engine = base64::engine::general_purpose::STANDARD;
        let encoded = engine.encode(hex_str.as_bytes());

        let parsed = parse_aes_key(&encoded).unwrap();
        assert_eq!(parsed, raw_key);
    }
}
