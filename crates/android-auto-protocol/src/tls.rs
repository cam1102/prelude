use std::{
    io::{BufReader, Cursor, Read, Write},
    sync::Arc,
};

use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme,
    client::{
        ResolvesClientCert,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    sign::CertifiedKey,
};

use crate::{Error, Result};

const HU_CERT_PEM: &[u8] = include_bytes!("../certs/hu-cert.pem");
const HU_KEY_PEM: &[u8] = include_bytes!("../certs/hu-key.pem");
const TLS_RECORD_HEADER: usize = 5;

#[derive(Debug)]
struct AcceptPhoneCertificate {
    provider: Arc<CryptoProvider>,
}

#[derive(Debug)]
struct FixedHeadUnitIdentity(Arc<CertifiedKey>);

impl ResolvesClientCert for FixedHeadUnitIdentity {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl ServerCertVerifier for AcceptPhoneCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
pub(crate) struct TlsProgress {
    pub(crate) plaintext: Vec<u8>,
    pub(crate) outbound_tls: Vec<u8>,
    pub(crate) handshake_complete: bool,
    pub(crate) peer_closed: bool,
}

pub(crate) struct AapTls {
    connection: ClientConnection,
    was_handshaking: bool,
}

impl std::fmt::Debug for AapTls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AapTls")
            .field("handshaking", &self.connection.is_handshaking())
            .finish_non_exhaustive()
    }
}

impl AapTls {
    pub(crate) fn new() -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cert_reader = BufReader::new(HU_CERT_PEM);
        let certificate_chain = rustls_pemfile::certs(&mut cert_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::Tls("embedded HU certificate is malformed"))?;
        if certificate_chain.len() != 1 {
            return Err(Error::Tls(
                "embedded HU certificate chain has an unexpected length",
            ));
        }
        let mut key_reader = BufReader::new(HU_KEY_PEM);
        let private_key = rustls_pemfile::private_key(&mut key_reader)
            .map_err(|_| Error::Tls("embedded HU private key is malformed"))?
            .ok_or(Error::Tls("embedded HU private key is missing"))?;

        let signing_key = provider
            .key_provider
            .load_private_key(private_key)
            .map_err(|_| Error::Tls("embedded HU private key is unsupported"))?;
        let identity = Arc::new(CertifiedKey::new(certificate_chain, signing_key));
        let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&[&rustls::version::TLS12])
            .map_err(|_| Error::Tls("TLS 1.2 is unavailable"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptPhoneCertificate { provider }))
            .with_client_cert_resolver(Arc::new(FixedHeadUnitIdentity(identity)));
        let server_name = ServerName::try_from("android-auto.local")
            .map_err(|_| Error::Tls("internal server name is invalid"))?;
        let connection = ClientConnection::new(Arc::new(config), server_name)
            .map_err(|_| Error::Tls("TLS client initialization failed"))?;
        Ok(Self {
            connection,
            was_handshaking: true,
        })
    }

    pub(crate) fn start(&mut self) -> Result<Vec<u8>> {
        self.drain_outbound()
    }

    pub(crate) fn inject(&mut self, bytes: &[u8]) -> Result<TlsProgress> {
        if bytes.is_empty() {
            return Err(Error::InvalidFrame("empty TLS payload"));
        }
        let mut cursor = Cursor::new(bytes);
        while usize::try_from(cursor.position()).unwrap_or(usize::MAX) < bytes.len() {
            let read = self.connection.read_tls(&mut cursor).map_err(|source| {
                Error::Io(std::io::Error::new(source.kind(), "TLS read failed"))
            })?;
            if read == 0 {
                return Err(Error::Tls("TLS record was not fully consumed"));
            }
        }
        let io_state = self
            .connection
            .process_new_packets()
            .map_err(|_| Error::Tls("TLS peer message was rejected"))?;
        let mut plaintext = Vec::new();
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            match self.connection.reader().read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => plaintext.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    return Err(Error::Io(std::io::Error::new(
                        error.kind(),
                        "TLS plaintext read failed",
                    )));
                }
            }
        }
        let outbound_tls = self.drain_outbound()?;
        let handshake_complete = self.was_handshaking && !self.connection.is_handshaking();
        self.was_handshaking = self.connection.is_handshaking();
        Ok(TlsProgress {
            plaintext,
            outbound_tls,
            handshake_complete,
            peer_closed: io_state.peer_has_closed(),
        })
    }

    pub(crate) fn encrypt(&mut self, cleartext: &[u8]) -> Result<Vec<Vec<u8>>> {
        if self.connection.is_handshaking() {
            return Err(Error::InvalidState {
                operation: "encrypted AAP write before TLS completion",
            });
        }
        self.connection
            .writer()
            .write_all(cleartext)
            .map_err(|source| {
                Error::Io(std::io::Error::new(
                    source.kind(),
                    "TLS plaintext write failed",
                ))
            })?;
        let records = self.drain_outbound()?;
        split_tls_records(&records)
    }

    fn drain_outbound(&mut self) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        while self.connection.wants_write() {
            let before = output.len();
            self.connection.write_tls(&mut output).map_err(|source| {
                Error::Io(std::io::Error::new(source.kind(), "TLS write failed"))
            })?;
            if output.len() == before {
                return Err(Error::Tls("TLS writer made no progress"));
            }
        }
        Ok(output)
    }
}

pub(crate) fn split_tls_records(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        if remaining.len() < TLS_RECORD_HEADER {
            return Err(Error::Tls("outbound TLS record header is truncated"));
        }
        let payload_len = usize::from(u16::from_be_bytes([remaining[3], remaining[4]]));
        let record_len = TLS_RECORD_HEADER
            .checked_add(payload_len)
            .ok_or(Error::Tls("outbound TLS record length overflow"))?;
        if remaining.len() < record_len {
            return Err(Error::Tls("outbound TLS record is truncated"));
        }
        records.push(remaining[..record_len].to_vec());
        offset = offset
            .checked_add(record_len)
            .ok_or(Error::Tls("outbound TLS record offset overflow"))?;
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use rustls::{
        ServerConfig, ServerConnection,
        server::{ClientHello, ResolvesServerCert},
    };

    use super::*;

    #[derive(Debug)]
    struct FixedServerIdentity(Arc<CertifiedKey>);

    impl ResolvesServerCert for FixedServerIdentity {
        fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(Arc::clone(&self.0))
        }
    }

    #[test]
    fn embedded_google_automotive_link_identity_loads() {
        let mut tls = AapTls::new().expect("TLS client");
        let client_hello = tls.start().expect("ClientHello");
        assert!(!client_hello.is_empty());
        assert_eq!(client_hello[0], 0x16);
    }

    #[test]
    fn tls_record_splitter_is_strict() {
        let records = split_tls_records(&[
            0x17, 0x03, 0x03, 0x00, 0x02, 1, 2, 0x15, 0x03, 0x03, 0x00, 0x01, 3,
        ])
        .expect("records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].len(), 7);
        assert!(split_tls_records(&[0x17, 0x03, 0x03, 0x00, 0x02, 1]).is_err());
    }

    #[test]
    fn tls12_handshake_and_application_framing_complete_in_memory() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cert_reader = BufReader::new(HU_CERT_PEM);
        let certificates = rustls_pemfile::certs(&mut cert_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("certificates");
        let mut key_reader = BufReader::new(HU_KEY_PEM);
        let key = rustls_pemfile::private_key(&mut key_reader)
            .expect("key parse")
            .expect("key");
        let signing_key = provider
            .key_provider
            .load_private_key(key)
            .expect("signing key");
        let identity = Arc::new(CertifiedKey::new(certificates, signing_key));
        let server_config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12])
            .expect("TLS 1.2")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(FixedServerIdentity(identity)));
        let mut server = ServerConnection::new(Arc::new(server_config)).expect("server");
        let mut client = AapTls::new().expect("client");
        let mut client_to_server = client.start().expect("ClientHello");
        let mut completed = false;

        for _ in 0..8 {
            if !client_to_server.is_empty() {
                server
                    .read_tls(&mut Cursor::new(&client_to_server))
                    .expect("server read");
                server.process_new_packets().expect("server packets");
            }
            let mut server_to_client = Vec::new();
            while server.wants_write() {
                server
                    .write_tls(&mut server_to_client)
                    .expect("server write");
            }
            if server_to_client.is_empty() {
                if !server.is_handshaking() && !client.connection.is_handshaking() {
                    completed = true;
                    break;
                }
                continue;
            }
            let progress = client.inject(&server_to_client).expect("client packets");
            client_to_server = progress.outbound_tls;
            completed |= progress.handshake_complete;
            if completed && !server.is_handshaking() && client_to_server.is_empty() {
                break;
            }
        }
        if !client_to_server.is_empty() {
            server
                .read_tls(&mut Cursor::new(&client_to_server))
                .expect("server final read");
            server.process_new_packets().expect("server final packets");
        }
        assert!(completed);
        assert!(!client.connection.is_handshaking());
        assert_eq!(
            client.connection.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );

        server
            .writer()
            .write_all(&[0, 5, 8, 0])
            .expect("application write");
        let mut encrypted = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut encrypted).expect("TLS write");
        }
        let progress = client.inject(&encrypted).expect("application read");
        assert_eq!(progress.plaintext, [0, 5, 8, 0]);
    }
}
