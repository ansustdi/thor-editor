//! SSH local port forwarding: 127.0.0.1:<random> → (ssh host) → target host:port.

use crate::model::SshOptions;
use anyhow::{bail, Context, Result};
use russh::client;
use russh::keys::{self, PrivateKeyWithHashAlg};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

struct Client {
    host: String,
    port: u16,
}

impl client::Handler for Client {
    type Error = anyhow::Error;

    /// Trust-on-first-use against ~/.ssh/known_hosts; a changed key is rejected.
    async fn check_server_key(&mut self, key: &keys::PublicKey) -> Result<bool, Self::Error> {
        match keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                let _ = keys::known_hosts::learn_known_hosts(&self.host, self.port, key);
                Ok(true)
            }
            Err(keys::Error::KeyChanged { line }) => bail!(
                "SSH host key for {}:{} does not match known_hosts line {line}. Possible man-in-the-middle; fix known_hosts to continue.",
                self.host,
                self.port
            ),
            Err(_) => Ok(true),
        }
    }
}

/// Keeps the SSH session and forwarding loop alive; dropping it closes the tunnel.
pub struct Tunnel {
    pub local_port: u16,
    accept: JoinHandle<()>,
    _session: Arc<client::Handle<Client>>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

pub async fn open(opts: &SshOptions, secret: &str, target_host: &str, target_port: u16) -> Result<Tunnel> {
    let port = if opts.port == 0 { 22 } else { opts.port };
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        ..Default::default()
    });
    let handler = Client { host: opts.host.clone(), port };
    let mut session = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client::connect(config, (opts.host.as_str(), port), handler),
    )
    .await
    .context("SSH connect timed out")?
    .with_context(|| format!("SSH connect to {}:{port} failed", opts.host))?;

    let auth = if !opts.key_path.is_empty() {
        let pass = (!secret.is_empty()).then_some(secret);
        let key = keys::load_secret_key(&opts.key_path, pass).context("cannot load SSH private key")?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        session.authenticate_publickey(&opts.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?
    } else {
        session.authenticate_password(&opts.user, secret).await?
    };
    if !auth.success() {
        bail!("SSH authentication failed for {}@{}", opts.user, opts.host);
    }

    let session = Arc::new(session);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    let target_host = target_host.to_string();
    let sess = session.clone();
    let accept = tokio::spawn(async move {
        while let Ok((mut sock, peer)) = listener.accept().await {
            let sess = sess.clone();
            let host = target_host.clone();
            tokio::spawn(async move {
                let Ok(ch) = sess
                    .channel_open_direct_tcpip(host, target_port as u32, peer.ip().to_string(), peer.port() as u32)
                    .await
                else {
                    return;
                };
                let mut stream = ch.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
            });
        }
    });
    Ok(Tunnel { local_port, accept, _session: session })
}
