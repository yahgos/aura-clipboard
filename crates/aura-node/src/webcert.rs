//! The phone page's HTTPS certificate. One self-signed certificate is shared by the
//! whole group, so a phone that accepted it once is not warned again when another
//! PC takes over serving.

use anyhow::Result;
use aura_net::proto::{KeyVersion, WebCert};
use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair};
use time::{Duration, OffsetDateTime};

/// Host name the phone page is served under (advertised over mDNS by the fofoqueiro).
pub const WEB_HOST: &str = "aura.local";

/// Apple rejects TLS server certificates valid for more than 825 days.
const VALIDITY_DAYS: i64 = 800;

/// Renew the certificate this long before it expires.
pub(crate) const RENEW_BEFORE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

pub(crate) fn generate(version: KeyVersion) -> Result<WebCert> {
    let mut params = CertificateParams::new(vec![WEB_HOST.to_string(), "localhost".to_string()])?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "Aura Clipboard");
    params.distinguished_name = name;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(VALIDITY_DAYS);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(WebCert {
        version,
        expires_ms: (params.not_after.unix_timestamp() as u64) * 1000,
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_short_lived_server_cert() {
        let cert = generate(KeyVersion::default()).unwrap();
        assert!(cert.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(cert.key_pem.contains("PRIVATE KEY"));
        let days = (cert.expires_ms / 1000) as i64 - OffsetDateTime::now_utc().unix_timestamp();
        assert!((799 * 86_400..=800 * 86_400).contains(&days));
    }
}
