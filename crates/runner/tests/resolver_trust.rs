//! Milestone 125. A resolver certificate that chains to the roots the
//! service checks is accepted with no key. A certificate that chains to
//! neither those roots nor a pin is refused, and the query bytes are not
//! written. An explicit key accepts only that key, including when the
//! certificate would otherwise chain.

#[path = "../../userspace/net/src/trust.rs"]
mod trust;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

const ADDR: [u8; 4] = [127, 0, 0, 1];
const QUERY: &[u8] = b"google.com";

#[test]
fn resolver_trusts_the_bundle() {
    let roots = trust::bundle_roots();
    assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    for (got, expect) in roots
        .roots
        .iter()
        .zip(webpki_roots::TLS_SERVER_ROOTS.iter())
    {
        assert_eq!(got, expect);
    }

    let provider = trust::provider();
    let (leaf, leaf_key, ca) = chain_for(ADDR);
    let other = chain_for(ADDR);
    let mut trusted = rustls::RootCertStore::empty();
    trusted
        .add(CertificateDer::from(ca.clone()))
        .expect("test ca");

    let (config, name) =
        trust::bundle_client(ADDR, trusted.clone(), provider.clone()).expect("bundle");
    let got = exchange(config, name, &leaf, &leaf_key);
    assert_eq!(got, QUERY, "a chain to the bundle sent no query");

    let (config, name) =
        trust::bundle_client(ADDR, trusted, provider.clone()).expect("same bundle");
    let got = exchange(config, name, &other.0, &other.1);
    assert!(
        got.is_empty(),
        "a certificate that does not chain to the bundle was accepted: {got:?}"
    );

    let (config, name) =
        trust::resolver_client(ADDR, None, provider.clone()).expect("mozilla client");
    let got = exchange(config, name, &leaf, &leaf_key);
    assert!(
        got.is_empty(),
        "a certificate outside the Mozilla roots was accepted: {got:?}"
    );

    let mut only_ca = rustls::RootCertStore::empty();
    only_ca
        .add(CertificateDer::from(ca))
        .expect("test ca again");
    let (config, name) =
        trust::bundle_client([127, 0, 0, 2], only_ca, provider.clone()).expect("other address");
    let got = exchange(config, name, &leaf, &leaf_key);
    assert!(
        got.is_empty(),
        "a chain for a different address was accepted: {got:?}"
    );

    let (ed_der, ed_key) = ed25519_cert();
    let pin = trust::ed25519_key(&ed_der).expect("ed25519 pin");
    let (config, name) =
        trust::resolver_client(ADDR, Some(pin), provider.clone()).expect("pinned client");
    let got = exchange(config, name, &ed_der, &ed_key);
    assert_eq!(got, QUERY, "the pinned key sent no query");

    let (config, name) =
        trust::resolver_client(ADDR, Some([0x22; 32]), provider.clone()).expect("wrong pin");
    let got = exchange(config, name, &ed_der, &ed_key);
    assert!(got.is_empty(), "a different key was accepted: {got:?}");

    let (config, name) =
        trust::resolver_client(ADDR, Some(pin), provider.clone()).expect("pin over bundle");
    let got = exchange(config, name, &leaf, &leaf_key);
    assert!(
        got.is_empty(),
        "a key file consulted the bundle and accepted another certificate: {got:?}"
    );
}

fn chain_for(addr: [u8; 4]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let alg = &rcgen::PKCS_ECDSA_P256_SHA256;
    let ca_key = rcgen::KeyPair::generate_for(alg).expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "resolver-test-ca");
    ca_params
        .key_usages
        .push(rcgen::KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(rcgen::KeyUsagePurpose::CrlSign);
    let ca = ca_params.self_signed(&ca_key).expect("ca cert");

    let text = format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3]);
    let mut leaf_params = rcgen::CertificateParams::new(vec![text]).expect("leaf params");
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "resolver");
    leaf_params.use_authority_key_identifier_extension = true;
    leaf_params
        .key_usages
        .push(rcgen::KeyUsagePurpose::DigitalSignature);
    leaf_params
        .extended_key_usages
        .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
    let leaf_key = rcgen::KeyPair::generate_for(alg).expect("leaf key");
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("leaf cert");
    (
        leaf.der().as_ref().to_vec(),
        leaf_key.serialize_der(),
        ca.der().as_ref().to_vec(),
    )
}

fn ed25519_cert() -> (Vec<u8>, Vec<u8>) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("ed25519 key");
    let params = rcgen::CertificateParams::new(vec!["echo".into()]).expect("echo params");
    let cert = params.self_signed(&key).expect("echo cert");
    (cert.der().as_ref().to_vec(), key.serialize_der())
}

/// Returns the application bytes the server read. Empty means the
/// handshake was refused before a query.
fn exchange(
    config: Arc<rustls::ClientConfig>,
    name: ServerName<'static>,
    cert_der: &[u8],
    key_der: &[u8],
) -> Vec<u8> {
    let cert = CertificateDer::from(cert_der.to_vec());
    let key = PrivateKeyDer::try_from(key_der.to_vec()).expect("key der");
    let server_provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = Arc::new(
        rustls::ServerConfig::builder_with_provider(server_provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("tls 1.3")
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("server config"),
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        sock.set_write_timeout(Some(Duration::from_secs(5)))
            .expect("write timeout");
        let mut conn = rustls::ServerConnection::new(server_config).expect("server conn");
        let mut tls = rustls::Stream::new(&mut conn, &mut sock);
        let mut buf = [0u8; 64];
        match tls.read(&mut buf) {
            Ok(n) => buf[..n].to_vec(),
            Err(_) => Vec::new(),
        }
    });
    let mut sock = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("client read timeout");
    sock.set_write_timeout(Some(Duration::from_secs(5)))
        .expect("client write timeout");
    let mut conn = rustls::ClientConnection::new(config, name).expect("client conn");
    {
        let mut tls = rustls::Stream::new(&mut conn, &mut sock);
        let _ = tls.write_all(QUERY);
    }
    let _ = sock.shutdown(std::net::Shutdown::Both);
    server.join().expect("server thread")
}
