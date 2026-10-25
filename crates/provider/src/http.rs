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
    install_crypto_provider();
    reqwest::Client::new()
}

/// A `reqwest::ClientBuilder` ready to build a TLS-capable client.
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
}
