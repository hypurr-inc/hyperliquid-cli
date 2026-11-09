//! Who signed what, read back from what the mock exchange received.

use alloy::dyn_abi::TypedData;
use alloy::primitives::{B256, Signature, U256, keccak256};
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::signing::agent_signing_hash;
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

/// The signer of an L1 action hypersdk's `Action` does not model, recovered
/// from the msgpack hash of `action` (a struct whose field order the test
/// fixes) and the nonce and signature `body` carries.
pub fn recover_raw_l1_signer(body: &Value, action: &impl Serialize, chain: Chain) -> String {
    let nonce = body["nonce"].as_u64().expect("a u64 nonce");
    let prehash = agent_signing_hash(chain, raw_l1_hash(action, nonce));
    recover_from_prehash(&body["signature"], prehash)
}

/// The signer of a user-signed action (`usdSend`, `approveAgent`, ...),
/// recovered from the EIP-712 digest of the action's own fields.
///
/// `type_string` is the typed-data type as the protocol documents it, for
/// example `HyperliquidTransaction:UsdSend(string hyperliquidChain,string
/// destination,string amount,uint64 time)`. The digest is built here from that
/// string and the fixed `HyperliquidSignTransaction` domain, without hypersdk,
/// so a CLI that signs a different field set or order recovers to another
/// address.
pub fn recover_user_signer(body: &Value, type_string: &str) -> String {
    let action = &body["action"];
    let chain_id = match action["hyperliquidChain"].as_str() {
        Some("Mainnet") => 0xa4b1,
        Some("Testnet") => 0x66eee,
        other => panic!("a user-signed action names its chain, not {other:?}"),
    };
    assert_eq!(
        action["signatureChainId"],
        format!("{chain_id:#x}"),
        "the typed-data domain is Arbitrum One for Mainnet and Arbitrum Sepolia for Testnet"
    );
    let (primary, fields) = type_string
        .strip_suffix(')')
        .and_then(|rest| rest.split_once('('))
        .expect("a type string like `Name(type field,...)`");
    let fields: Vec<(&str, &str)> = fields
        .split(',')
        .map(|field| field.split_once(' ').expect("`type name`"))
        .collect();
    let message: serde_json::Map<String, Value> = fields
        .iter()
        .map(|(_, name)| {
            let value = action
                .get(*name)
                .unwrap_or_else(|| panic!("the action has no `{name}`: {action}"));
            (name.to_string(), value.clone())
        })
        .collect();
    let typed_data: TypedData = serde_json::from_value(serde_json::json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"},
            ],
            primary: fields
                .iter()
                .map(|(ty, name)| serde_json::json!({"name": name, "type": ty}))
                .collect::<Vec<_>>(),
        },
        "primaryType": primary,
        "domain": {
            "name": "HyperliquidSignTransaction",
            "version": "1",
            "chainId": chain_id,
            "verifyingContract": "0x0000000000000000000000000000000000000000",
        },
        "message": message,
    }))
    .expect("typed data alloy can read");
    recover_from_prehash(
        &body["signature"],
        typed_data.eip712_signing_hash().expect("a signing hash"),
    )
}

fn recover_from_prehash(signature: &Value, prehash: B256) -> String {
    let scalar = |name: &str| {
        let text = signature[name].as_str().expect("a hex scalar");
        U256::from_str_radix(text.trim_start_matches("0x"), 16).expect("a hex scalar")
    };
    let parity = match signature["v"].as_u64().expect("a numeric v") {
        27 => false,
        28 => true,
        other => panic!("v is 27 or 28, not {other}"),
    };
    Signature::new(scalar("r"), scalar("s"), parity)
        .recover_address_from_prehash(&prehash)
        .expect("a signature that recovers")
        .to_string()
        .to_lowercase()
}

/// The typed-data types of the user-signed actions, as the protocol documents
/// them, for [`recover_user_signer`].
pub mod user_signed {
    pub const USD_CLASS_TRANSFER: &str = "HyperliquidTransaction:UsdClassTransfer(string hyperliquidChain,string amount,bool toPerp,uint64 nonce)";
    pub const USD_SEND: &str = "HyperliquidTransaction:UsdSend(string hyperliquidChain,string destination,string amount,uint64 time)";
    pub const SPOT_SEND: &str = "HyperliquidTransaction:SpotSend(string hyperliquidChain,string destination,string token,string amount,uint64 time)";
    pub const SEND_ASSET: &str = "HyperliquidTransaction:SendAsset(string hyperliquidChain,string destination,string sourceDex,string destinationDex,string token,string amount,string fromSubAccount,uint64 nonce)";
    pub const WITHDRAW: &str = "HyperliquidTransaction:Withdraw(string hyperliquidChain,string destination,string amount,uint64 time)";
    pub const TOKEN_DELEGATE: &str = "HyperliquidTransaction:TokenDelegate(string hyperliquidChain,address validator,uint64 wei,bool isUndelegate,uint64 nonce)";
    pub const C_DEPOSIT: &str =
        "HyperliquidTransaction:CDeposit(string hyperliquidChain,uint64 wei,uint64 nonce)";
    pub const C_WITHDRAW: &str =
        "HyperliquidTransaction:CWithdraw(string hyperliquidChain,uint64 wei,uint64 nonce)";
    pub const LINK_STAKING_USER: &str = "HyperliquidTransaction:LinkStakingUser(string hyperliquidChain,address user,bool isFinalize,uint64 nonce)";
    pub const APPROVE_BUILDER_FEE: &str = "HyperliquidTransaction:ApproveBuilderFee(string hyperliquidChain,string maxFeeRate,address builder,uint64 nonce)";
    pub const APPROVE_AGENT: &str = "HyperliquidTransaction:ApproveAgent(string hyperliquidChain,address agentAddress,string agentName,uint64 nonce)";
}
