// Copyright 2015-2019 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Trust dns implementation of Secret Key Transaction Authentication for DNS (TSIG)
//! [RFC 8945](https://www.rfc-editor.org/rfc/rfc8945) November 2020
//!
//! Current deviation from RFC in implementation as of 2022-10-28
//!
//! - Mac checking don't support HMAC truncation with TSIG (pedantic constant time verification)
//! - Time checking not in TSIG implementation but in caller

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use tracing::debug;

use super::rdata::DNSSECRData;
use super::rdata::tsig::{
    TSIG, TsigAlgorithm, make_tsig_record, message_tbs, signed_bitmessage_to_buf,
};
use super::{DnsSecError, DnsSecErrorKind};
use crate::error::{ProtoError, ProtoResult};
use crate::op::{Message, MessageFinalizer, MessageVerifier};
use crate::rr::{Name, RData, Record};
use crate::xfer::DnsResponse;

/// Struct to pass to a client for it to authenticate requests using TSIG.
#[derive(Clone)]
pub struct TSigner(Arc<TSignerInner>);

struct TSignerInner {
    key: Vec<u8>, // TODO this might want to be some sort of auto-zeroing on drop buffer, as it's cryptographic material
    algorithm: TsigAlgorithm,
    signer_name: Name,
    fudge: u16,
}

impl TSigner {
    /// Create a new Tsigner from its parts
    ///
    /// # Arguments
    ///
    /// * `key` - cryptographic key used to authenticate exchanges
    /// * `algorithm` - algorithm used to authenticate exchanges
    /// * `signer_name` - name of the key. Must match the name known to the server
    /// * `fudge` - maximum difference between client and server time, in seconds, see [fudge](TSigner::fudge) for details
    pub fn new(
        key: Vec<u8>,
        algorithm: TsigAlgorithm,
        mut signer_name: Name,
        fudge: u16,
    ) -> Result<Self, DnsSecError> {
        signer_name.set_fqdn(true);
        if algorithm.supported() {
            Ok(Self(Arc::new(TSignerInner {
                key,
                algorithm,
                signer_name,
                fudge,
            })))
        } else {
            Err(DnsSecErrorKind::TsigUnsupportedMacAlgorithm(algorithm).into())
        }
    }

    /// Return the key used for message authentication
    pub fn key(&self) -> &[u8] {
        &self.0.key
    }

    /// Return the algorithm used for message authentication
    pub fn algorithm(&self) -> &TsigAlgorithm {
        &self.0.algorithm
    }

    /// Name of the key used by this signer
    pub fn signer_name(&self) -> &Name {
        &self.0.signer_name
    }

    /// Maximum time difference between client time when issuing a message, and server time when
    /// receiving it, in second. If time is out, the server will consider the request invalid.
    /// Longer values means more room for replay by an attacker. A few minutes are usually a good
    /// value.
    pub fn fudge(&self) -> u16 {
        self.0.fudge
    }

    /// Compute authentication tag for a buffer
    pub fn sign(&self, tbs: &[u8]) -> Result<Vec<u8>, DnsSecError> {
        self.0.algorithm.mac_data(&self.0.key, tbs)
    }

    /// Compute authentication tag for a message
    pub fn sign_message(&self, message: &Message, pre_tsig: &TSIG) -> Result<Vec<u8>, DnsSecError> {
        self.sign(&message_tbs(None, message, pre_tsig, &self.0.signer_name)?)
    }

    /// Verify hmac in constant time to prevent timing attacks
    pub fn verify(&self, tbv: &[u8], tag: &[u8]) -> Result<(), DnsSecError> {
        self.0.algorithm.verify_mac(&self.0.key, tbv, tag)
    }

    /// Verify the message is correctly signed
    /// This does not perform time verification on its own, instead one should verify current time
    /// lie in returned Range
    ///
    /// # Arguments
    /// * `previous_hash` - Hash of the last message received before this one, or of the query for
    ///   the first message
    /// * `message` - byte buffer containing current message
    /// * `first_message` - is this the first response message
    ///
    /// # Returns
    /// Return Ok(_) on valid signature. Inner tuple contain the following values, in order:
    /// * a byte buffer containing the hash of this message. Need to be passed back when
    ///   authenticating next message
    /// * a Range of time that is acceptable
    /// * the time the signature was emitted. It must be greater or equal to the time of previous
    ///   messages, if any
    pub fn verify_message_byte(
        &self,
        previous_hash: Option<&[u8]>,
        message: &[u8],
        first_message: bool,
    ) -> Result<(Vec<u8>, Range<u64>, u64), DnsSecError> {
        let (tbv, record) = signed_bitmessage_to_buf(previous_hash, message, first_message)?;
        let tsig = if let RData::DNSSEC(DNSSECRData::TSIG(tsig)) = record.data() {
            tsig
        } else {
            unreachable!("tsig::signed_message_to_buff always returns a TSIG record")
        };

        // https://tools.ietf.org/html/rfc8945#section-5.2
        // 1.  Check key
        if record.name() != &self.0.signer_name || tsig.algorithm() != &self.0.algorithm {
            return Err(DnsSecErrorKind::TsigWrongKey.into());
        }

        // 2.  Check MAC
        //  note: that this verification does not allow for truncation of the HMAC, which technically the RFC suggests.
        //    this is to be pedantic about constant time HMAC validation (prevent timing attacks) as well as any security
        //    concerns about MAC truncation and collisions.
        if tsig.mac().len() < tsig.algorithm().output_len()? {
            return Err(DnsSecError::from(
                "Please file an issue with https://github.com/hickory-dns/hickory-dns to support truncated HMACs with TSIG",
            ));
        }

        // verify the MAC
        let mac = tsig.mac();
        self.verify(&tbv, mac)?;

        // 3.  Check time values
        // we don't actually have time here so we will let upper level decide
        // this is technically in violation of the RFC, in case both time and
        // truncation policy are bad, time should be reported and this code will report
        // truncation issue instead

        // 4.  Check truncation policy
        //   see not above in regards to not supporting verification of truncated HMACs.
        // if tsig.mac().len() < std::cmp::max(10, self.0.algorithm.output_len()? / 2) {
        //     return Err(ProtoError::from(
        //         "tsig validation error: truncated signature",
        //     ));
        // }

        Ok((
            tsig.mac().to_vec(),
            Range {
                start: tsig.time() - tsig.fudge() as u64,
                end: tsig.time() + tsig.fudge() as u64,
            },
            tsig.time(),
        ))
    }
}

impl MessageFinalizer for TSigner {
    fn finalize_message(
        &self,
        message: &Message,
        current_time: u32,
    ) -> ProtoResult<(Vec<Record>, Option<MessageVerifier>)> {
        debug!("signing message: {:?}", message);
        let current_time = current_time as u64;

        let pre_tsig = TSIG::new(
            self.0.algorithm.clone(),
            current_time,
            self.0.fudge,
            Vec::new(),
            message.id(),
            0,
            Vec::new(),
        );
        let mut signature: Vec<u8> = self
            .sign_message(message, &pre_tsig)
            .map_err(|err| ProtoError::from(err.to_string()))?;
        let tsig = make_tsig_record(
            self.0.signer_name.clone(),
            pre_tsig.set_mac(signature.clone()),
        );
        let self2 = self.clone();
        let mut remote_time = 0;
        let verifier = move |dns_response: &[u8]| {
            let (last_sig, range, rt) = self2
                .verify_message_byte(Some(signature.as_ref()), dns_response, remote_time == 0)
                .map_err(|err| ProtoError::from(err.to_string()))?;
            if rt >= remote_time && range.contains(&current_time)
            // this assumes a no-latency answer
            {
                signature = last_sig;
                remote_time = rt;
                DnsResponse::from_buffer(dns_response.to_vec())
            } else {
                Err(ProtoError::from("tsig validation error: outdated response"))
            }
        };
        Ok((vec![tsig], Some(Box::new(verifier))))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::dbg_macro, clippy::print_stdout)]

    use crate::op::{Message, MessageSignature, Query};
    use crate::rr::Name;
    use crate::serialize::binary::BinEncodable;

    use super::*;
    fn assert_send_and_sync<T: Send + Sync>() {}

    #[test]
    fn test_send_and_sync() {
        assert_send_and_sync::<TSigner>();
    }

    #[test]
    fn test_sign_and_verify_message_tsig() {
        let time_begin = 1609459200u64;
        let fudge = 300u64;
        let origin: Name = Name::parse("example.com.", None).unwrap();
        let key_name: Name = Name::from_ascii("key_name.").unwrap();
        let mut question: Message = Message::new();
        let mut query: Query = Query::new();
        query.set_name(origin);
        question.add_query(query);

        let sig_key = b"some_key".to_vec();
        let signer =
            TSigner::new(sig_key, TsigAlgorithm::HmacSha512, key_name, fudge as u16).unwrap();

        assert_eq!(question.signature(), &MessageSignature::Unsigned);
        question
            .finalize(&signer, time_begin as u32)
            .expect("should have signed");
        assert!(matches!(question.signature(), &MessageSignature::Tsig(_)));

        let (_, validity_range, _) = signer
            .verify_message_byte(None, &question.to_bytes().unwrap(), true)
            .unwrap();
        assert!(validity_range.contains(&(time_begin + fudge / 2))); // slightly outdated, but still to be acceptable
        assert!(validity_range.contains(&(time_begin - fudge / 2))); // sooner than our time, but still acceptable
        assert!(!validity_range.contains(&(time_begin + fudge * 2))); // too late to be accepted
        assert!(!validity_range.contains(&(time_begin - fudge * 2))); // too soon to be accepted
    }

    // make rejection tests shorter by centralizing common setup code
    fn get_message_and_signer() -> (Message, TSigner) {
        let time_begin = 1609459200u64;
        let fudge = 300u64;
        let origin: Name = Name::parse("example.com.", None).unwrap();
        let key_name: Name = Name::from_ascii("key_name.").unwrap();
        let mut question: Message = Message::new();
        let mut query: Query = Query::new();
        query.set_name(origin);
        question.add_query(query);

        let sig_key = b"some_key".to_vec();
        let signer =
            TSigner::new(sig_key, TsigAlgorithm::HmacSha512, key_name, fudge as u16).unwrap();

        assert_eq!(question.signature(), &MessageSignature::Unsigned);
        question
            .finalize(&signer, time_begin as u32)
            .expect("should have signed");
        assert!(matches!(question.signature(), &MessageSignature::Tsig(_)));

        // this should be ok, it has not been tampered with
        assert!(
            signer
                .verify_message_byte(None, &question.to_bytes().unwrap(), true)
                .is_ok()
        );

        (question, signer)
    }

    #[test]
    fn test_sign_and_verify_message_tsig_reject_keyname() {
        let (mut question, signer) = get_message_and_signer();

        let other_name: Name = Name::from_ascii("other_name.").unwrap();
        let MessageSignature::Tsig(mut signature) = question.take_signature() else {
            panic!("should have TSIG signed");
        };
        signature.set_name(other_name);
        question.set_signature(MessageSignature::Tsig(signature));

        assert!(
            signer
                .verify_message_byte(None, &question.to_bytes().unwrap(), true)
                .is_err()
        );
    }

    #[test]
    fn test_sign_and_verify_message_tsig_reject_invalid_mac() {
        let (mut question, signer) = get_message_and_signer();

        let mut query: Query = Query::new();
        let origin: Name = Name::parse("example.net.", None).unwrap();
        query.set_name(origin);
        question.add_query(query);

        assert!(
            signer
                .verify_message_byte(None, &question.to_bytes().unwrap(), true)
                .is_err()
        );
    }
}
