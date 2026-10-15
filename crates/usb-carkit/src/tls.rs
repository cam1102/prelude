use openssl::{
    memcmp,
    pkey::PKey,
    ssl::{SslConnector, SslMethod, SslOptions, SslStream, SslVerifyMode, SslVersion},
    x509::X509,
};

use crate::{BulkTransport, Error, MuxConnection, PairRecord, Result};

pub(crate) type TlsMuxStream<B> = SslStream<MuxConnection<B>>;
const SSL_OP_LEGACY_SERVER_CONNECT: u64 = 0x4;

pub(crate) fn connect_tls<B: BulkTransport>(
    stream: MuxConnection<B>,
    pairing: &PairRecord,
) -> Result<TlsMuxStream<B>> {
    let host_certificate = parse_certificate(pairing.host_certificate(), "HostCertificate")?;
    let device_certificate = parse_certificate(pairing.device_certificate(), "DeviceCertificate")?;
    let device_certificate_der = device_certificate.to_der().map_err(|error| {
        Error::InvalidPairingRecord(format!("invalid DeviceCertificate: {error}"))
    })?;
    let private_key = PKey::private_key_from_pem(pairing.host_private_key())
        .map_err(|error| Error::InvalidPairingRecord(format!("invalid HostPrivateKey: {error}")))?;

    let mut connector = SslConnector::builder(SslMethod::tls_client())
        .map_err(|error| Error::Tls(error.to_string()))?;
    connector
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(|error| Error::Tls(error.to_string()))?;
    connector
        .set_max_proto_version(Some(SslVersion::TLS1_3))
        .map_err(|error| Error::Tls(error.to_string()))?;
    connector.set_security_level(0);
    connector.set_options(
        SslOptions::from_bits_retain(SSL_OP_LEGACY_SERVER_CONNECT) | SslOptions::NO_COMPRESSION,
    );
    connector
        .set_cipher_list("ALL:!aNULL:!eNULL:@SECLEVEL=0")
        .map_err(|error| Error::Tls(error.to_string()))?;
    connector
        .set_certificate(&host_certificate)
        .map_err(|error| Error::Tls(error.to_string()))?;
    connector
        .set_private_key(&private_key)
        .map_err(|error| Error::Tls(error.to_string()))?;
    connector
        .check_private_key()
        .map_err(|error| Error::Tls(error.to_string()))?;

    let expected_peer = device_certificate_der.clone();
    connector.set_verify_callback(SslVerifyMode::PEER, move |_preverified, context| {
        if context.error_depth() != 0 {
            return true;
        }
        context
            .current_cert()
            .and_then(|certificate| certificate.to_der().ok())
            .is_some_and(|certificate| memcmp::eq(&certificate, &expected_peer))
    });

    let mut configuration = connector
        .build()
        .configure()
        .map_err(|error| Error::Tls(error.to_string()))?;
    configuration.set_verify_hostname(false);
    let ssl = configuration
        .into_ssl("lockdown.local")
        .map_err(|error| Error::Tls(error.to_string()))?;
    let mut tls = SslStream::new(ssl, stream).map_err(|error| Error::Tls(error.to_string()))?;
    tls.connect()
        .map_err(|error| Error::Tls(error.to_string()))?;

    let peer = tls
        .ssl()
        .peer_certificate()
        .ok_or_else(|| Error::Tls("peer omitted its Lockdown certificate".to_owned()))?
        .to_der()
        .map_err(|error| Error::Tls(error.to_string()))?;
    if !memcmp::eq(&peer, &device_certificate_der) {
        return Err(Error::Tls(
            "peer certificate does not match the Lockdown pairing record".to_owned(),
        ));
    }
    Ok(tls)
}

fn parse_certificate(bytes: &[u8], field: &str) -> Result<X509> {
    X509::from_pem(bytes)
        .map_err(|error| Error::InvalidPairingRecord(format!("invalid {field}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_certificate_is_rejected_before_network_use() {
        assert!(matches!(
            parse_certificate(b"not a certificate", "DeviceCertificate"),
            Err(Error::InvalidPairingRecord(_))
        ));
    }
}
