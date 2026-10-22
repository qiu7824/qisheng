use super::model::{invalid, quota};
use super::{SyncError, SyncErrorKind, SyncResult};
use aes::{
    Aes128,
    cipher::{Block, BlockDecrypt, BlockEncrypt, KeyInit},
};
use base64::{
    Engine as _, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig, general_purpose::STANDARD},
};
use md5::{Digest, Md5};
use rsa::{
    Oaep, RsaPrivateKey, RsaPublicKey,
    pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding},
    traits::PublicKeyParts,
};

#[derive(Clone)]
pub struct SharedKey([u8; 16]);
impl std::fmt::Debug for SharedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedKey([redacted])")
    }
}
impl SharedKey {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
    pub fn from_base64(value: &str) -> SyncResult<Self> {
        if value.len() > 64 {
            return Err(quota("AES key encoding exceeds budget"));
        }
        let mut normalized = String::with_capacity(value.len());
        for byte in value.bytes() {
            match byte {
                b'\t' | b'\n' | b'\r' | b' ' | b'\x0b' | b'\x0c' => {}
                b'-' => normalized.push('+'),
                b'_' => normalized.push('/'),
                byte if byte.is_ascii_alphanumeric() || b"+/=".contains(&byte) => {
                    normalized.push(char::from(byte))
                }
                _ => return Err(invalid("Invalid AES key encoding")),
            }
        }
        let decoder = GeneralPurpose::new(
            &alphabet::STANDARD,
            GeneralPurposeConfig::new()
                .with_decode_padding_mode(DecodePaddingMode::Indifferent)
                .with_decode_allow_trailing_bits(true),
        );
        let bytes = decoder
            .decode(normalized)
            .map_err(|_| invalid("Invalid AES key encoding"))?;
        Ok(Self(
            bytes
                .try_into()
                .map_err(|_| invalid("AES key must be 16 bytes"))?,
        ))
    }
    pub fn expose_base64(&self) -> String {
        STANDARD.encode(self.0)
    }
    pub fn generate() -> SyncResult<Self> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| auth("Cannot generate AES key"))?;
        Ok(Self(bytes))
    }
}
pub struct PairingKeyPair {
    pub public_pem: String,
    pub private_pem: String,
}
impl std::fmt::Debug for PairingKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingKeyPair([redacted])")
    }
}
pub fn code_key(code: &str) -> SyncResult<SharedKey> {
    if code.is_empty() || code.len() > 128 {
        return Err(invalid("Invalid pairing code"));
    }
    let hex = format!("{:x}", Md5::digest(code.as_bytes()));
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hex.as_bytes()[..16]);
    Ok(SharedKey(bytes))
}
pub fn aes_encrypt(plaintext: &[u8], key: &SharedKey) -> SyncResult<String> {
    if plaintext.len() > 16384 {
        return Err(quota("Auth plaintext exceeds budget"));
    }
    let pad = 16 - plaintext.len() % 16;
    let mut bytes = plaintext.to_vec();
    bytes.resize(bytes.len() + pad, pad as u8);
    let cipher = Aes128::new_from_slice(&key.0).unwrap();
    for chunk in bytes.as_chunks_mut::<16>().0 {
        let mut block = Block::<Aes128>::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    Ok(STANDARD.encode(bytes))
}
pub fn aes_decrypt(ciphertext: &str, key: &SharedKey) -> SyncResult<Vec<u8>> {
    if ciphertext.len() > 24 * 1024 {
        return Err(quota("Auth ciphertext exceeds budget"));
    }
    let mut bytes = STANDARD
        .decode(ciphertext)
        .map_err(|_| auth("Invalid auth ciphertext"))?;
    if bytes.is_empty() || bytes.len() % 16 != 0 || bytes.len() > 16400 {
        return Err(auth("Invalid auth ciphertext"));
    }
    let cipher = Aes128::new_from_slice(&key.0).unwrap();
    for chunk in bytes.as_chunks_mut::<16>().0 {
        let mut block = Block::<Aes128>::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        chunk.copy_from_slice(&block);
    }
    let pad = usize::from(*bytes.last().unwrap());
    if pad == 0
        || pad > 16
        || bytes[bytes.len() - pad..]
            .iter()
            .any(|value| usize::from(*value) != pad)
    {
        return Err(auth("Invalid auth ciphertext"));
    }
    bytes.truncate(bytes.len() - pad);
    Ok(bytes)
}
pub fn generate_pairing_key_pair() -> SyncResult<PairingKeyPair> {
    let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
        .map_err(|_| auth("Cannot generate RSA pairing key"))?;
    Ok(PairingKeyPair {
        public_pem: RsaPublicKey::from(&private)
            .to_public_key_pem(LineEnding::LF)
            .map_err(|_| auth("Cannot encode RSA public key"))?,
        private_pem: private
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|_| auth("Cannot encode RSA private key"))?
            .to_string(),
    })
}
pub fn rsa_encrypt(plaintext: &[u8], public_pem: &str) -> SyncResult<String> {
    if public_pem.len() > 16384 || plaintext.len() > 214 {
        return Err(quota("RSA auth input exceeds budget"));
    }
    let public = RsaPublicKey::from_public_key_pem(public_pem)
        .map_err(|_| auth("Invalid RSA public key"))?;
    if public.n().bits() != 2048 {
        return Err(auth("Pairing requires RSA 2048"));
    }
    let bytes = public
        .encrypt(
            &mut rsa::rand_core::OsRng,
            Oaep::new::<sha1::Sha1>(),
            plaintext,
        )
        .map_err(|_| auth("RSA auth encryption failed"))?;
    Ok(STANDARD.encode(bytes))
}
pub fn rsa_decrypt(ciphertext: &str, private_pem: &str) -> SyncResult<Vec<u8>> {
    if private_pem.len() > 16384 || ciphertext.len() > 512 {
        return Err(quota("RSA auth input exceeds budget"));
    }
    let private =
        RsaPrivateKey::from_pkcs8_pem(private_pem).map_err(|_| auth("Invalid RSA private key"))?;
    if private.n().bits() != 2048 {
        return Err(auth("Pairing requires RSA 2048"));
    }
    let bytes = STANDARD
        .decode(ciphertext)
        .map_err(|_| auth("Invalid RSA ciphertext"))?;
    if bytes.len() != 256 {
        return Err(auth("Invalid RSA ciphertext"));
    }
    private
        .decrypt_blinded(
            &mut rsa::rand_core::OsRng,
            Oaep::new::<sha1::Sha1>(),
            &bytes,
        )
        .map_err(|_| auth("RSA auth decryption failed"))
}
fn auth(message: &str) -> SyncError {
    SyncError::new(SyncErrorKind::Authentication, message)
}
