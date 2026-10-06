//! SharePoint credential selection and certificate assertions. Certificate
//! parsing and signing are called from Tokio's blocking pool, never from GPUI.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use opendal::{Error, ErrorKind, Result};
use p12_keystore::{KeyStore, KeyStoreEntry, Pkcs12ImportPolicy};
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs8::{DecodePrivateKey, DecodePublicKey},
    pss::BlindedSigningKey,
    rand_core::OsRng,
    signature::{RandomizedSigner, SignatureEncoding},
    traits::PublicKeyParts,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};
use x509_cert::{
    Certificate,
    der::{Decode, Encode},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharePointAuthMethod {
    ClientSecret,
    Certificate,
    AccessToken,
    RefreshToken,
}

impl SharePointAuthMethod {
    pub const ALL: [Self; 4] = [
        Self::ClientSecret,
        Self::Certificate,
        Self::AccessToken,
        Self::RefreshToken,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientSecret => "client_secret",
            Self::Certificate => "certificate",
            Self::AccessToken => "access_token",
            Self::RefreshToken => "refresh_token",
        }
    }

    pub fn from_profile(profile: &crate::Profile) -> crate::Result<Self> {
        let get = |key: &str| profile.options.get(key).map(|s| s.trim()).unwrap_or("");
        let method = get("auth_method");
        if !method.is_empty() {
            return Self::ALL
                .into_iter()
                .find(|m| m.as_str() == method)
                .ok_or_else(|| crate::Error::Config("SharePoint: 不支持的认证方式".into()));
        }
        // Preserve saved connections created before explicit auth selection.
        Ok(if !get("access_token").is_empty() {
            Self::AccessToken
        } else if !get("refresh_token").is_empty() {
            Self::RefreshToken
        } else if !get("certificate_path").is_empty() {
            Self::Certificate
        } else {
            Self::ClientSecret
        })
    }

    /// Fields relevant to this method, including shared connection fields.
    pub fn includes_field(self, key: &str) -> bool {
        match key {
            "auth_method" => false,
            "tenant_id" | "client_id" => self != Self::AccessToken,
            "client_secret" => matches!(self, Self::ClientSecret | Self::RefreshToken),
            "certificate_path" | "certificate_password" => self == Self::Certificate,
            "access_token" => self == Self::AccessToken,
            "refresh_token" => self == Self::RefreshToken,
            _ => true,
        }
    }

    pub fn field_required(self, key: &str) -> bool {
        match key {
            "tenant_id" => matches!(self, Self::ClientSecret | Self::Certificate),
            "client_id" => self != Self::AccessToken,
            "client_secret" => self == Self::ClientSecret,
            "certificate_path" => self == Self::Certificate,
            "access_token" => self == Self::AccessToken,
            "refresh_token" => self == Self::RefreshToken,
            _ => false,
        }
    }
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::ConfigInvalid, format!("SharePoint: {message}"))
}

pub(crate) struct CertificateCredential {
    key: RsaPrivateKey,
    thumbprint: String,
    not_before: SystemTime,
    not_after: SystemTime,
}

impl CertificateCredential {
    pub(crate) fn load(path: &str, password: &str) -> Result<Self> {
        Self::from_pkcs12(&Self::read_file(std::path::Path::new(path))?, password)
    }

    pub(crate) fn read_file(path: &std::path::Path) -> Result<Vec<u8>> {
        const LIMIT: u64 = 1024 * 1024;
        let file = std::fs::File::open(path).map_err(|_| invalid("无法读取 PFX 证书文件"))?;
        if !file
            .metadata()
            .map_err(|_| invalid("无法读取 PFX 证书文件"))?
            .is_file()
        {
            return Err(invalid("请选择 PFX 证书文件，不能选择目录"));
        }
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid("无法读取 PFX 证书文件"))?;
        if bytes.len() as u64 > LIMIT {
            return Err(invalid("PFX 证书文件不能超过 1 MiB"));
        }
        Ok(bytes)
    }

    pub(crate) fn from_pkcs12(bytes: &[u8], password: &str) -> Result<Self> {
        let store = KeyStore::from_pkcs12(bytes, password, Pkcs12ImportPolicy::Strict)
            .map_err(|_| invalid("无法解密 PFX，请检查证书格式和密码"))?;
        if store
            .entries()
            .filter(|(_, e)| matches!(e, KeyStoreEntry::PrivateKeyChain(_)))
            .count()
            != 1
        {
            return Err(invalid("PFX 必须包含且仅包含一个私钥及其证书"));
        }
        let (_, chain) = store
            .private_key_chain()
            .ok_or_else(|| invalid("PFX 缺少私钥"))?;
        let leaf = chain
            .certs()
            .first()
            .ok_or_else(|| invalid("PFX 缺少证书"))?;
        let cert = Certificate::from_der(leaf.as_der()).map_err(|_| invalid("PFX 证书无效"))?;
        let key = RsaPrivateKey::from_pkcs8_der(chain.key().as_der())
            .map_err(|_| invalid("PFX 认证需要 RSA 私钥"))?;
        if key.n().bits() < 2048 {
            return Err(invalid("RSA 私钥长度不能小于 2048 位"));
        }
        let public_der = cert
            .tbs_certificate()
            .subject_public_key_info()
            .to_der()
            .map_err(|_| invalid("PFX 公钥无效"))?;
        let public = RsaPublicKey::from_public_key_der(&public_der)
            .map_err(|_| invalid("PFX 认证需要 RSA 证书"))?;
        if public != RsaPublicKey::from(&key) {
            return Err(invalid("PFX 私钥和证书不匹配"));
        }
        Ok(Self {
            key,
            thumbprint: URL_SAFE_NO_PAD.encode(Sha256::digest(leaf.as_der())),
            not_before: cert
                .tbs_certificate()
                .validity()
                .not_before
                .to_system_time(),
            not_after: cert.tbs_certificate().validity().not_after.to_system_time(),
        })
    }

    pub(crate) fn assertion(&self, client_id: &str, audience: &str) -> Result<String> {
        self.ensure_valid()?;
        let now = SystemTime::now();
        let seconds = now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("系统时间无效"))?
            .as_secs();
        let encode = |value: serde_json::Value| URL_SAFE_NO_PAD.encode(value.to_string());
        let header = encode(json!({"alg":"PS256", "typ":"JWT", "x5t#S256":self.thumbprint}));
        let claims = encode(json!({
            "aud":audience, "iss":client_id, "sub":client_id,
            "iat":seconds, "nbf":seconds.saturating_sub(60), "exp":seconds + 300,
            "jti":uuid::Uuid::new_v4().to_string(),
        }));
        let message = format!("{header}.{claims}");
        let signature = BlindedSigningKey::<Sha256>::new(self.key.clone())
            .try_sign_with_rng(&mut OsRng, message.as_bytes())
            .map_err(|_| invalid("PFX 签名失败"))?;
        Ok(format!(
            "{message}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    pub(crate) fn ensure_valid(&self) -> Result<()> {
        let now = SystemTime::now();
        if now < self.not_before || now >= self.not_after {
            return Err(invalid("PFX 证书尚未生效或已经过期"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::{
        pss::{Signature, VerifyingKey},
        signature::Verifier,
    };
    const PASSWORD: &str = " pfx+test password ";
    const PFX: &[u8] = include_bytes!("../tests/fixtures/sharepoint-test.pfx");

    fn claims(assertion: &str, index: usize) -> serde_json::Value {
        serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(assertion.split('.').nth(index).unwrap())
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn modern_and_legacy_pfx_assertions_follow_entra_format_and_verify() {
        for data in [
            PFX,
            include_bytes!("../tests/fixtures/sharepoint-test-legacy.pfx").as_slice(),
        ] {
            let credential = CertificateCredential::from_pkcs12(data, PASSWORD).unwrap();
            let audience = "https://login.microsoftonline.com/tenant-id/oauth2/v2.0/token";
            let assertion = credential.assertion("app-id", audience).unwrap();
            let header = claims(&assertion, 0);
            assert_eq!(header["alg"], "PS256");
            assert_eq!(header["typ"], "JWT");
            let store = KeyStore::from_pkcs12(data, PASSWORD, Pkcs12ImportPolicy::Strict).unwrap();
            let cert = &store.private_key_chain().unwrap().1.certs()[0];
            assert_eq!(
                header["x5t#S256"],
                URL_SAFE_NO_PAD.encode(Sha256::digest(cert.as_der()))
            );
            let payload = claims(&assertion, 1);
            assert_eq!(payload["aud"], audience);
            assert_eq!(payload["iss"], "app-id");
            assert_eq!(payload["sub"], "app-id");
            assert_eq!(
                payload["exp"].as_u64().unwrap() - payload["iat"].as_u64().unwrap(),
                300
            );
            assert!(payload["nbf"].as_u64() <= payload["iat"].as_u64());
            assert_ne!(
                payload["jti"],
                claims(&credential.assertion("app-id", audience).unwrap(), 1)["jti"]
            );
            let (message, signature) = assertion.rsplit_once('.').unwrap();
            let signature =
                Signature::try_from(URL_SAFE_NO_PAD.decode(signature).unwrap().as_slice()).unwrap();
            VerifyingKey::<Sha256>::new(RsaPublicKey::from(&credential.key))
                .verify(message.as_bytes(), &signature)
                .unwrap();
        }
    }

    #[test]
    fn incorrect_password_invalid_data_and_certificate_dates_fail_without_secret_echo() {
        for (data, password) in [
            (PFX, "WRONG-PRIVATE-PASSWORD"),
            (b"invalid pfx".as_slice(), PASSWORD),
        ] {
            let error = CertificateCredential::from_pkcs12(data, password)
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains(password));
        }
        let mut credential = CertificateCredential::from_pkcs12(PFX, PASSWORD).unwrap();
        credential.not_after = UNIX_EPOCH;
        assert!(credential.assertion("app", "aud").is_err());
        credential.not_after = SystemTime::now() + std::time::Duration::from_secs(600);
        credential.not_before = credential.not_after;
        assert!(credential.assertion("app", "aud").is_err());
    }

    #[test]
    fn certificate_file_reads_are_bounded_and_empty_password_encrypted_pfx_works() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(1024 * 1024 + 1).unwrap();
        assert!(CertificateCredential::load(file.path().to_str().unwrap(), PASSWORD).is_err());
        let store = KeyStore::from_pkcs12(PFX, PASSWORD, Pkcs12ImportPolicy::Strict).unwrap();
        let empty_password = store.writer("").write().unwrap();
        assert!(CertificateCredential::from_pkcs12(&empty_password, "").is_ok());
        let empty = KeyStore::new().writer(PASSWORD).write().unwrap();
        assert!(CertificateCredential::from_pkcs12(&empty, PASSWORD).is_err());
    }
}
