//! Process-wide HTTP client construction.
//!
//! reqwest is built without a bundled TLS crypto provider, so every client
//! must be created after the `ring` provider has been installed. All client
//! construction goes through these helpers.

use std::sync::Once;

static INSTALL_CRYPTO_PROVIDER: Once = Once::new();

fn install_crypto_provider() {
    INSTALL_CRYPTO_PROVIDER.call_once(|| {
        // Err means another provider is already installed process-wide, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A `reqwest::Client` with default settings.
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("default HTTP client configuration is valid")
}

/// A `reqwest::ClientBuilder` ready to build a TLS-capable client.
#[allow(clippy::disallowed_methods)]
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    with_fallback_roots(reqwest::Client::builder())
}

/// Trusts Mozilla's root set when the system has no CA bundle, as in minimal
/// container images. Systems with a bundle keep their own trust decisions.
#[cfg(all(unix, not(target_os = "android"), not(target_vendor = "apple")))]
fn with_fallback_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    static SYSTEM_HAS_ROOTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let has_roots = *SYSTEM_HAS_ROOTS
        .get_or_init(|| !rustls_native_certs::load_native_certs().certs.is_empty());
    if has_roots {
        return builder;
    }
    builder.tls_certs_merge(bundled_roots())
}

#[cfg(not(all(unix, not(target_os = "android"), not(target_vendor = "apple"))))]
fn with_fallback_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
}

#[cfg(all(unix, not(target_os = "android"), not(target_vendor = "apple")))]
fn bundled_roots() -> impl Iterator<Item = reqwest::Certificate> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der).ok())
}

#[cfg(all(test, unix, not(target_os = "android"), not(target_vendor = "apple")))]
mod tests {
    use super::*;

    #[test]
    fn bundled_roots_parse_and_build_a_client() {
        assert_eq!(
            bundled_roots().count(),
            webpki_root_certs::TLS_SERVER_ROOT_CERTS.len()
        );
        install_crypto_provider();
        #[allow(clippy::disallowed_methods)]
        let builder = reqwest::Client::builder();
        builder
            .tls_certs_merge(bundled_roots())
            .build()
            .expect("client with bundled roots");
    }
}
