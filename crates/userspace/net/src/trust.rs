//! DNS-over-TLS trust for one resolver.
//!
//! A key is that raw key, and the bundle is not consulted. No key means
//! the certificate must chain to the Mozilla roots in this image, be
//! currently valid, and be valid for the domain taken from that
//! certificate. The address you typed is not that name. A lane does
//! not call this.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
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

/// Client for one resolver. `key` set means that pin, and `dns_name` is
/// ignored. `key` absent requires the domain taken from the certificate.
pub fn resolver_client(
    key: Option<[u8; 32]>,
    dns_name: Option<&[u8]>,
    provider: Arc<CryptoProvider>,
) -> Result<(Arc<ClientConfig>, ServerName<'static>), Error> {
    if let Some(key) = key {
        let config = pinned_client(key, provider)?;
        let name = ServerName::try_from("echo".to_string())
            .map_err(|_| Error::General("bad name".into()))?;
        return Ok((config, name));
    }
    let Some(dns_name) = dns_name else {
        return Err(Error::General("no name".into()));
    };
    let dns_name = std::str::from_utf8(dns_name).map_err(|_| Error::General("bad name".into()))?;
    bundle_client(dns_name, bundle_roots(), provider)
}

/// A certificate that chains to `roots`, is currently valid, and names
/// `dns_name`. The address of the resolver is not checked.
pub fn bundle_client(
    dns_name: &str,
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
) -> Result<(Arc<ClientConfig>, ServerName<'static>), Error> {
    if roots.is_empty() {
        return Err(Error::General("no roots".into()));
    }
    let name = ServerName::try_from(dns_name.to_string())
        .map_err(|_| Error::General("bad name".into()))?;
    if !matches!(name, ServerName::DnsName(_)) {
        return Err(Error::General("bad name".into()));
    }
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((Arc::new(config), name))
}

/// Chain and dates only. The caller reads the domain off the certificate
/// and does not send a query on this config.
pub fn learn_client(
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
) -> Result<Arc<ClientConfig>, Error> {
    if roots.is_empty() {
        return Err(Error::General("no roots".into()));
    }
    let verifier = Arc::new(BundleVerifier {
        roots,
        provider: Arc::clone(&provider),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// First concrete DNS name on `der`, skipping a wildcard. `None` when
/// the certificate has no such name, or the name is longer than `max`.
pub fn dns_pin_name(der: &[u8], max: usize) -> Option<Vec<u8>> {
    let body = san_body(der)?;
    let mut at = 0usize;
    while at < body.len() {
        let tag = *body.get(at)?;
        let (value, used) = der_value(&body[at + 1..])?;
        at += 1 + used;
        if tag != 0x82 || value.starts_with(b"*.") || value.len() > max {
            continue;
        }
        if domain_ok(value) {
            return Some(value.to_vec());
        }
    }
    None
}

/// The certificate is valid for `dns_name`. Chain and dates are a
/// separate check.
pub fn name_on_cert(der: &[u8], dns_name: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(dns_name) else {
        return false;
    };
    let Ok(server) = ServerName::try_from(text.to_string()) else {
        return false;
    };
    let owned = CertificateDer::from(der.to_vec());
    let Ok(cert) = ParsedCertificate::try_from(&owned) else {
        return false;
    };
    verify_server_name(&cert, &server).is_ok()
}

fn domain_ok(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    let mut label = 0usize;
    let mut dot = false;
    for b in name {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' => {
                label += 1;
                if label > 63 {
                    return false;
                }
            }
            b'.' => {
                if label == 0 {
                    return false;
                }
                dot = true;
                label = 0;
            }
            _ => return false,
        }
    }
    label != 0 && dot
}

/// SubjectAltName value, the sequence of general names.
fn san_body(der: &[u8]) -> Option<&[u8]> {
    const OID: &[u8] = &[0x06, 0x03, 0x55, 0x1d, 0x11];
    let at = der.windows(OID.len()).position(|w| w == OID)?;
    let mut rest = &der[at + OID.len()..];
    if rest.first() == Some(&0x01) {
        let (_value, used) = der_value(&rest[1..])?;
        rest = &rest[1 + used..];
    }
    if rest.first() != Some(&0x04) {
        return None;
    }
    let (octet, _) = der_value(&rest[1..])?;
    if octet.first() != Some(&0x30) {
        return None;
    }
    let (seq, _) = der_value(&octet[1..])?;
    Some(seq)
}

/// `(value, bytes consumed including the length)`.
fn der_value(input: &[u8]) -> Option<(&[u8], usize)> {
    let lead = *input.first()?;
    if lead < 0x80 {
        let n = lead as usize;
        let value = input.get(1..1 + n)?;
        return Some((value, 1 + n));
    }
    let nbytes = (lead & 0x7f) as usize;
    if nbytes == 0 || nbytes > 2 {
        return None;
    }
    let mut n = 0usize;
    for byte in input.get(1..1 + nbytes)? {
        n = (n << 8) | usize::from(*byte);
    }
    let value = input.get(1 + nbytes..1 + nbytes + n)?;
    Some((value, 1 + nbytes + n))
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

/// Chain and dates against `roots`. The certificate name is read by the
/// caller, not checked here.
#[derive(Debug)]
struct BundleVerifier {
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for BundleVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
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
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
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
