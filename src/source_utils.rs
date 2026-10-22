//! Bounded, process-local implementations of the LX source utility contract.
//!
//! This bridge has no filesystem, process, environment, or network access. Its
//! buffers and expanded zlib output are limited to 2 MiB. AES implements the
//! CBC/ECB algorithms exposed by Node's createCipheriv, with PKCS#7 padding.

use aes::cipher::{Block, BlockEncrypt, BlockSizeUser, KeyInit, consts::U16};
use aes::{Aes128, Aes192, Aes256};
use anyhow::{Context as _, Result, bail, ensure};
use base64::{
    Engine as _, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use boa_engine::object::builtins::JsUint8Array;
use boa_engine::{
    Context, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source,
};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use md5::{Digest, Md5};
use rsa::{
    BigUint, RsaPrivateKey, RsaPublicKey,
    pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey},
    pkcs8::{DecodePrivateKey, DecodePublicKey},
    traits::PublicKeyParts,
};
use serde_json::{Value, json};
use std::io::{Read, Write};

pub const MAX_UTIL_BYTES: usize = 2 * 1024 * 1024;
const MAX_JSON_BYTES: usize = MAX_UTIL_BYTES * 12 + 4096;

/// Installs the native bridge and its frozen JavaScript utility facade. The
/// bootstrap captures `__lxHostUtils` and removes that temporary global before
/// evaluating an approved user script.
pub fn register(context: &mut Context) -> JsResult<()> {
    context.register_global_callable(
        JsString::from("__lx_native_utils"),
        4,
        NativeFunction::from_fn_ptr(native_utils),
    )?;
    context.eval(Source::from_bytes(include_str!("source_utils.js")))?;
    Ok(())
}

fn native_utils(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let argument = args
        .first()
        .and_then(JsValue::as_string)
        .ok_or_else(|| JsNativeError::typ().with_message("LX utility bridge requires JSON text"))?;
    let input = argument.to_std_string_lossy();
    if input.len() > MAX_JSON_BYTES {
        return Err(JsNativeError::range()
            .with_message("LX utility JSON exceeds its bounded size limit")
            .into());
    }
    let mut request: Value = serde_json::from_str(&input).map_err(|error| {
        JsNativeError::typ().with_message(format!("Invalid LX utility JSON: {error}"))
    })?;
    if !request.is_object() {
        return Err(JsNativeError::typ()
            .with_message("LX utility descriptor must be an object")
            .into());
    }
    // Byte payloads cross as native Uint8Array views rather than JSON integer
    // arrays: a 2 MiB response must not allocate millions of Boa JS objects.
    for (index, name) in [(1, "data"), (2, "key"), (3, "iv")] {
        if let Some(object) = args.get(index).and_then(JsValue::as_object) {
            let buffer = JsUint8Array::from_object(object.clone())?;
            if buffer.byte_length(context)? > MAX_UTIL_BYTES {
                return Err(JsNativeError::range()
                    .with_message("LX utility buffer exceeds 2 MiB limit")
                    .into());
            }
            request[name] = json!(buffer.to_vec(context)?);
        }
    }
    let value = match operation(&request) {
        Ok(value) => value,
        Err(error) => {
            if let Some(rsa) = error.downcast_ref::<RsaNodeError>() {
                let object = JsNativeError::error()
                    .with_message(rsa.message)
                    .into_opaque(context);
                for (name, value) in [
                    ("code", rsa.code),
                    ("library", "rsa routines"),
                    ("reason", rsa.reason),
                ] {
                    object.set(JsString::from(name), JsString::from(value), true, context)?;
                }
                return Err(JsError::from_opaque(object.into()));
            }
            return Err(JsNativeError::error()
                .with_message(format!("{error:#}"))
                .into());
        }
    };
    if let Some(array) = value.as_array() {
        let bytes = array
            .iter()
            .map(|value| value.as_u64().unwrap_or_default() as u8);
        return Ok(JsUint8Array::from_iter(bytes, context)?.into());
    }
    match value {
        Value::String(value) => Ok(JsValue::from(JsString::from(value))),
        _ => Err(JsNativeError::error()
            .with_message("Invalid LX utility result")
            .into()),
    }
}

#[derive(Debug)]
struct RsaNodeError {
    code: &'static str,
    message: &'static str,
    reason: &'static str,
}
impl std::fmt::Display for RsaNodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for RsaNodeError {}
fn rsa_node_error(
    code: &'static str,
    message: &'static str,
    reason: &'static str,
) -> anyhow::Error {
    RsaNodeError {
        code,
        message,
        reason,
    }
    .into()
}

fn bytes(request: &Value, name: &str) -> Result<Vec<u8>> {
    let values = request
        .get(name)
        .and_then(Value::as_array)
        .with_context(|| format!("LX utility {name} must contain bytes"))?;
    ensure!(
        values.len() <= MAX_UTIL_BYTES,
        "LX utility buffer exceeds 2 MiB limit"
    );
    values
        .iter()
        .map(|value| {
            let byte = value
                .as_u64()
                .context("LX utility byte must be an integer")?;
            ensure!(byte <= 255, "LX utility byte is outside 0..255");
            Ok(byte as u8)
        })
        .collect()
}

fn text<'a>(request: &'a Value, name: &str) -> Result<&'a str> {
    request
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("LX utility {name} must be a string"))
}

fn operation(request: &Value) -> Result<Value> {
    match text(request, "op")? {
        "from" => Ok(json!(encode(
            text(request, "text")?,
            text(request, "encoding")?
        )?)),
        "toString" => Ok(json!(decode(
            &bytes(request, "data")?,
            text(request, "encoding")?
        )?)),
        "md5" => Ok(json!(format!("{:x}", Md5::digest(bytes(request, "data")?)))),
        "random" => {
            let size = request
                .get("size")
                .and_then(Value::as_u64)
                .context("randomBytes size must be a nonnegative integer")?;
            ensure!(
                size <= MAX_UTIL_BYTES as u64,
                "randomBytes size exceeds 2 MiB limit"
            );
            let mut output = vec![0; size as usize];
            getrandom::fill(&mut output)
                .map_err(|error| anyhow::anyhow!("OS randomness failed: {error}"))?;
            Ok(json!(output))
        }
        "aes" => Ok(json!(aes_encrypt(
            &bytes(request, "data")?,
            text(request, "mode")?,
            &bytes(request, "key")?,
            &bytes(request, "iv")?
        )?)),
        "rsa" => Ok(json!(rsa_encrypt(
            &bytes(request, "data")?,
            &bytes(request, "key")?
        )?)),
        "inflate" => Ok(json!(inflate(&bytes(request, "data")?)?)),
        "deflate" => Ok(json!(deflate(&bytes(request, "data")?)?)),
        name => bail!("Unsupported LX utility operation: {name}"),
    }
}

/// Matches LX's helper: left-pad to 128 bytes, then RSA publicEncrypt with
/// RSA_NO_PADDING. This is a compatibility primitive, never key generation or
/// private-key decryption; all arithmetic and PEM parsing use RustCrypto RSA.
fn rsa_encrypt(input: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        input.len() <= 128,
        "LX RSA input exceeds its 128-byte block"
    );
    ensure!(key.len() <= 16 * 1024, "RSA PEM key exceeds 16 KiB limit");
    let pem = std::str::from_utf8(key).context("RSA key must contain UTF-8 PEM")?;
    let public = if pem.contains("-----BEGIN RSA PUBLIC KEY-----") {
        RsaPublicKey::from_pkcs1_pem(pem).context("Invalid RSA PKCS#1 public key")?
    } else if pem.contains("-----BEGIN PUBLIC KEY-----") {
        RsaPublicKey::from_public_key_pem(pem).context("Invalid RSA SPKI public key")?
    } else if pem.contains("-----BEGIN RSA PRIVATE KEY-----") {
        RsaPublicKey::from(
            &RsaPrivateKey::from_pkcs1_pem(pem).context("Invalid RSA PKCS#1 private PEM")?,
        )
    } else if pem.contains("-----BEGIN PRIVATE KEY-----") {
        RsaPublicKey::from(
            &RsaPrivateKey::from_pkcs8_pem(pem).context("Invalid RSA PKCS#8 private PEM")?,
        )
    } else {
        bail!("RSA key must be an unencrypted RSA PEM public or private key")
    };
    if public.size() < 128 {
        return Err(rsa_node_error(
            "ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE",
            "error:0200006E:rsa routines::data too large for key size",
            "data too large for key size",
        ));
    }
    if public.size() > 128 {
        return Err(rsa_node_error(
            "ERR_OSSL_RSA_DATA_TOO_SMALL_FOR_KEY_SIZE",
            "error:0200007A:rsa routines::data too small for key size",
            "data too small for key size",
        ));
    }
    let message = BigUint::from_bytes_be(input);
    // The raw RustCrypto primitive applies modular exponentiation without
    // rejecting an out-of-domain message; Node's RSA_NO_PADDING rejects it.
    if &message >= public.n() {
        return Err(rsa_node_error(
            "ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS",
            "error:02000084:rsa routines::data too large for modulus",
            "data too large for modulus",
        ));
    }
    let encrypted = rsa::hazmat::rsa_encrypt(&public, &message)
        .context("RSA input is not below the public modulus")?
        .to_bytes_be();
    ensure!(encrypted.len() <= 128, "Invalid RSA public result size");
    let mut output = vec![0; 128];
    output[128 - encrypted.len()..].copy_from_slice(&encrypted);
    Ok(output)
}

fn encoding(name: &str) -> Result<&str> {
    match name {
        "utf8" | "utf-8" => Ok("utf8"),
        "hex" => Ok("hex"),
        "base64" => Ok("base64"),
        "base64url" => Ok("base64url"),
        "latin1" | "binary" => Ok("latin1"),
        "ascii" => Ok("ascii"),
        "utf16le" | "utf-16le" | "ucs2" | "ucs-2" => Ok("utf16le"),
        _ => bail!("Unknown Buffer encoding: {name}"),
    }
}

fn encode(input: &str, name: &str) -> Result<Vec<u8>> {
    ensure!(
        input.len() <= MAX_UTIL_BYTES * 4,
        "Buffer string exceeds size limit"
    );
    let output = match encoding(name)? {
        "utf8" => input.as_bytes().to_vec(),
        "hex" => {
            let mut output = Vec::with_capacity(input.len() / 2);
            // Node stops at the first invalid pair and drops a trailing nibble.
            for pair in input.as_bytes().as_chunks::<2>().0 {
                let digit = |value: u8| (value as char).to_digit(16);
                let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
                    break;
                };
                output.push((high * 16 + low) as u8);
            }
            output
        }
        "base64" | "base64url" => {
            // Buffer.from tolerates whitespace, both alphabets, missing padding,
            // and non-alphabet characters. Padding terminates the useful data.
            let mut cleaned: Vec<u8> = input
                .bytes()
                .take_while(|byte| *byte != b'=')
                .filter_map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => Some(byte),
                    b'-' => Some(b'+'),
                    b'_' => Some(b'/'),
                    _ => None,
                })
                .collect();
            if cleaned.len() % 4 == 1 {
                cleaned.pop();
            }
            let engine = GeneralPurpose::new(
                &alphabet::STANDARD,
                GeneralPurposeConfig::new()
                    .with_decode_padding_mode(DecodePaddingMode::Indifferent)
                    .with_decode_allow_trailing_bits(true),
            );
            engine
                .decode(cleaned)
                .context("Invalid Buffer base64 input")?
        }
        "latin1" | "ascii" => input.encode_utf16().map(|unit| unit as u8).collect(),
        "utf16le" => input.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        _ => unreachable!(),
    };
    ensure!(
        output.len() <= MAX_UTIL_BYTES,
        "LX utility buffer exceeds 2 MiB limit"
    );
    Ok(output)
}

fn decode(input: &[u8], name: &str) -> Result<String> {
    ensure!(
        input.len() <= MAX_UTIL_BYTES,
        "LX utility buffer exceeds 2 MiB limit"
    );
    Ok(match encoding(name)? {
        "utf8" => String::from_utf8_lossy(input).into_owned(),
        "hex" => input.iter().map(|byte| format!("{byte:02x}")).collect(),
        "base64" => base64::engine::general_purpose::STANDARD.encode(input),
        "base64url" => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input),
        "latin1" => input.iter().map(|byte| char::from(*byte)).collect(),
        "ascii" => input.iter().map(|byte| char::from(*byte & 0x7f)).collect(),
        "utf16le" => String::from_utf16_lossy(
            &input
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        ),
        _ => unreachable!(),
    })
}

fn aes_encrypt(input: &[u8], mode: &str, key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        input.len() <= MAX_UTIL_BYTES,
        "AES input exceeds 2 MiB limit"
    );
    let (bits, cbc) = match mode.to_ascii_lowercase().as_str() {
        "aes-128-cbc" => (128, true),
        "aes-192-cbc" => (192, true),
        "aes-256-cbc" => (256, true),
        "aes-128-ecb" => (128, false),
        "aes-192-ecb" => (192, false),
        "aes-256-ecb" => (256, false),
        _ => bail!("Unsupported AES mode: {mode}; supported modes are AES-128/192/256 CBC/ECB"),
    };
    ensure!(
        key.len() == bits / 8,
        "Invalid {mode} key length: expected {} bytes",
        bits / 8
    );
    ensure!(
        iv.len() == if cbc { 16 } else { 0 },
        "Invalid {mode} IV length: expected {} bytes",
        if cbc { 16 } else { 0 }
    );
    let padding = 16 - input.len() % 16;
    ensure!(
        input.len() + padding <= MAX_UTIL_BYTES,
        "AES padded output exceeds 2 MiB limit"
    );
    let mut output = input.to_vec();
    output.resize(output.len() + padding, padding as u8);
    match bits {
        128 => encrypt_blocks::<Aes128>(&mut output, key, iv, cbc)?,
        192 => encrypt_blocks::<Aes192>(&mut output, key, iv, cbc)?,
        256 => encrypt_blocks::<Aes256>(&mut output, key, iv, cbc)?,
        _ => unreachable!(),
    }
    Ok(output)
}

fn encrypt_blocks<C: BlockEncrypt + KeyInit + BlockSizeUser<BlockSize = U16>>(
    output: &mut [u8],
    key: &[u8],
    iv: &[u8],
    cbc: bool,
) -> Result<()> {
    let cipher = C::new_from_slice(key).map_err(|_| anyhow::anyhow!("Invalid AES key"))?;
    let mut previous = [0; 16];
    if cbc {
        previous.copy_from_slice(iv);
    }
    for chunk in output.as_chunks_mut::<16>().0 {
        if cbc {
            for (byte, previous) in chunk.iter_mut().zip(previous) {
                *byte ^= previous;
            }
        }
        let mut block = Block::<C>::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
        if cbc {
            previous.copy_from_slice(chunk);
        }
    }
    Ok(())
}

fn inflate(input: &[u8]) -> Result<Vec<u8>> {
    let decoder = ZlibDecoder::new(input);
    let mut output = Vec::new();
    decoder
        .take(MAX_UTIL_BYTES as u64 + 1)
        .read_to_end(&mut output)
        .context("Invalid zlib stream")?;
    ensure!(
        output.len() <= MAX_UTIL_BYTES,
        "zlib expanded output exceeds 2 MiB limit"
    );
    Ok(output)
}

fn deflate(input: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        input.len() <= MAX_UTIL_BYTES,
        "zlib input exceeds 2 MiB limit"
    );
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(input)
        .context("zlib compression failed")?;
    let output = encoder.finish().context("zlib compression failed")?;
    ensure!(
        output.len() <= MAX_UTIL_BYTES,
        "zlib compressed output exceeds 2 MiB limit"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: &str) -> Vec<u8> {
        encode(value, "hex").unwrap()
    }

    #[test]
    fn aes_nist_first_blocks_and_default_padding() {
        // NIST SP 800-38A F.1 / F.2 vectors, compared independently of the
        // additional PKCS#7 final block required by Node's default padding.
        let input = hex("6bc1bee22e409f96e93d7e117393172a");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        for (bits, key, ecb, cbc) in [
            (
                128,
                "2b7e151628aed2a6abf7158809cf4f3c",
                "3ad77bb40d7a3660a89ecaf32466ef97",
                "7649abac8119b246cee98e9b12e9197d",
            ),
            (
                192,
                "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b",
                "bd334f1d6e45f25ff712a214571fa5cc",
                "4f021db243bc633d7178183a9fa071e8",
            ),
            (
                256,
                "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
                "f3eed1bdb5d2a03c064b5a7e3db181f8",
                "f58c4c04d6e5f1ba779eabfb5f7bfbd6",
            ),
        ] {
            let encrypted =
                aes_encrypt(&input, &format!("aes-{bits}-ecb"), &hex(key), &[]).unwrap();
            assert_eq!(&encrypted[..16], hex(ecb));
            assert_eq!(encrypted.len(), 32);
            let encrypted =
                aes_encrypt(&input, &format!("aes-{bits}-cbc"), &hex(key), &iv).unwrap();
            assert_eq!(&encrypted[..16], hex(cbc));
            assert_eq!(encrypted.len(), 32);
        }
        // Empty input is one entire padding block, not an empty ciphertext.
        assert_eq!(
            decode(
                &aes_encrypt(&[], "aes-128-ecb", &[0; 16], &[]).unwrap(),
                "hex"
            )
            .unwrap(),
            "0143db63ee66b0cdff9f69917680151e"
        );
        assert!(aes_encrypt(b"x", "aes-128-cbc", &[0; 15], &[0; 16]).is_err());
        assert!(aes_encrypt(b"x", "aes-128-cbc", &[0; 16], &[]).is_err());
        assert!(aes_encrypt(b"x", "aes-128-gcm", &[0; 16], &[0; 12]).is_err());
        let input = hex(concat!(
            "6bc1bee22e409f96e93d7e117393172a",
            "ae2d8a571e03ac9c9eb76fac45af8e51",
            "30c81c46a35ce411e5fbc1191a0a52ef",
            "f69f2445df4f9b17ad2b417be66c3710"
        ));
        let expected = hex(concat!(
            "7649abac8119b246cee98e9b12e9197d",
            "5086cb9b507219ee95db113a917678b2",
            "73bed6b8e3c1743b7116e69e22229516",
            "3ff1caa1681fac09120eca307586e1a7"
        ));
        let encrypted = aes_encrypt(
            &input,
            "aes-128-cbc",
            &hex("2b7e151628aed2a6abf7158809cf4f3c"),
            &iv,
        )
        .unwrap();
        assert_eq!(&encrypted[..64], expected);
        assert_eq!(encrypted.len(), 80);
    }

    #[test]
    fn node_encodings_and_md5_vectors() {
        assert_eq!(encode("1ag123", "hex").unwrap(), vec![0x1a]);
        assert_eq!(encode("1a7", "hex").unwrap(), vec![0x1a]);
        assert_eq!(encode(" YW Jj\n!!", "base64").unwrap(), b"abc");
        assert_eq!(encode("-_8", "base64").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(encode("a", "base64").unwrap(), Vec::<u8>::new());
        assert_eq!(encode("é", "utf8").unwrap(), vec![0xc3, 0xa9]);
        assert_eq!(encode("é", "ascii").unwrap(), vec![0xe9]);
        assert_eq!(decode(&[0xe9], "ascii").unwrap(), "i");
        assert_eq!(encode("A水", "utf16le").unwrap(), vec![65, 0, 0x34, 0x6c]);
        assert_eq!(decode(&[65, 0, 99], "utf16le").unwrap(), "A");
        assert_eq!(
            operation(&json!({"op":"md5", "data":[]})).unwrap(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            operation(&json!({"op":"md5", "data": b"abc"})).unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn zlib_roundtrip_errors_and_expansion_budget() {
        let input = b"Original legal fixture: hello, music.\0\xff";
        let compressed = deflate(input).unwrap();
        assert_eq!(inflate(&compressed).unwrap(), input);
        assert!(inflate(&compressed[..compressed.len() - 1]).is_err());
        assert!(inflate(b"not a zlib stream").is_err());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&vec![0; MAX_UTIL_BYTES + 1]).unwrap();
        let bomb = encoder.finish().unwrap();
        assert!(inflate(&bomb).unwrap_err().to_string().contains("2 MiB"));
    }

    #[test]
    fn random_and_byte_argument_limits() {
        assert_eq!(
            operation(&json!({"op":"random", "size":32}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            operation(&json!({"op":"random", "size":0})).unwrap(),
            json!([])
        );
        assert!(operation(&json!({"op":"random", "size":MAX_UTIL_BYTES+1})).is_err());
        assert!(operation(&json!({"op":"random", "size":-1})).is_err());
        assert!(operation(&json!({"op":"md5", "data":[256]})).is_err());
    }
}
