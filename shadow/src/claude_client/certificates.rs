use super::private;
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use std::path::Path;
use time::{Duration, OffsetDateTime};
use tokio_rustls::TlsAcceptor;

fn bad(_: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other("Invalid Claude client certificate/key; trust was not replaced")
}

pub fn expires(pem: &[u8]) -> std::io::Result<i64> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem).map_err(bad)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&p.contents).map_err(bad)?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if cert.validity().not_before.timestamp() > now {
        return Err(bad("not yet valid"));
    }
    Ok(cert.validity().not_after.timestamp())
}

/// A stable, profile-only CA with a renewable 30-day leaf. Existing prototype RSA CAs and
/// proxy credentials are reused during migration; hourly renewal never changes client trust.
pub fn ensure(profile: &Path) -> std::io::Result<(TlsAcceptor, i64)> {
    let dir = profile.join("probe-certs");
    private::directory(&dir)?;
    let ca_path = dir.join("ca.pem");
    let ca_key_path = dir.join("ca-key.pem");
    if !ca_path.exists() {
        if ca_key_path.exists() {
            return Err(bad("orphan CA key"));
        }
        let key = KeyPair::generate().map_err(bad)?;
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(bad)?;
        params.distinguished_name.push(DnType::CommonName, "Agent Pools Claude Code CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = OffsetDateTime::now_utc() - Duration::minutes(5);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(5 * 365);
        let cert = params.self_signed(&key).map_err(bad)?;
        private::write(&ca_key_path, key.serialize_pem().as_bytes())?;
        private::write(&ca_path, cert.pem().as_bytes())?;
    }
    let ca_pem = std::fs::read_to_string(&ca_path)?;
    let now = OffsetDateTime::now_utc();
    if expires(ca_pem.as_bytes())? < (now + Duration::days(31)).unix_timestamp() {
        return Err(std::io::Error::other(
            "Claude profile CA expires soon; explicit trust replacement and client restart required",
        ));
    }
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server-key.pem");
    let renew = !key_path.exists()
        || std::fs::read(&cert_path)
            .ok()
            .and_then(|b| expires(&b).ok())
            .is_none_or(|exp| exp < (now + Duration::days(7)).unix_timestamp());
    if renew {
        let ca_key = KeyPair::from_pem(&std::fs::read_to_string(ca_key_path)?).map_err(bad)?;
        let issuer = CertificateParams::from_ca_cert_pem(&ca_pem).map_err(bad)?.self_signed(&ca_key).map_err(bad)?;
        let key = if key_path.exists() {
            KeyPair::from_pem(&std::fs::read_to_string(&key_path)?).map_err(bad)?
        } else {
            KeyPair::generate().map_err(bad)?
        };
        let mut params = CertificateParams::new(vec!["api.anthropic.com".into()]).map_err(bad)?;
        params.distinguished_name.push(DnType::CommonName, "api.anthropic.com");
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = now - Duration::minutes(5);
        params.not_after = now + Duration::days(30);
        let cert = params.signed_by(&key, &issuer, &ca_key).map_err(bad)?;
        private::write(&key_path, key.serialize_pem().as_bytes())?;
        private::write(&cert_path, cert.pem().as_bytes())?;
    }
    let cert = std::fs::read(cert_path)?;
    let key = std::fs::read(key_path)?;
    let exp = expires(&cert)?;
    Ok((crate::live::tls::acceptor(&cert, &key)?, exp))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renewal_keeps_ca_and_key_and_protects_files() {
        let dir = std::env::temp_dir().join(format!("pools-claude-cert-{}", uuid::Uuid::new_v4()));
        private::directory(&dir).unwrap();
        let (_, exp) = ensure(&dir).unwrap();
        assert!(exp > (OffsetDateTime::now_utc() + Duration::days(29)).unix_timestamp());
        let ca = std::fs::read(dir.join("probe-certs/ca.pem")).unwrap();
        let key = std::fs::read(dir.join("probe-certs/server-key.pem")).unwrap();
        std::fs::remove_file(dir.join("probe-certs/server.pem")).unwrap();
        ensure(&dir).unwrap();
        assert_eq!(ca, std::fs::read(dir.join("probe-certs/ca.pem")).unwrap());
        assert_eq!(key, std::fs::read(dir.join("probe-certs/server-key.pem")).unwrap());
        let mut params = CertificateParams::from_ca_cert_pem(std::str::from_utf8(&ca).unwrap()).unwrap();
        let key = KeyPair::from_pem(&std::fs::read_to_string(dir.join("probe-certs/ca-key.pem")).unwrap()).unwrap();
        params.not_after = OffsetDateTime::now_utc() + Duration::days(10);
        std::fs::write(dir.join("probe-certs/ca.pem"), params.self_signed(&key).unwrap().pem()).unwrap();
        assert!(ensure(&dir).is_err(), "Expiring CA was silently replaced");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
