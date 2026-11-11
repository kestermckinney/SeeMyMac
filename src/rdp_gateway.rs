//! RD Gateway for RDP: the connection to the target goes through an HTTPS gateway
//! (MS-TSGU) instead of straight to port 3389.
//!
//! `ironrdp-mstsgu` is async (tokio) while the client in [`crate::rdp`] is blocking, so a
//! small forwarder bridges them: a helper thread signs in to the gateway, listens on a
//! loopback port, and pipes that port into the gateway tunnel. The client then connects
//! to the loopback port as though it were the server, while still using the real host
//! name for TLS and sign-in.

use anyhow::{anyhow, Context, Result};
use ironrdp_mstsgu::{GwClient, GwConnectTarget};
use std::net::SocketAddr;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

/// Sign in to `gateway` (`host` or `host:port`, port 443 by default) for a connection to `server`,
/// and return the loopback address to connect the RDP client to. The forwarder ends with the connection.
pub fn start(gateway: &str, user: &str, pass: &str, server: &str, client_name: &str) -> Result<SocketAddr> {
    let endpoint = if gateway.contains(':') { gateway.to_string() } else { format!("{gateway}:443") };
    let target = GwConnectTarget { gw_endpoint: endpoint, gw_user: user.to_string(), gw_pass: pass.to_string(), server: server.to_string() };
    let client_name = client_name.to_string();
    let (tx, rx) = sync_channel::<Result<SocketAddr, String>>(1);
    std::thread::Builder::new()
        .name("rdp-gateway".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return drop(tx.send(Err(format!("{e}")))),
            };
            rt.block_on(async move {
                let (mut gw, _) = match GwClient::connect(&target, &client_name).await {
                    Ok(g) => g,
                    Err(e) => return drop(tx.send(Err(format!("{e}")))),
                };
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                    Ok(l) => l,
                    Err(e) => return drop(tx.send(Err(format!("{e}")))),
                };
                match listener.local_addr() {
                    Ok(a) => {
                        let _ = tx.send(Ok(a));
                    }
                    Err(e) => return drop(tx.send(Err(format!("{e}")))),
                }
                let Ok(Ok((mut local, _))) = tokio::time::timeout(Duration::from_secs(30), listener.accept()).await else { return };
                drop(listener);
                let _ = tokio::io::copy_bidirectional(&mut local, &mut gw).await;
            });
        })
        .context("cannot start the gateway thread")?;
    match rx.recv_timeout(Duration::from_secs(45)) {
        Ok(Ok(a)) => Ok(a),
        Ok(Err(e)) => Err(anyhow!("RD Gateway: {e}")),
        Err(_) => Err(anyhow!("RD Gateway: no answer")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unreachable_gateway_is_reported_not_hung() {
        // Port 1 on localhost: refused straight away.
        let e = super::start("127.0.0.1:1", "u", "p", "host", "t").unwrap_err().to_string();
        assert!(e.starts_with("RD Gateway:"), "{e}");
    }
}
