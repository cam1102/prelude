use rand_core::{CryptoRng, OsRng, RngCore};

use crate::{
    Error, MfiSapRequest, MfiSapResponder, MfiSapState, MfiSigner, PairVerification, Result,
    RtspRequest, RtspResponse,
};

const AUTH_SETUP_SUFFIX: &str = "/auth-setup";
const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Debug)]
pub struct AuthSetupHandler<S, R = OsRng> {
    responder: MfiSapResponder<S, R>,
}

impl<S: MfiSigner> AuthSetupHandler<S, OsRng> {
    pub fn new(signer: S) -> Self {
        Self {
            responder: MfiSapResponder::new(signer),
        }
    }
}

impl<S, R> AuthSetupHandler<S, R>
where
    S: MfiSigner,
    R: RngCore + CryptoRng,
{
    pub fn with_rng(signer: S, random: R) -> Self {
        Self {
            responder: MfiSapResponder::with_rng(signer, random),
        }
    }

    pub fn state(&self) -> MfiSapState {
        self.responder.state()
    }

    pub fn handle(
        &mut self,
        request: &RtspRequest,
        verification: &PairVerification,
    ) -> Result<RtspResponse> {
        if request.method != "POST"
            || !request
                .target
                .to_ascii_lowercase()
                .ends_with(AUTH_SETUP_SUFFIX)
        {
            return Err(Error::InvalidRtsp(
                "MFiSAP handler requires POST */auth-setup".to_owned(),
            ));
        }
        let mfi_request = match MfiSapRequest::parse(&request.body) {
            Ok(request) => request,
            Err(Error::InvalidMfiSap(_)) => {
                return Ok(RtspResponse::new(400, "Bad Request"));
            }
            Err(error) => return Err(error),
        };
        let proof = self.responder.respond(mfi_request, verification)?;
        let mut response = RtspResponse::new(200, "OK");
        response
            .headers
            .insert("content-type".to_owned(), OCTET_STREAM.to_owned());
        response.body = proof.response().to_vec();
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use rand_core::{CryptoRng, Error as RngError, RngCore};
    use x25519_dalek::{PublicKey, StaticSecret};

    use super::*;
    use crate::{HeaderMap, SignerError};

    #[derive(Debug)]
    struct FixedSigner;

    impl MfiSigner for FixedSigner {
        fn protocol_major(&self) -> u8 {
            3
        }

        fn certificate(&mut self) -> std::result::Result<Vec<u8>, SignerError> {
            Ok(vec![0x30; 608])
        }

        fn sign_digest(&mut self, _digest: &[u8; 32]) -> std::result::Result<Vec<u8>, SignerError> {
            Ok(vec![0x44; 64])
        }
    }

    #[derive(Debug)]
    struct FixedRng(u8);

    impl RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            u32::from(self.0).wrapping_mul(0x0101_0101)
        }

        fn next_u64(&mut self) -> u64 {
            u64::from(self.next_u32()).wrapping_mul(0x0000_0001_0000_0001)
        }

        fn fill_bytes(&mut self, destination: &mut [u8]) {
            destination.fill(self.0);
            self.0 = self.0.wrapping_add(1);
        }

        fn try_fill_bytes(&mut self, destination: &mut [u8]) -> std::result::Result<(), RngError> {
            self.fill_bytes(destination);
            Ok(())
        }
    }

    impl CryptoRng for FixedRng {}

    fn request(body: Vec<u8>) -> RtspRequest {
        RtspRequest {
            method: "POST".to_owned(),
            target: "rtsp://carplay.local/auth-setup".to_owned(),
            version: "RTSP/1.0".to_owned(),
            headers: HeaderMap::from([("cseq".to_owned(), "7".to_owned())]),
            body,
        }
    }

    #[test]
    fn integrates_mfisap_with_the_rtsp_auth_setup_endpoint() {
        let peer_secret = StaticSecret::from([11; 32]);
        let peer_public = PublicKey::from(&peer_secret);
        let mut body = vec![1];
        body.extend_from_slice(peer_public.as_bytes());
        let verification = PairVerification::new("phone", [3; 32]).expect("verification");
        let mut handler = AuthSetupHandler::with_rng(FixedSigner, FixedRng(7));
        let response = handler
            .handle(&request(body), &verification)
            .expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some(OCTET_STREAM)
        );
        assert_eq!(response.body.len(), 32 + 4 + 608 + 4 + 64);
        assert_eq!(handler.state(), MfiSapState::ProofIssued);
    }

    #[test]
    fn malformed_request_returns_400_without_consuming_the_responder() {
        let verification = PairVerification::new("phone", [3; 32]).expect("verification");
        let mut handler = AuthSetupHandler::with_rng(FixedSigner, FixedRng(7));
        let response = handler
            .handle(&request(vec![1; 4]), &verification)
            .expect("bad request");
        assert_eq!(response.status, 400);
        assert_eq!(handler.state(), MfiSapState::AwaitingRequest);
    }
}
