//! TLS as `sslmode` asks for it, the way libpq reads the word: `require`
//! encrypts without checking who answered, and `verify-full` checks.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    /// TLS when the server offers it. Nothing is verified.
    Prefer,
    /// TLS or no connection. Nothing is verified.
    Require,
    /// The certificate must chain to a trusted root.
    VerifyCa,
    /// And name the host connected to.
    VerifyFull,
}

impl SslMode {
    pub fn parse(word: &str) -> Result<Self, String> {
        Ok(match word.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" | "off" | "false" => SslMode::Disable,
            "prefer" | "preferred" | "allow" => SslMode::Prefer,
            "require" | "required" | "on" | "true" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" | "verify-identity" => SslMode::VerifyFull,
            other => return Err(format!("sslmode {other:?} is not one of disable, prefer, require, verify-ca, verify-full")),
        })
    }
}

/// Accepts any certificate, and still checks the handshake's signatures.
#[derive(Debug)]
struct Unverified(Arc<CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The roots to trust: the CA file given, else the public web's.
fn roots(ca_file: Option<&str>) -> Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();
    match ca_file {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path).map_err(|e| format!("{path}: {e}"))?;
            for cert in certs {
                store.add(cert.map_err(|e| format!("{path}: {e}"))?).map_err(|e| format!("{path}: {e}"))?;
            }
            if store.is_empty() {
                return Err(format!("{path}: no certificate in it"));
            }
        }
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(store)
}

pub fn client_config(mode: SslMode, ca_file: Option<&str>) -> Result<ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    Ok(match mode {
        // A CA file is a wish to verify, whatever the mode says.
        SslMode::Prefer | SslMode::Require if ca_file.is_none() => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Unverified(provider)))
            .with_no_client_auth(),
        _ => builder.with_root_certificates(roots(ca_file)?).with_no_client_auth(),
    })
}
