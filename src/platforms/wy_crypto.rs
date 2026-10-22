//! Native NetEase request cryptography (AES PKCS#7 and RSA without padding).
use crate::platforms::types::{ProviderError, encode};
use aes::{
    Aes128,
    cipher::{Block, BlockEncrypt, KeyInit},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use serde_json::Value;

fn aes(input: &[u8], key: &[u8; 16], iv: Option<&[u8; 16]>) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).expect("AES-128 key");
    let padding = 16 - input.len() % 16;
    let mut data = input.to_vec();
    data.resize(data.len() + padding, padding as u8);
    let mut previous = iv.copied().unwrap_or([0; 16]);
    for chunk in data.as_chunks_mut::<16>().0 {
        if iv.is_some() {
            for (byte, prior) in chunk.iter_mut().zip(previous) {
                *byte ^= prior;
            }
        }
        cipher.encrypt_block(Block::<Aes128>::from_mut_slice(chunk));
        previous.copy_from_slice(chunk);
    }
    data
}
fn hex(data: &[u8], uppercase: bool) -> String {
    data.iter()
        .map(|b| {
            if uppercase {
                format!("{b:02X}")
            } else {
                format!("{b:02x}")
            }
        })
        .collect()
}

pub fn eapi(path: &str, value: &Value) -> String {
    let content = value.to_string();
    let digest = format!(
        "{:x}",
        Md5::digest(format!("nobody{path}use{content}md5forencrypt"))
    );
    let data = format!("{path}-36cd479b6b5-{content}-36cd479b6b5-{digest}");
    format!(
        "params={}",
        hex(&aes(data.as_bytes(), b"e82ckenh8dichen8", None), true)
    )
}
pub fn linuxapi(value: &Value) -> String {
    format!(
        "eparams={}",
        hex(
            &aes(value.to_string().as_bytes(), b"rFgB&h#%2?^eDg:Q", None),
            true
        )
    )
}
pub fn weapi(value: &Value) -> Result<String, ProviderError> {
    let mut secret = [0u8; 16];
    getrandom::fill(&mut secret).map_err(|e| ProviderError::Network {
        message: format!("request entropy: {e}"),
    })?;
    let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for byte in &mut secret {
        *byte = alphabet[*byte as usize % 62];
    }
    Ok(weapi_with_secret(value, secret))
}
/// Deterministic entry point also permits independent protocol-vector verification.
pub fn weapi_with_secret(value: &Value, secret: [u8; 16]) -> String {
    let first = STANDARD.encode(aes(
        value.to_string().as_bytes(),
        b"0CoJUm6Qyw8W8jud",
        Some(b"0102030405060708"),
    ));
    let params = STANDARD.encode(aes(first.as_bytes(), &secret, Some(b"0102030405060708")));
    let mut reversed = secret;
    reversed.reverse();
    format!("params={}&encSecKey={}", encode(&params), rsa(&reversed))
}

const MODULUS: &str = "e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7";
fn rsa(secret: &[u8; 16]) -> String {
    use num_bigint::BigUint;
    let modulus = BigUint::parse_bytes(MODULUS.as_bytes(), 16).expect("RSA modulus");
    let value = BigUint::from_bytes_be(secret).modpow(&BigUint::from(65537u32), &modulus);
    format!("{:0>256}", value.to_str_radix(16))
}
