//! Who signed what, read back from what the mock exchange received.

use alloy::primitives::{B256, keccak256};
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::ActionRequest;
use serde::Serialize;
use serde_json::Value;

use super::{VALID_PRIVATE_KEY, expected_address};

/// The address [`VALID_PRIVATE_KEY`] signs as, lowercase like
/// [`recover_signer`] returns it.
pub fn test_signer() -> String {
    expected_address(VALID_PRIVATE_KEY).to_lowercase()
}

/// The address that signed an `/exchange` request `body`, recovered by hypersdk
/// from the action, nonce, vault and expiry the body carries, as lowercase hex.
/// It is the signer only if the CLI hashed those the way hypersdk does, so this
/// proves the CLI agrees with the library it signs with, not the protocol.
pub fn recover_signer(body: &Value, chain: Chain) -> String {
    serde_json::from_value::<ActionRequest>(body.clone())
        .expect("a request hypersdk can read")
        .recover(chain)
        .expect("a signature that recovers")
        .to_string()
        .to_lowercase()
}

/// The hash an L1 action signs that hypersdk's `Action` does not model: the
/// msgpack of `action`, then the nonce, then no vault, keccak-hashed.
pub fn raw_l1_hash(action: &impl Serialize, nonce: u64) -> B256 {
    let mut bytes = rmp_serde::to_vec_named(action).unwrap();
    bytes.extend(nonce.to_be_bytes());
    bytes.push(0);
    keccak256(bytes)
}

/// A signing command's JSON receipt names the test key as signer and as the
/// account it acts for.
pub fn assert_signed_by_test_key(receipt: &Value) {
    let signer = expected_address(VALID_PRIVATE_KEY);
    assert_eq!(receipt["signer"], signer);
    assert_eq!(receipt["acting_as"], signer);
}
