//! TLS between a primary and its replicas, each proving who it is.
//!
//! Both ends hold a certificate, its private key, and the certificate of
//! the authority that signed the other end's. A primary answers only a
//! replica whose certificate that authority signed; a replica sends its
//! ask only to a primary whose certificate it signed and that names the
//! host the replica was told to follow. There is no way to switch either
//! check off, and no older protocol to fall back to: TLS 1.3 only.
//!
//! The three are read from PEM files that whoever runs the server made
//! and placed. Nothing here makes a key or a certificate.

// Until the replicas' port is given these, only the tests below use them.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the replicas' port takes these next")
)]

use std::fmt;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::version::TLS13;
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// What one end holds, as the bytes of its PEM files.
#[derive(Clone, Copy)]
pub(super) struct Pems<'a> {
    /// This end's certificate, and any between it and the authority.
    pub(super) certificate: &'a [u8],
    /// The private key of that certificate.
    pub(super) key: &'a [u8],
    /// The authority whose signature the other end's certificate needs.
    pub(super) authority: &'a [u8],
}

/// Why what was given cannot be used. Says which of the three, and never
/// any of a key's bytes.
#[derive(Debug)]
pub(super) struct TlsError(String);

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TlsError {}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn certificates(pem: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError(format!("{what} is not PEM: {error}")))?;
    if certificates.is_empty() {
        return Err(TlsError(format!("{what} holds no certificate")));
    }
    Ok(certificates)
}

fn key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, TlsError> {
    // The error names what kind of thing was missing, not what was read.
    PrivateKeyDer::from_pem_slice(pem)
        .map_err(|error| TlsError(format!("the private key cannot be read: {error}")))
}

fn authority(pem: &[u8]) -> Result<Arc<RootCertStore>, TlsError> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates(pem, "the authority's certificate")? {
        roots.add(certificate).map_err(|error| {
            TlsError(format!(
                "the authority's certificate cannot be used: {error}"
            ))
        })?;
    }
    Ok(Arc::new(roots))
}

fn unusable(error: impl fmt::Display) -> TlsError {
    TlsError(format!(
        "the certificate and its key cannot be used: {error}"
    ))
}

/// How a primary answers replicas: with its certificate, and only one
/// that shows a certificate the authority signed.
pub(super) fn server(pems: Pems<'_>) -> Result<Arc<ServerConfig>, TlsError> {
    let roots = authority(pems.authority)?;
    let verifier = WebPkiClientVerifier::builder_with_provider(roots, provider())
        .build()
        .map_err(|error| {
            TlsError(format!(
                "the authority's certificate cannot be used: {error}"
            ))
        })?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&TLS13])
        .map_err(|error| TlsError(error.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            certificates(pems.certificate, "the certificate")?,
            key(pems.key)?,
        )
        .map_err(unusable)?;
    Ok(Arc::new(config))
}

/// How a replica asks its primary: showing its certificate, and only a
/// primary whose certificate the authority signed for the name asked.
pub(super) fn client(pems: Pems<'_>) -> Result<Arc<ClientConfig>, TlsError> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&TLS13])
        .map_err(|error| TlsError(error.to_string()))?
        .with_root_certificates(authority(pems.authority)?)
        .with_client_auth_cert(
            certificates(pems.certificate, "the certificate")?,
            key(pems.key)?,
        )
        .map_err(unusable)?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use rustls::pki_types::ServerName;
    use rustls::{ClientConnection, ServerConnection, StreamOwned};

    use super::*;

    /// An authority made for one test, and gone with it.
    struct Authority {
        certificate: String,
        issuer: Issuer<'static, KeyPair>,
    }

    /// A certificate and its key, as PEM.
    struct Held {
        certificate: String,
        key: String,
    }

    impl Authority {
        fn new() -> Self {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(Vec::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let certificate = params.self_signed(&key).unwrap().pem();
            Self {
                certificate,
                issuer: Issuer::new(params, key),
            }
        }

        fn sign(&self, name: &str) -> Held {
            let key = KeyPair::generate().unwrap();
            let params = CertificateParams::new(vec![name.to_owned()]).unwrap();
            let certificate = params.signed_by(&key, &self.issuer).unwrap().pem();
            Held {
                certificate,
                key: key.serialize_pem(),
            }
        }
    }

    fn pems<'a>(held: &'a Held, trusted: &'a Authority) -> Pems<'a> {
        Pems {
            certificate: held.certificate.as_bytes(),
            key: held.key.as_bytes(),
            authority: trusted.certificate.as_bytes(),
        }
    }

    /// One connection: the client sends `ping` and reads four bytes, the
    /// server reads four and sends `pong`. What each end got.
    fn exchange(
        server: Arc<ServerConfig>,
        client: Arc<ClientConfig>,
        name: &str,
    ) -> (io::Result<[u8; 4]>, io::Result<[u8; 4]>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let served = thread::spawn(move || {
            let (stream, _) = listener.accept()?;
            let connection = ServerConnection::new(server).map_err(io::Error::other)?;
            let mut stream = StreamOwned::new(connection, stream);
            let mut got = [0; 4];
            stream.read_exact(&mut got)?;
            stream.write_all(b"pong")?;
            Ok(got)
        });
        let asked = (|| {
            let stream = TcpStream::connect(address)?;
            let name = ServerName::try_from(name.to_owned()).map_err(io::Error::other)?;
            let connection = ClientConnection::new(client, name).map_err(io::Error::other)?;
            let mut stream = StreamOwned::new(connection, stream);
            stream.write_all(b"ping")?;
            let mut got = [0; 4];
            stream.read_exact(&mut got)?;
            Ok(got)
        })();
        (served.join().unwrap(), asked)
    }

    #[test]
    fn two_ends_signed_by_the_authority_talk() {
        let authority = Authority::new();
        let primary = authority.sign("localhost");
        let replica = authority.sign("replica");

        let (served, asked) = exchange(
            server(pems(&primary, &authority)).unwrap(),
            client(pems(&replica, &authority)).unwrap(),
            "localhost",
        );

        assert_eq!(served.unwrap(), *b"ping");
        assert_eq!(asked.unwrap(), *b"pong");
    }

    #[test]
    fn the_key_exchange_is_the_post_quantum_one() {
        let authority = Authority::new();
        let primary = authority.sign("localhost");
        let replica = authority.sign("replica");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = server(pems(&primary, &authority)).unwrap();
        let served = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), stream);
            stream.write_all(b"pong").unwrap();
        });

        let name = ServerName::try_from("localhost").unwrap();
        let connection =
            ClientConnection::new(client(pems(&replica, &authority)).unwrap(), name).unwrap();
        let mut stream = StreamOwned::new(connection, TcpStream::connect(address).unwrap());
        let mut got = [0; 4];
        stream.read_exact(&mut got).unwrap();
        served.join().unwrap();

        let group = stream.conn.negotiated_key_exchange_group().unwrap();
        assert_eq!(group.name(), rustls::NamedGroup::X25519MLKEM768);
        assert_eq!(
            stream.conn.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
    }

    #[test]
    fn a_replica_signed_by_another_authority_is_not_answered() {
        let authority = Authority::new();
        let other = Authority::new();
        let primary = authority.sign("localhost");
        let stranger = other.sign("replica");
        // It trusts the primary's authority, so only its own certificate
        // is what is wrong.
        let stranger = Pems {
            authority: authority.certificate.as_bytes(),
            ..pems(&stranger, &other)
        };

        let (served, asked) = exchange(
            server(pems(&primary, &authority)).unwrap(),
            client(stranger).unwrap(),
            "localhost",
        );

        assert!(served.is_err());
        assert!(asked.is_err());
    }

    #[test]
    fn a_replica_with_no_certificate_is_not_answered() {
        let authority = Authority::new();
        let primary = authority.sign("localhost");
        let bare = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&TLS13])
            .unwrap()
            .with_root_certificates(super::authority(authority.certificate.as_bytes()).unwrap())
            .with_no_client_auth();

        let (served, asked) = exchange(
            server(pems(&primary, &authority)).unwrap(),
            Arc::new(bare),
            "localhost",
        );

        assert!(served.is_err());
        assert!(asked.is_err());
    }

    #[test]
    fn a_primary_signed_by_another_authority_is_not_asked() {
        let authority = Authority::new();
        let other = Authority::new();
        let impostor = other.sign("localhost");
        let replica = authority.sign("replica");

        let (served, asked) = exchange(
            server(pems(&impostor, &authority)).unwrap(),
            client(pems(&replica, &authority)).unwrap(),
            "localhost",
        );

        assert!(served.is_err());
        assert!(asked.is_err());
    }

    #[test]
    fn a_primary_whose_certificate_names_another_host_is_not_asked() {
        let authority = Authority::new();
        let primary = authority.sign("elsewhere");
        let replica = authority.sign("replica");

        let (served, asked) = exchange(
            server(pems(&primary, &authority)).unwrap(),
            client(pems(&replica, &authority)).unwrap(),
            "localhost",
        );

        assert!(served.is_err());
        assert!(asked.is_err());
    }

    #[test]
    fn a_primary_followed_by_address_needs_the_address_in_its_certificate() {
        let authority = Authority::new();
        let named = authority.sign("localhost");
        let addressed = authority.sign("127.0.0.1");
        let replica = authority.sign("replica");
        let ask = |primary: &Held| {
            exchange(
                server(pems(primary, &authority)).unwrap(),
                client(pems(&replica, &authority)).unwrap(),
                "127.0.0.1",
            )
            .1
        };

        assert!(ask(&named).is_err());
        assert_eq!(ask(&addressed).unwrap(), *b"pong");
    }

    #[test]
    fn a_plain_client_gets_nothing_from_a_tls_port() {
        let authority = Authority::new();
        let primary = authority.sign("localhost");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = server(pems(&primary, &authority)).unwrap();
        let served = thread::spawn(move || {
            let (stream, _) = listener.accept()?;
            let connection = ServerConnection::new(config).map_err(io::Error::other)?;
            let mut stream = StreamOwned::new(connection, stream);
            let mut got = [0; 4];
            stream.read_exact(&mut got).map(|()| got)
        });

        let mut plain = TcpStream::connect(address).unwrap();
        plain.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();

        assert!(served.join().unwrap().is_err());
    }

    #[test]
    fn what_is_not_a_certificate_or_a_key_is_refused_with_which_it_was() {
        let authority = Authority::new();
        let held = authority.sign("localhost");
        let other = authority.sign("localhost");
        let good = pems(&held, &authority);

        let no_certificate = server(Pems {
            certificate: b"",
            ..good
        })
        .unwrap_err();
        let no_key = server(Pems { key: b"", ..good }).unwrap_err();
        let no_authority = client(Pems {
            authority: b"",
            ..good
        })
        .unwrap_err();
        let mismatch = server(Pems {
            key: other.key.as_bytes(),
            ..good
        })
        .unwrap_err();

        assert_eq!(
            no_certificate.to_string(),
            "the certificate holds no certificate"
        );
        assert!(
            no_key
                .to_string()
                .starts_with("the private key cannot be read")
        );
        assert_eq!(
            no_authority.to_string(),
            "the authority's certificate holds no certificate"
        );
        assert!(
            mismatch
                .to_string()
                .starts_with("the certificate and its key cannot be used")
        );
    }
}
