//! TLS for the desktop relay and the inference relay, from the pair Set-Desktop-Relay.ps1 writes
//! to `state/certs` (`desktop-relay.crt`, `desktop-relay.key`, PEM), as Node's `https` serves it.
//! The listener is Corten's guide's (the app, Your own listeners): each handshake runs on its
//! own, so a slow client cannot hold up the next connection.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::server::TlsStream;
use tokio_rustls::{TlsAcceptor, rustls};

/// The acceptor for `dir`'s pair, `None` when either file is missing (`loadRelayTls`), or why
/// the pair does not load.
pub fn load(dir: &Path) -> std::io::Result<Option<TlsAcceptor>> {
    let Some(pair) = pools_gateway::backend::load_relay_tls(dir)? else { return Ok(None) };
    acceptor(&pair.cert, &pair.key).map(Some)
}

/// A TLS 1.2 and 1.3 acceptor for HTTP/1.1 with `cert` (the chain) and `key`, both PEM.
pub fn acceptor(cert: &[u8], key: &[u8]) -> std::io::Result<TlsAcceptor> {
    let bad = |what: &str, e: &dyn std::fmt::Display| std::io::Error::other(format!("relay TLS {what}: {e}"));
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert).collect::<Result<_, _>>().map_err(|e| bad("certificate", &e))?;
    if chain.is_empty() {
        return Err(std::io::Error::other("relay TLS certificate: no certificate in the file"));
    }
    let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| bad("key", &e))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("versions", &e))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| bad("pair", &e))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Connections whose handshake is done.
pub struct Tls(mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>, SocketAddr);

impl corten::axum::serve::Listener for Tls {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, SocketAddr) {
        match self.0.recv().await {
            Some(accepted) => accepted,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.1)
    }
}

/// `tcp` behind `acceptor`.
pub fn listener(tcp: TcpListener, acceptor: TlsAcceptor) -> std::io::Result<Tls> {
    let (addr, (tx, rx)) = (tcp.local_addr()?, mpsc::channel(64));
    tokio::spawn(async move {
        // Ends when the server drops its side at shutdown.
        while !tx.is_closed() {
            let Ok((socket, peer)) = tcp.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                let handshake = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket));
                if let Ok(Ok(stream)) = handshake.await {
                    let _ = tx.send((stream, peer)).await;
                }
            });
        }
    });
    Ok(Tls(rx, addr))
}
