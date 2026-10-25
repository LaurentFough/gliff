//! TCP is a development transport without SSH authentication.

use std::io;
use std::net::SocketAddr;
use tokio::net::TcpListener;

/// Resolve once, validate every candidate, then bind those same addresses.
pub async fn bind_development_listener(addr: &str, allow_remote: bool) -> io::Result<TcpListener> {
    let addresses: Vec<_> = tokio::net::lookup_host(addr).await?.collect();
    validate_addresses(&addresses, allow_remote)?;
    TcpListener::bind(addresses.as_slice()).await
}

fn validate_addresses(addresses: &[SocketAddr], allow_remote: bool) -> io::Result<()> {
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "listen address resolved to no addresses",
        ));
    }
    if !allow_remote && addresses.iter().any(|a| !a.ip().is_loopback()) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "unauthenticated --listen requires a loopback address; use --allow-remote to expose the desktop to the network"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resolved_address_requires_loopback_or_explicit_opt_in() {
        let local = ["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()];
        assert!(validate_addresses(&local, false).is_ok());
        for addr in ["0.0.0.0:0", "[::]:0", "192.168.1.2:0"] {
            let address = addr.parse().unwrap();
            assert!(validate_addresses(&[address], false).is_err());
            assert!(validate_addresses(&[local[0], address], false).is_err());
            assert!(validate_addresses(&[address], true).is_ok());
        }
        assert!(validate_addresses(&[], false).is_err());
        assert!(validate_addresses(&[], true).is_err());
    }

    #[tokio::test]
    async fn rejected_addresses_never_open_a_listener() {
        for addr in ["0.0.0.0:0", "[::]:0"] {
            assert_eq!(
                bind_development_listener(addr, false)
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        let listener = bind_development_listener("127.0.0.1:0", false)
            .await
            .unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }
}
