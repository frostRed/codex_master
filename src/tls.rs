pub fn install_rustls_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
