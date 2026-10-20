//! Pure-Rust TLS for the server drivers (rustls + ring).
//!
//! Trust anchors = the operating system's certificate store (so corporate /
//! internal CAs installed by IT just work) plus Mozilla's root set compiled
//! into the binary (so it also works on a bare machine). No OpenSSL needed.

use std::sync::{Arc, Once};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

/// Several crates bring their own rustls backend; choose one for the process.
pub fn install_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn roots() -> RootCertStore {
    let mut store = RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = store.add(cert);
    }
    store
}

/// "Trust server certificate": accept any certificate (self-signed dev
/// servers). The connection is still encrypted, just not authenticated.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[allow(dead_code)]
pub fn client_config(trust_any: bool) -> ClientConfig {
    install_provider();
    let builder = ClientConfig::builder();
    if trust_any {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny(provider))).with_no_client_auth()
    } else {
        builder.with_root_certificates(roots()).with_no_client_auth()
    }
}
