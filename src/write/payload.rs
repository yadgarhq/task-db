//! What D9's fingerprint is taken over, for each of the three mutating RPCs.
//!
//! Split out of `write.rs` when that file crossed the shared 500-line ceiling.
//! The trait and its three impls stood there unchanged, and they are a unit: no
//! part of them touches a pool, a transaction or [`crate::service::TaskDb`].

use prost::Message as _;

use crate::pb::yadgar::task::v1::*;

/// The bytes D9's fingerprint is taken over.
///
/// Which fields count as the payload is per-RPC, and D9 requires that written
/// down rather than inferred. This module's answer is the same for all three of
/// its mutating RPCs: **every field of the request except `scope`,
/// `idempotency` and `caller_fingerprint`** — see the header of `src/idem.rs`
/// for why those three, and only those three, are excluded.
///
/// **`caller_fingerprint` IS CLEARED, AND NO KNOWN-ANSWER TEST OVER AN EMPTY
/// ONE CAN SEE IT.** proto3 encodes empty `bytes` as nothing, so a request that
/// leaves it empty digests the same whether it is cleared or not. The tests
/// that send it POPULATED are what pin the exclusion.
///
/// Stated as "clear the two, keep the rest" rather than as a list of the fields
/// to hash. The list would be the same today and would silently stop being
/// right the day the contract grows a field: a new one nobody added here would
/// be omitted from the digest, and a request differing only in it would be
/// replayed. The exclusions are what this module has actually decided about.
pub(super) trait Payload {
    fn payload(&self) -> Vec<u8>;
}

impl Payload for CreateTaskRequest {
    fn payload(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.scope = None;
        canonical.idempotency = None;
        canonical.encode_to_vec()
    }
}

impl Payload for UpdateTaskRequest {
    fn payload(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.scope = None;
        canonical.idempotency = None;
        canonical.caller_fingerprint = Vec::new();
        // A mask is a SET of field names, and its encoding is a sequence. Two
        // masks naming the same fields in another order, or one naming a field
        // twice, ask for the identical write — so a digest taken over the bytes
        // as they arrived would refuse a request that discards nothing. D9's
        // test is whether a replay would silently discard the difference.
        if let Some(mask) = canonical.update_mask.as_mut() {
            mask.paths.sort();
            mask.paths.dedup();
        }
        canonical.encode_to_vec()
    }
}

impl Payload for DeleteTaskRequest {
    fn payload(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.scope = None;
        canonical.idempotency = None;
        canonical.caller_fingerprint = Vec::new();
        canonical.encode_to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure half of what `tests/known_answer.rs` pins end to end: requests
    /// identical but for `caller_fingerprint` — absent, or two different 32-byte
    /// values — digest identically.
    #[test]
    fn the_store_payload_excludes_the_caller_fingerprint() {
        let update = |caller: Vec<u8>| UpdateTaskRequest {
            id: "yadgar:task:x".into(),
            expect_version: 3,
            caller_fingerprint: caller,
            ..Default::default()
        };
        assert_eq!(update(vec![]).payload(), update(vec![1; 32]).payload());
        assert_eq!(update(vec![1; 32]).payload(), update(vec![2; 32]).payload());

        let delete = |caller: Vec<u8>| DeleteTaskRequest {
            id: "yadgar:task:x".into(),
            expect_version: 3,
            caller_fingerprint: caller,
            ..Default::default()
        };
        assert_eq!(delete(vec![]).payload(), delete(vec![1; 32]).payload());
        assert_eq!(delete(vec![1; 32]).payload(), delete(vec![2; 32]).payload());
    }
}
