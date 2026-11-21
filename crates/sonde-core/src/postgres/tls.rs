//! TLS configuration for each [`TlsMode`].

use std::sync::Arc;

use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, ring, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};

use crate::{
    driver::TlsMode,
    error::{Error, Result},
};

/// Build the client configuration for `mode`.
///
/// Only [`TlsMode::VerifyFull`] checks the server's certificate, against the
/// operating system's trust store. The other modes encrypt without
/// authenticating, which is what libpq's `prefer` and `require` do.
pub(crate) fn client_config(mode: TlsMode) -> Result<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Config(format!("TLS setup failed: {error}")))?;

    let config = if mode == TlsMode::VerifyFull {
        let loaded = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(loaded.certs);
        if roots.is_empty() {
            return Err(Error::Config(
                "no trusted root certificates found on this system".into(),
            ));
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth()
    };
    Ok(config)
}

/// Accepts whatever certificate the server presents, while still checking
/// that the handshake was signed by its key.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
