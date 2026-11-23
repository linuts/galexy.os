//! DNS-over-TLS trust for one resolver.
//!
//! A key is that raw key, and the bundle is not consulted. No key means
//! the certificate must chain to the Mozilla roots in this image and be
//! valid for the resolver address. A lane does not call this.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, IpAddr, Ipv4Addr, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme};

/// The roots linked into this image. The list is the Mozilla set.
pub fn bundle_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// Cipher suites and groups this phase speaks. Signature algorithms stay
/// the ones the pure-Rust provider already verifies.
pub fn provider() -> Arc<CryptoProvider> {
    let mut provider = rustls_pure::provider();
    provider.cipher_suites.retain(|suite| {
        let name = suite.suite();
        name == rustls::CipherSuite::TLS13_AES_128_GCM_SHA256
            || name == rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256
    });
    provider
        .kx_groups
        .retain(|group| group.name() == rustls::NamedGroup::X25519);
    Arc::new(provider)
}

/// Client for one resolver. `key` set means that pin. `key` absent means
/// [`bundle_roots`].
pub fn resolver_client(
    addr: [u8; 4],
    key: Option<[u8; 32]>,
    provider: Arc<CryptoProvider>,
) -> Result<(Arc<ClientConfig>, ServerName<'static>), Error> {
    if let Some(key) = key {
        let config = pinned_client(key, provider)?;
        let name = ServerName::try_from("echo".to_string())
            .map_err(|_| Error::General("bad name".into()))?;
        return Ok((config, name));
    }
    bundle_client(addr, bundle_roots(), provider)
}

/// A certificate that chains to `roots` and is valid for `addr`.
pub fn bundle_client(
    addr: [u8; 4],
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
) -> Result<(Arc<ClientConfig>, ServerName<'static>), Error> {
    if roots.is_empty() {
        return Err(Error::General("no roots".into()));
    }
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::IpAddress(IpAddr::V4(Ipv4Addr::from(addr)));
    Ok((Arc::new(config), name))
}

/// The raw 32-byte key, and nothing else.
pub fn pinned_client(
    key: [u8; 32],
    provider: Arc<CryptoProvider>,
) -> Result<Arc<ClientConfig>, Error> {
    let verifier = Arc::new(PinVerifier {
        key,
        provider: Arc::clone(&provider),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Ed25519 public key from a certificate, when the certificate carries one.
pub fn ed25519_key(der: &[u8]) -> Option<[u8; 32]> {
    const OID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
    let at = der.windows(OID.len()).position(|w| w == OID)?;
    let rest = &der[at + OID.len()..];
    let bit = rest.windows(3).position(|w| w == [0x03, 0x21, 0x00])?;
    if rest.len() < bit + 35 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&rest[bit + 3..bit + 35]);
    Some(key)
}

#[derive(Debug)]
struct PinVerifier {
    key: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let Some(got) = ed25519_key(end_entity.as_ref()) else {
            return Err(Error::General("pin-miss".into()));
        };
        if got != self.key {
            return Err(Error::General("pin-mismatch".into()));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::ServerDoesNotSupportTls12Or13,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )?;
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
