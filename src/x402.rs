//! RFC-0046 S1: local verification of an x402 `exact` authorisation.
//!
//! Arithmetic over bytes already in hand: the EIP-712 signer is the stated payer, the recipient is
//! our configured address, the amount covers the price, the window is open, the network matches.
//! `now` is passed in so this is a pure function. No facilitator, no RPC, no filesystem.
//!
//! The digest is written out. A wrong construction recovers to some address rather than erroring,
//! so it presents as a forged payment rather than as our bug; tests sign with alloy's EIP-712
//! hasher, not with the function under test.

use std::str::FromStr;

use alloy_primitives::{address, b256, keccak256, Address, Signature, B256, U256};
use base64::Engine;
use serde::Deserialize;

/// EIP-3009 `TransferWithAuthorization` typehash. Recomputed in tests rather than trusted.
const TRANSFER_WITH_AUTHORIZATION_TYPEHASH: B256 =
    b256!("0x7c7c6cdb67a18743f49ec6fa9b35f50d52ed05cbed4cc592e13b44501c1a2267");

const DOMAIN_TYPE_HASH: B256 =
    b256!("0x8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f");

/// USDC on Base and Base Sepolia. Same constants lodestar pinned from live observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellerNetwork {
    Mainnet,
    Testnet,
}

/// Chain parameters the EIP-712 domain is built from. Ours, not the payment's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainParams {
    pub network: &'static str,
    pub asset: Address,
    pub asset_name: &'static str,
    pub asset_version: &'static str,
    pub chain_id: u64,
}

impl SellerNetwork {
    pub fn params(self) -> ChainParams {
        match self {
            Self::Mainnet => ChainParams {
                network: "eip155:8453",
                asset: address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
                asset_name: "USD Coin",
                asset_version: "2",
                chain_id: 8453,
            },
            Self::Testnet => ChainParams {
                network: "eip155:84532",
                asset: address!("0x036CbD53842c5426634e7929541eC2318f3dCF7e"),
                asset_name: "USDC",
                asset_version: "2",
                chain_id: 84532,
            },
        }
    }
}

/// What we will accept. Every check in [`verify_payment`] is against this, not against the
/// payment's own claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SellerConfig {
    pub network: SellerNetwork,
    pub pay_to: Address,
    pub price_base_units: U256,
}

/// A payment that authorised what we asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub from: Address,
    pub value: U256,
    pub nonce: B256,
}

/// Why a presented payment is not a payment to us. Negative tests match these, not a generic reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotBase64Json,
    NoAuthorization,
    UnsupportedScheme(String),
    WrongNetwork { got: String, want: &'static str },
    MissingFields,
    PaysSomebodyElse,
    UnparseableAmount,
    Insufficient { authorized: U256, price: U256 },
    NotYetValid,
    Expired,
    SignatureDidNotRecover,
    SignatureDoesNotMatchPayer,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBase64Json => write!(f, "payment header is not base64 JSON"),
            Self::NoAuthorization => write!(f, "payment carries no authorization"),
            Self::UnsupportedScheme(s) => write!(f, "unsupported scheme {s}"),
            Self::WrongNetwork { got, want } => {
                write!(f, "payment is for {got}, not {want}")
            }
            Self::MissingFields => write!(f, "authorization is missing fields"),
            Self::PaysSomebodyElse => write!(f, "authorization pays somebody else"),
            Self::UnparseableAmount => write!(f, "unparseable amount"),
            Self::Insufficient { authorized, price } => {
                write!(f, "authorized {authorized}, price is {price}")
            }
            Self::NotYetValid => write!(f, "authorization is not yet valid"),
            Self::Expired => write!(f, "authorization has expired"),
            Self::SignatureDidNotRecover => write!(f, "signature did not recover"),
            Self::SignatureDoesNotMatchPayer => {
                write!(f, "signature does not match the stated payer")
            }
        }
    }
}

pub type VerifyResult = Result<Accepted, Refusal>;

#[derive(Debug, Deserialize)]
struct PaymentPayload {
    scheme: Option<String>,
    network: Option<String>,
    payload: Option<PaymentBody>,
}

#[derive(Debug, Deserialize)]
struct PaymentBody {
    signature: Option<String>,
    authorization: Option<AuthorizationWire>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationWire {
    from: Option<String>,
    to: Option<String>,
    value: Option<String>,
    #[serde(rename = "validAfter")]
    valid_after: Option<String>,
    #[serde(rename = "validBefore")]
    valid_before: Option<String>,
    nonce: Option<String>,
}

fn word_addr(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

fn word_u256(n: U256) -> [u8; 32] {
    n.to_be_bytes::<32>()
}

fn keccak_words(words: &[[u8; 32]]) -> B256 {
    let mut buf = Vec::with_capacity(words.len() * 32);
    for w in words {
        buf.extend_from_slice(w);
    }
    keccak256(buf)
}

/// EIP-712 digest for an EIP-3009 `TransferWithAuthorization`.
///
/// Field order is load-bearing. Swapping two fields, or using the wrong typehash, still produces a
/// 32-byte digest that recovers to *some* address.
fn authorization_digest(
    chain: &ChainParams,
    from: Address,
    to: Address,
    value: U256,
    valid_after: U256,
    valid_before: U256,
    nonce: B256,
) -> B256 {
    let domain_separator = keccak_words(&[
        DOMAIN_TYPE_HASH.into(),
        keccak256(chain.asset_name.as_bytes()).into(),
        keccak256(chain.asset_version.as_bytes()).into(),
        word_u256(U256::from(chain.chain_id)),
        word_addr(chain.asset),
    ]);
    let struct_hash = keccak_words(&[
        TRANSFER_WITH_AUTHORIZATION_TYPEHASH.into(),
        word_addr(from),
        word_addr(to),
        word_u256(value),
        word_u256(valid_after),
        word_u256(valid_before),
        nonce.into(),
    ]);
    let mut prefixed = [0u8; 66];
    prefixed[0] = 0x19;
    prefixed[1] = 0x01;
    prefixed[2..34].copy_from_slice(domain_separator.as_slice());
    prefixed[34..66].copy_from_slice(struct_hash.as_slice());
    keccak256(prefixed)
}

fn parse_addr(s: &str) -> Option<Address> {
    Address::from_str(s).ok()
}

fn parse_b256(s: &str) -> Option<B256> {
    B256::from_str(s).ok()
}

/// Check that a presented payment authorises what we asked for.
///
/// `header_value` is the base64 JSON an x402 `Payment-Signature` header carries. `now` is Unix
/// seconds, supplied by the caller so this stays a pure function.
pub fn verify_payment(cfg: &SellerConfig, header_value: &str, now: u64) -> VerifyResult {
    let chain = cfg.network.params();

    let json = base64::engine::general_purpose::STANDARD
        .decode(header_value.trim().as_bytes())
        .ok()
        .and_then(|b| serde_json::from_slice::<PaymentPayload>(&b).ok())
        .ok_or(Refusal::NotBase64Json)?;

    let body = json.payload.ok_or(Refusal::NoAuthorization)?;
    let auth = body.authorization.ok_or(Refusal::NoAuthorization)?;
    let sig = body.signature.ok_or(Refusal::NoAuthorization)?;
    match json.scheme.as_deref() {
        Some("exact") => {}
        Some(scheme) => return Err(Refusal::UnsupportedScheme(scheme.to_string())),
        None => return Err(Refusal::MissingFields),
    }
    match json.network.as_deref() {
        Some(network) if network == chain.network => {}
        Some(network) => {
            return Err(Refusal::WrongNetwork {
                got: network.to_string(),
                want: chain.network,
            });
        }
        None => return Err(Refusal::MissingFields),
    }

    let from = auth.from.as_deref().and_then(parse_addr);
    let to = auth.to.as_deref().and_then(parse_addr);
    let nonce = auth.nonce.as_deref().and_then(parse_b256);
    let (from, to, nonce) = match (from, to, nonce) {
        (Some(f), Some(t), Some(n)) => (f, t, n),
        _ => return Err(Refusal::MissingFields),
    };
    let valid_after = auth
        .valid_after
        .as_deref()
        .and_then(|s| U256::from_str(s).ok());
    let valid_before = auth
        .valid_before
        .as_deref()
        .and_then(|s| U256::from_str(s).ok());
    let (valid_after, valid_before) = match (valid_after, valid_before) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err(Refusal::MissingFields),
    };

    if to != cfg.pay_to {
        return Err(Refusal::PaysSomebodyElse);
    }

    let amount = match auth.value.as_deref().and_then(|s| U256::from_str(s).ok()) {
        Some(v) => v,
        None => return Err(Refusal::UnparseableAmount),
    };
    if amount < cfg.price_base_units {
        return Err(Refusal::Insufficient {
            authorized: amount,
            price: cfg.price_base_units,
        });
    }

    // A window that has not opened or has closed is not a payment we can settle: the token will
    // refuse it, so serving against it would be giving the answer away.
    let now = U256::from(now);
    if now < valid_after {
        return Err(Refusal::NotYetValid);
    }
    if now >= valid_before {
        return Err(Refusal::Expired);
    }

    let digest = authorization_digest(&chain, from, to, amount, valid_after, valid_before, nonce);
    let signature = Signature::from_str(sig.trim()).map_err(|_| Refusal::SignatureDidNotRecover)?;
    let recovered = signature
        .recover_address_from_prehash(&digest)
        .map_err(|_| Refusal::SignatureDidNotRecover)?;
    if recovered != from {
        return Err(Refusal::SignatureDoesNotMatchPayer);
    }

    Ok(Accepted {
        from,
        value: amount,
        nonce,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::{eip712_domain, sol, SolStruct};
    use k256::ecdsa::SigningKey;

    sol! {
        struct TransferWithAuthorization {
            address from;
            address to;
            uint256 value;
            uint256 validAfter;
            uint256 validBefore;
            bytes32 nonce;
        }
    }

    /// Anvil/Hardhat account #1. Signing happens through k256; the digest it signs is alloy's.
    const PAYER_KEY: [u8; 32] =
        alloy_primitives::hex!("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");

    const NOW: u64 = 1_800_000_000;

    fn payer() -> (SigningKey, Address) {
        let sk = SigningKey::from_slice(&PAYER_KEY).expect("anvil key");
        let addr = Address::from_private_key(&sk);
        (sk, addr)
    }

    fn cfg() -> SellerConfig {
        SellerConfig {
            network: SellerNetwork::Testnet,
            pay_to: address!("0x1111111111111111111111111111111111111111"),
            price_base_units: U256::from(1000u64),
        }
    }

    struct Auth {
        from: Address,
        to: Address,
        value: U256,
        valid_after: U256,
        valid_before: U256,
        nonce: B256,
    }

    fn default_auth(from: Address, cfg: &SellerConfig) -> Auth {
        Auth {
            from,
            to: cfg.pay_to,
            value: cfg.price_base_units,
            valid_after: U256::from(NOW - 60),
            valid_before: U256::from(NOW + 600),
            nonce: keccak256(b"nonce-1"),
        }
    }

    /// Sign with alloy-sol-types' hasher, not ours. That is the independent EIP-712 implementation.
    fn sign_authorization(cfg: &SellerConfig, auth: &Auth) -> String {
        let (sk, _) = payer();
        let chain = cfg.network.params();
        let domain = eip712_domain! {
            name: chain.asset_name,
            version: chain.asset_version,
            chain_id: chain.chain_id,
            verifying_contract: chain.asset,
        };
        let msg = TransferWithAuthorization {
            from: auth.from,
            to: auth.to,
            value: auth.value,
            validAfter: auth.valid_after,
            validBefore: auth.valid_before,
            nonce: auth.nonce,
        };
        let digest = msg.eip712_signing_hash(&domain);
        let (sig, recid) = sk
            .sign_prehash_recoverable(digest.as_slice())
            .expect("sign");
        let signature = Signature::from((sig, recid));

        let payload = serde_json::json!({
            "x402Version": 1,
            "scheme": "exact",
            "network": chain.network,
            "payload": {
                "signature": signature.to_string(),
                "authorization": {
                    "from": auth.from.to_string(),
                    "to": auth.to.to_string(),
                    "value": auth.value.to_string(),
                    "validAfter": auth.valid_after.to_string(),
                    "validBefore": auth.valid_before.to_string(),
                    "nonce": auth.nonce.to_string(),
                }
            }
        });
        base64::engine::general_purpose::STANDARD.encode(payload.to_string().as_bytes())
    }

    fn decode_header(header: &str) -> serde_json::Value {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(header.as_bytes())
            .unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    fn encode_header(v: &serde_json::Value) -> String {
        base64::engine::general_purpose::STANDARD.encode(v.to_string().as_bytes())
    }

    #[test]
    fn the_typehash_is_the_one_eip3009_specifies_recomputed_rather_than_trusted() {
        assert_eq!(
            keccak256(
                b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)"
            ),
            TRANSFER_WITH_AUTHORIZATION_TYPEHASH
        );
        assert_eq!(
            keccak256(
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
            ),
            DOMAIN_TYPE_HASH
        );
    }

    #[test]
    fn our_digest_matches_alloy_sol_types() {
        let cfg = cfg();
        let (_, from) = payer();
        let auth = default_auth(from, &cfg);
        let chain = cfg.network.params();
        let domain = eip712_domain! {
            name: chain.asset_name,
            version: chain.asset_version,
            chain_id: chain.chain_id,
            verifying_contract: chain.asset,
        };
        let msg = TransferWithAuthorization {
            from: auth.from,
            to: auth.to,
            value: auth.value,
            validAfter: auth.valid_after,
            validBefore: auth.valid_before,
            nonce: auth.nonce,
        };
        let ours = authorization_digest(
            &chain,
            auth.from,
            auth.to,
            auth.value,
            auth.valid_after,
            auth.valid_before,
            auth.nonce,
        );
        assert_eq!(ours, msg.eip712_signing_hash(&domain));
    }

    #[test]
    fn a_signature_made_by_an_independent_eip712_implementation_verifies() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        let got = verify_payment(&cfg, &header, NOW).expect("independent signature must verify");
        assert_eq!(got.from, from);
        assert_eq!(got.value, U256::from(1000u64));
    }

    #[test]
    fn an_overpayment_is_still_the_price() {
        let cfg = cfg();
        let (_, from) = payer();
        let mut auth = default_auth(from, &cfg);
        auth.value = U256::from(5000u64);
        let header = sign_authorization(&cfg, &auth);
        assert!(verify_payment(&cfg, &header, NOW).is_ok());
    }

    #[test]
    fn refuses_an_underpayment() {
        let cfg = cfg();
        let (_, from) = payer();
        let mut auth = default_auth(from, &cfg);
        auth.value = U256::from(999u64);
        let header = sign_authorization(&cfg, &auth);
        assert_eq!(
            verify_payment(&cfg, &header, NOW),
            Err(Refusal::Insufficient {
                authorized: U256::from(999u64),
                price: U256::from(1000u64),
            })
        );
    }

    #[test]
    fn refuses_a_payment_addressed_to_another_recipient() {
        let cfg = cfg();
        let (_, from) = payer();
        let mut auth = default_auth(from, &cfg);
        auth.to = address!("0x2222222222222222222222222222222222222222");
        let header = sign_authorization(&cfg, &auth);
        assert_eq!(
            verify_payment(&cfg, &header, NOW),
            Err(Refusal::PaysSomebodyElse)
        );
    }

    #[test]
    fn refuses_a_payment_signed_by_someone_other_than_the_stated_payer() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        let mut obj = decode_header(&header);
        obj["payload"]["authorization"]["from"] =
            serde_json::Value::String("0x3333333333333333333333333333333333333333".into());
        assert_eq!(
            verify_payment(&cfg, &encode_header(&obj), NOW),
            Err(Refusal::SignatureDoesNotMatchPayer)
        );
    }

    #[test]
    fn refuses_an_authorization_whose_window_has_closed() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        assert_eq!(
            verify_payment(&cfg, &header, NOW + 100_000),
            Err(Refusal::Expired)
        );
    }

    #[test]
    fn refuses_an_authorization_whose_window_has_not_opened() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        assert_eq!(
            verify_payment(&cfg, &header, NOW - 100_000),
            Err(Refusal::NotYetValid)
        );
    }

    #[test]
    fn refuses_a_payment_for_the_wrong_network() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        let mut obj = decode_header(&header);
        obj["network"] = serde_json::Value::String("eip155:1".into());
        assert_eq!(
            verify_payment(&cfg, &encode_header(&obj), NOW),
            Err(Refusal::WrongNetwork {
                got: "eip155:1".into(),
                want: "eip155:84532",
            })
        );
    }

    #[test]
    fn refuses_a_payment_missing_scheme() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        let mut obj = decode_header(&header);
        obj.as_object_mut().unwrap().remove("scheme");
        assert_eq!(
            verify_payment(&cfg, &encode_header(&obj), NOW),
            Err(Refusal::MissingFields)
        );
    }

    #[test]
    fn refuses_a_payment_missing_network() {
        let cfg = cfg();
        let (_, from) = payer();
        let header = sign_authorization(&cfg, &default_auth(from, &cfg));
        let mut obj = decode_header(&header);
        obj.as_object_mut().unwrap().remove("network");
        assert_eq!(
            verify_payment(&cfg, &encode_header(&obj), NOW),
            Err(Refusal::MissingFields)
        );
    }

    #[test]
    fn refuses_an_authorization_at_valid_before() {
        let cfg = cfg();
        let (_, from) = payer();
        let auth = default_auth(from, &cfg);
        let header = sign_authorization(&cfg, &auth);
        let at_expiry: u64 = auth.valid_before.try_into().expect("fits u64");
        assert_eq!(
            verify_payment(&cfg, &header, at_expiry),
            Err(Refusal::Expired)
        );
    }

    #[test]
    fn refuses_malformed_payment_headers() {
        let cfg = cfg();
        assert_eq!(
            verify_payment(&cfg, "not base64 at all", NOW),
            Err(Refusal::NotBase64Json)
        );
        let empty = base64::engine::general_purpose::STANDARD.encode(b"{}");
        assert_eq!(
            verify_payment(&cfg, &empty, NOW),
            Err(Refusal::NoAuthorization)
        );
    }

    #[test]
    fn a_valid_signature_recovers_as_someone_else_when_the_digest_swaps_from_and_to() {
        // The failure mode RFC-0046 §7 names: a bad construction does not error, it recovers to
        // the wrong person, and then looks like a forged payment.
        let cfg = cfg();
        let (_, from) = payer();
        let auth = default_auth(from, &cfg);
        let header = sign_authorization(&cfg, &auth);
        let obj = decode_header(&header);
        let sig = Signature::from_str(obj["payload"]["signature"].as_str().unwrap()).unwrap();
        let chain = cfg.network.params();
        let swapped = authorization_digest(
            &chain,
            auth.to,
            auth.from,
            auth.value,
            auth.valid_after,
            auth.valid_before,
            auth.nonce,
        );
        let recovered = sig.recover_address_from_prehash(&swapped).unwrap();
        assert_ne!(
            recovered, from,
            "swapping from/to must not still recover as the payer"
        );
    }
}
