// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! TLS to the device, shared by the management and session clients.
//!
//! The device speaks TLS 1.2 only, with no ECDHE. What it offers differs by
//! port (probed on firmware 2.2.1258.1.0):
//!
//! - **HTTPS (443):** DHE with a 1024-bit group (AES-GCM), or static RSA as
//!   `AES256-SHA` only. OpenSSL's default level 2 rejects the group; level 1
//!   keeps forward secrecy and AES-GCM, which falling back to `AES256-SHA`
//!   would lose.
//! - **Session (900):** static RSA only, with AES-GCM. No forward secrecy is
//!   on offer, but it works at the default security level.
//!
//! Both ports serve the same certificate, without any chain.

use std::path::PathBuf;

/// [`connect`]'s cipher list for the HTTPS port. See the module docs.
pub const HTTPS_CIPHERS: &str = "DHE+AESGCM:@SECLEVEL=1";

/// [`connect`]'s cipher list for the session port. See the module docs.
pub const SESSION_CIPHERS: &str = "kRSA+AESGCM";

/// How to check the device's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    /// Check that the certificate chains to a trusted one and names the host.
    /// Off accepts any certificate, such as the factory self-signed one.
    pub verify: bool,
    /// More trusted certificates (PEM), on top of the system's: a private CA,
    /// the intermediate the device doesn't send, or the device's own
    /// certificate. Any of them can anchor the chain.
    pub ca_file: Option<PathBuf>,
}

impl Default for TlsOptions {
    fn default() -> Self {
        TlsOptions {
            verify: true,
            ca_file: None,
        }
    }
}

/// Start TLS 1.2 to `host` over `stream` with `ciphers`.
#[cfg(not(target_arch = "wasm32"))]
pub fn connect<S: std::io::Read + std::io::Write + std::fmt::Debug>(
    opts: &TlsOptions,
    ciphers: &str,
    host: &str,
    stream: S,
) -> Result<openssl::ssl::SslStream<S>, String> {
    use openssl::ssl::{HandshakeError, SslConnector, SslMethod, SslVerifyMode, SslVersion};
    use openssl::x509::verify::X509VerifyFlags;
    use openssl::x509::{X509VerifyResult, X509};

    let setup = |e: openssl::error::ErrorStack| e.to_string();
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(setup)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(setup)?;
    builder.set_cipher_list(ciphers).map_err(setup)?;
    if opts.verify {
        if let Some(path) = &opts.ca_file {
            let pem =
                std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            let certs = X509::stack_from_pem(&pem)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            if certs.is_empty() {
                return Err(format!("{} has no PEM certificates", path.display()));
            }
            for cert in certs {
                builder.cert_store_mut().add_cert(cert).map_err(setup)?;
            }
        }
        // Lets an intermediate or the device's own certificate anchor the
        // chain, since the device sends no chain.
        builder
            .verify_param_mut()
            .set_flags(X509VerifyFlags::PARTIAL_CHAIN)
            .map_err(setup)?;
    } else {
        builder.set_verify(SslVerifyMode::NONE);
    }

    let mut cfg = builder.build().configure().map_err(setup)?;
    cfg.set_verify_hostname(opts.verify);
    // The device has one certificate, so no SNI.
    cfg.set_use_server_name_indication(false);
    cfg.connect(host, stream).map_err(|e| match &e {
        HandshakeError::Failure(mid) if mid.ssl().verify_result() != X509VerifyResult::OK => {
            let ssl = mid.ssl();
            if ssl.verify_result().as_raw() == X509_V_ERR_HOSTNAME_MISMATCH {
                format!(
                    "the device's certificate doesn't name {host}. Connect by a name it \
                     lists, or pass --insecure to skip the check"
                )
            } else {
                format!(
                    "the device's certificate is not trusted: {}. Pass --ca-file with its \
                     issuer, or --insecure to skip the check",
                    ssl.verify_result()
                )
            }
        }
        _ => e.to_string(),
    })
}

/// `X509_V_ERR_HOSTNAME_MISMATCH`, which the `openssl` crate doesn't name.
#[cfg(not(target_arch = "wasm32"))]
const X509_V_ERR_HOSTNAME_MISMATCH: i32 = 62;
