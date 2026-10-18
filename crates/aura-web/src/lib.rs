//! The phone page. One PC in the group, the fofoqueiro (elected by `aura-node`),
//! serves its clipboard history over HTTPS at `https://aura.local:47821` so a
//! phone can read it and send text back without joining the group.
//!
//! - [`spawn`] watches the node's status and runs the server while this PC is
//!   the fofoqueiro.
//! - [`WebServer`] is one running HTTPS listener (also used directly by tests).
//! - [`qr_login_url`] and [`qr_svg`] make the QR code a PC shows to log a phone in.

mod routes;
pub mod session;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aura_node::{NodeHandle, WebCert};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::task::JoinHandle;

pub use aura_node::webcert::WEB_HOST;

/// A running HTTPS listener for the phone page. Dropping it stops the server.
pub struct WebServer {
    handle: Handle<SocketAddr>,
    addr: SocketAddr,
}

impl WebServer {
    /// Serves the page on `0.0.0.0:port` (0 picks a free port) with the group's shared
    /// certificate.
    pub async fn start(node: NodeHandle, port: u16) -> Result<Self> {
        let cert = node
            .web_cert()
            .await?
            .context("the group has no phone-page certificate yet")?;
        let tls = RustlsConfig::from_config(Arc::new(tls_config(&cert)?));
        let app = routes::router(node).into_make_service_with_connect_info::<SocketAddr>();
        let handle = Handle::new();
        let server = axum_server::bind_rustls(SocketAddr::from(([0, 0, 0, 0], port)), tls)
            .handle(handle.clone());
        tokio::spawn(async move {
            if let Err(e) = server.serve(app).await {
                tracing::warn!("phone page server stopped: {e}");
            }
        });
        let addr = handle
            .listening()
            .await
            .with_context(|| format!("the phone page could not listen on port {port}"))?;
        tracing::info!(
            "serving the phone page on https://{WEB_HOST}:{}",
            addr.port()
        );
        Ok(Self { handle, addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.handle.graceful_shutdown(Some(Duration::from_secs(1)));
    }
}

fn tls_config(cert: &WebCert) -> Result<rustls::ServerConfig> {
    let chain = CertificateDer::pem_slice_iter(cert.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("reading the phone-page certificate")?;
    let key = PrivateKeyDer::from_pem_slice(cert.key_pem.as_bytes())
        .context("reading the phone-page key")?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(chain, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// Runs the phone page whenever this PC is the elected fofoqueiro, and stops it when
/// another PC takes over. Spawn it once after starting the node; see [`spawn`].
pub async fn supervise(node: NodeHandle) {
    let mut status = node.watch_status();
    // (port, certificate expiry) the running server was started with.
    let mut running: Option<(WebServer, (u16, Option<u64>))> = None;
    loop {
        let s = status.borrow_and_update().clone();
        let want = s
            .is_fofoqueiro
            .then_some((s.web_port, s.web_cert_expires_ms));
        if want != running.as_ref().map(|(_, k)| *k) {
            running = None; // stop the old listener first, freeing the port
            if let Some(key) = want {
                match WebServer::start(node.clone(), key.0).await {
                    Ok(server) => running = Some((server, key)),
                    Err(e) => tracing::warn!("phone page: {e:#}"),
                }
            }
        }
        if status.changed().await.is_err() {
            break;
        }
    }
}

/// [`supervise`] on a new tokio task.
pub fn spawn(node: NodeHandle) -> JoinHandle<()> {
    tokio::spawn(supervise(node))
}

/// A link that logs a phone in when opened (valid for 10 minutes), for a QR code.
pub async fn qr_login_url(node: &NodeHandle, host: &str, port: u16) -> Result<String> {
    let token = routes::qr_token(node).await?;
    Ok(format!("https://{host}:{port}/#t={token}"))
}

/// Renders `data` as an SVG QR code.
pub fn qr_svg(data: &str) -> Result<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(data.as_bytes())?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}
