//! Milestone 125. A resolver certificate that chains to the roots the
//! service checks is accepted with no key when it is valid for the
//! domain taken from that certificate. That domain need not be the
//! pinned address. A certificate that chains to neither those roots
//! nor a pin is refused, and the query bytes are not written. An
//! explicit key accepts only that key, including when the certificate
//! would otherwise chain.

#[path = "../../userspace/net/src/trust.rs"]
mod trust;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, IpAddr, Ipv4Addr as PkiV4, PrivateKeyDer, ServerName};

const ADDR: [u8; 4] = [127, 0, 0, 1];
const QUERY: &[u8] = b"google.com";
const DOMAIN: &str = "cloudflare-dns.com";

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
    let (leaf, leaf_key, ca) = chain_named(&[DOMAIN], false);
    let other = chain_named(&[DOMAIN], false);
    let mut trusted = rustls::RootCertStore::empty();
    trusted
        .add(CertificateDer::from(ca.clone()))
        .expect("test ca");

    let (config, name) =
        trust::bundle_client(DOMAIN, trusted.clone(), provider.clone()).expect("bundle");
    let (ok, got) = exchange(config, name, &leaf, &leaf_key, true);
    assert!(ok, "a chain to the bundle did not finish");
    assert_eq!(
        got, QUERY,
        "a chain whose name is not the pinned address sent no query"
    );

    let (config, name) =
        trust::bundle_client("dns.google", trusted.clone(), provider.clone()).expect("other name");
    let (ok, got) = exchange(config, name, &leaf, &leaf_key, true);
    assert!(!ok, "a different domain was accepted");
    assert!(got.is_empty(), "a different domain sent a query: {got:?}");

    let (config, name) =
        trust::bundle_client(DOMAIN, trusted, provider.clone()).expect("same bundle");
    let (ok, got) = exchange(config, name, &other.0, &other.1, true);
    assert!(!ok, "a certificate that does not chain was accepted");
    assert!(
        got.is_empty(),
        "a certificate that does not chain to the bundle sent a query: {got:?}"
    );

    assert!(
        trust::resolver_client(None, None, provider.clone()).is_err(),
        "a bundle pin with no domain built a client"
    );
    let (config, name) = trust::resolver_client(None, Some(DOMAIN.as_bytes()), provider.clone())
        .expect("mozilla client");
    let (ok, got) = exchange(config, name, &leaf, &leaf_key, true);
    assert!(!ok, "a certificate outside the Mozilla roots was accepted");
    assert!(
        got.is_empty(),
        "a certificate outside the Mozilla roots sent a query: {got:?}"
    );

    let (stale, stale_key, stale_ca) = chain_named(&[DOMAIN], true);
    let mut stale_roots = rustls::RootCertStore::empty();
    stale_roots
        .add(CertificateDer::from(stale_ca))
        .expect("stale ca");
    let (config, name) =
        trust::bundle_client(DOMAIN, stale_roots, provider.clone()).expect("expired bundle");
    let (ok, got) = exchange(config, name, &stale, &stale_key, true);
    assert!(!ok, "an expired chain was accepted");
    assert!(
        got.is_empty(),
        "an expired chain to the bundle sent a query: {got:?}"
    );

    let (ip_leaf, _, _) = chain_named(&["127.0.0.1"], false);
    assert!(
        trust::dns_cert_name(&ip_leaf, 128).is_none(),
        "an address-only certificate produced a domain"
    );
    let (mixed, _, _) = chain_named(&["*.example.com", "one.one.one.one"], false);
    assert_eq!(
        trust::dns_cert_name(&mixed, 128).as_deref(),
        Some(b"one.one.one.one".as_slice())
    );
    assert_eq!(
        trust::dns_cert_name(&leaf, 128).as_deref(),
        Some(DOMAIN.as_bytes())
    );
    assert!(trust::name_on_cert(&leaf, DOMAIN.as_bytes()));

    let mut named_roots = rustls::RootCertStore::empty();
    named_roots.add(CertificateDer::from(ca)).expect("learn ca");
    let config = trust::learn_client(named_roots, provider.clone()).expect("learn");
    let server = ServerName::IpAddress(IpAddr::V4(PkiV4::from(ADDR)));
    let (ok, got) = exchange(config, server, &leaf, &leaf_key, false);
    assert!(ok, "learning the domain did not finish the handshake");
    assert!(got.is_empty(), "learning the domain sent a query: {got:?}");

    let config =
        trust::learn_client(trust::bundle_roots(), provider.clone()).expect("mozilla learn");
    let server = ServerName::IpAddress(IpAddr::V4(PkiV4::from(ADDR)));
    let (ok, got) = exchange(config, server, &leaf, &leaf_key, false);
    assert!(!ok, "a certificate outside the Mozilla roots was learned");
    assert!(
        got.is_empty(),
        "a certificate that chains to neither sent a query: {got:?}"
    );

    let (ed_der, ed_key) = ed25519_cert();
    let pin = trust::ed25519_key(&ed_der).expect("ed25519 pin");
    let (config, name) =
        trust::resolver_client(Some(pin), None, provider.clone()).expect("pinned client");
    let (ok, got) = exchange(config, name, &ed_der, &ed_key, true);
    assert!(ok, "the pinned key did not finish");
    assert_eq!(got, QUERY, "the pinned key sent no query");

    let (config, name) =
        trust::resolver_client(Some([0x22; 32]), None, provider.clone()).expect("wrong pin");
    let (ok, got) = exchange(config, name, &ed_der, &ed_key, true);
    assert!(!ok, "a different key was accepted");
    assert!(got.is_empty(), "a different key sent a query: {got:?}");

    let (config, name) =
        trust::resolver_client(Some(pin), Some(DOMAIN.as_bytes()), provider.clone())
            .expect("pin over bundle");
    let (ok, got) = exchange(config, name, &leaf, &leaf_key, true);
    assert!(!ok, "a key file consulted the bundle");
    assert!(
        got.is_empty(),
        "a key file consulted the bundle and sent a query: {got:?}"
    );
}

fn chain_named(names: &[&str], expired: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
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

    let sans: Vec<String> = names.iter().map(|name| (*name).to_string()).collect();
    let mut leaf_params = rcgen::CertificateParams::new(sans).expect("leaf params");
    if expired {
        leaf_params.not_after = leaf_params.not_before;
    }
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

/// `(handshake finished, application bytes the server read)`.
/// `send` false completes the handshake and writes no query.
fn exchange(
    config: Arc<rustls::ClientConfig>,
    name: ServerName<'static>,
    cert_der: &[u8],
    key_der: &[u8],
    send: bool,
) -> (bool, Vec<u8>) {
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
    let finished = if send {
        let mut tls = rustls::Stream::new(&mut conn, &mut sock);
        tls.write_all(QUERY).is_ok()
    } else {
        drive_handshake(&mut conn, &mut sock)
    };
    let _ = sock.shutdown(std::net::Shutdown::Both);
    (finished, server.join().expect("server thread"))
}

fn drive_handshake(conn: &mut rustls::ClientConnection, sock: &mut TcpStream) -> bool {
    while conn.is_handshaking() {
        if conn.wants_write() && conn.write_tls(sock).is_err() {
            return false;
        }
        if !conn.wants_write() && !conn.wants_read() {
            return false;
        }
        if conn.wants_read() {
            match conn.read_tls(sock) {
                Ok(0) | Err(_) => return false,
                Ok(_) => {}
            }
        }
        if conn.process_new_packets().is_err() {
            return false;
        }
    }
    true
}
