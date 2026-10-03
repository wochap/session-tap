//! Hub TLS identity and the TLS 1.3 configurations for the remote listener.
//! Trust is pinned to SubjectPublicKeyInfo hashes, never to a CA or names.

use anyhow::{Context, Result, anyhow};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, WebPkiSupportedAlgorithms},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

/// File name of the hub identity inside the hub state directory.
pub const IDENTITY_FILE: &str = "remote-identity.pem";

/// A private key and its self-signed certificate.
#[derive(Debug)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    /// DER SubjectPublicKeyInfo of `cert`.
    pub spki: Vec<u8>,
}

impl Identity {
    /// Generates an ECDSA P-256 key and a long-lived self-signed certificate.
    pub fn generate(common_name: &str) -> Result<Self> {
        Self::from_pem(&Self::generate_pem(common_name)?)
    }

    fn generate_pem(common_name: &str) -> Result<String> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(4000, 1, 1);
        let cert = params.self_signed(&key)?;
        Ok(format!("{}{}", key.serialize_pem(), cert.pem()))
    }

    /// Parses one private key and one certificate from PEM text.
    pub fn from_pem(pem: &str) -> Result<Self> {
        let key = PrivateKeyDer::from_pem_slice(pem.as_bytes())
            .map_err(|error| anyhow!("identity private key: {error}"))?;
        let cert = CertificateDer::pem_slice_iter(pem.as_bytes())
            .next()
            .ok_or_else(|| anyhow!("identity has no certificate"))?
            .map_err(|error| anyhow!("identity certificate: {error}"))?;
        let spki = spki_of(&cert)?;
        Ok(Self { cert, key, spki })
    }

    /// Lowercase hex SHA-256 of the SPKI: the hub ID or device key hash.
    #[must_use]
    pub fn spki_sha256(&self) -> String {
        sha256_hex(&self.spki)
    }

    #[must_use]
    pub fn clone_key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

/// Loads the hub identity from `path`, generating and storing a new one
/// (mode 0600) when the file is missing.
pub fn load_or_create_identity(path: &Path, common_name: &str) -> Result<Identity> {
    match std::fs::read_to_string(path) {
        Ok(pem) => Identity::from_pem(&pem).with_context(|| format!("load {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let pem = Identity::generate_pem(common_name)?;
            sessiontap_infra::fs::atomic_write(path, pem.as_bytes(), 0o600)
                .with_context(|| format!("write {}", path.display()))?;
            Identity::from_pem(&pem)
        }
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// DER SubjectPublicKeyInfo of a certificate.
pub fn spki_of(cert: &CertificateDer<'_>) -> Result<Vec<u8>> {
    let parsed = webpki::EndEntityCert::try_from(cert)
        .map_err(|error| anyhow!("malformed certificate: {error}"))?;
    Ok(parsed.subject_public_key_info().as_ref().to_vec())
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[must_use]
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// TLS 1.3 server config presenting the hub identity. Client certificates
/// are optional and any well-formed one is accepted; authorization happens
/// after the handshake by SPKI lookup.
pub fn server_config(identity: &Identity) -> Result<Arc<rustls::ServerConfig>> {
    let provider = provider();
    let verifier = Arc::new(AnyClientCert {
        algorithms: provider.signature_verification_algorithms,
    });
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.clone_key())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Client config that pins the server SPKI hash and optionally presents a
/// client certificate. This is the trust model a paired device uses.
pub fn client_config(hub_id: &str, client: Option<&Identity>) -> Result<Arc<rustls::ClientConfig>> {
    client_config_with_versions(hub_id, client, &[&rustls::version::TLS13])
}

/// `client_config` restricted to the given protocol versions.
pub fn client_config_with_versions(
    hub_id: &str,
    client: Option<&Identity>,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<Arc<rustls::ClientConfig>> {
    let provider = provider();
    let verifier = Arc::new(PinnedServer {
        hub_id: hub_id.to_owned(),
        algorithms: provider.signature_verification_algorithms,
    });
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match client {
        Some(identity) => {
            builder.with_client_auth_cert(vec![identity.cert.clone()], identity.clone_key())?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct AnyClientCert {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        spki_of(end_entity)
            .map(|_| ClientCertVerified::assertion())
            .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedServer {
    hub_id: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match spki_of(end_entity) {
            Ok(spki) if sha256_hex(&spki) == self.hub_id => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn identity_is_reused_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(IDENTITY_FILE);
        let first = load_or_create_identity(&path, "hub").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let second = load_or_create_identity(&path, "hub").unwrap();
        assert_eq!(first.spki_sha256(), second.spki_sha256());
        assert_eq!(first.spki_sha256().len(), 64);
        std::fs::remove_file(&path).unwrap();
        let third = load_or_create_identity(&path, "hub").unwrap();
        assert_ne!(first.spki_sha256(), third.spki_sha256());
    }
}
