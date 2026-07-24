//! Centralized classification of RPC / transaction error strings.
//!
//! Ethereum nodes and RPC providers (Geth, Erigon, Nethermind, Alchemy,
//! publicnode, …) describe the *same* condition with different wording, and
//! several embed a JSON-RPC error code. Historically the mint hot path matched
//! these strings ad-hoc in several places, so a phrase that one provider used
//! but another didn't could silently change a retry / RBF decision.
//!
//! This module is the single source of truth. Each predicate owns its phrase
//! list; [`classify`] composes them with a fixed priority; and
//! [`classify_mint_error`] preserves the original `"fatal"` / `"retryable"`
//! contract used by the mint loop. All matching is case-insensitive.

/// A coarse category for a transaction / RPC error string, used to decide how
/// the mint loop should react.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxErrorKind {
    /// Sender cannot pay — never retry, never report "dry-run OK".
    InsufficientFunds,
    /// Known unrecoverable contract/wallet revert (wrong proof, sold out, …).
    FatalContract,
    /// Node already has this tx in its pool — treat as accepted (dup broadcast).
    AlreadyKnown,
    /// Nonce below account nonce — refresh nonce and retry.
    NonceTooLow,
    /// Fee below the pool's minimum / replacement — bump fee and retry (RBF).
    Underpriced,
    /// Gas limit below the intrinsic minimum — bump the gas limit and retry.
    IntrinsicGasTooLow,
    /// Anything else — transient by default (phase not open, RPC noise, …).
    Retryable,
}

/// Sender has insufficient balance for `value + gas`. Fatal.
///
/// The `insufficient funds` / `insufficient balance` substrings already cover
/// the common Geth/Erigon phrasings (e.g. `insufficient funds for gas * price +
/// value`, `insufficient funds for transfer`).
pub fn is_insufficient_funds(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("insufficient funds")
        || l.contains("insufficient balance")
        || l.contains("outoffunds")
        || l.contains("out of funds")
        || l.contains("out of fund")
        || l.contains("exceeds balance")
        || l.contains("overshot the sender account's balance")
}

/// Known unrecoverable contract/allowlist errors. Fatal.
///
/// Generic `execution reverted` is deliberately **not** here: it is usually
/// transient (phase not open yet, momentary sold-out, RPC noise) so retry
/// bursts can keep trying.
pub fn is_fatal_contract(msg: &str) -> bool {
    const FATAL_PATTERNS: [&str; 6] = [
        "invalidproof",
        "payernotallowed",
        "signaturealreadyused",
        "incorrectpayment",
        "mintquantityexceedsmaxmintedperwallet",
        "mintquantityexceedsmaxsupply",
    ];
    let l = msg.to_lowercase();
    FATAL_PATTERNS.iter().any(|p| l.contains(p))
}

/// Node already has this transaction. Treat as accepted (a parallel broadcast
/// beat us to the same node).
pub fn is_already_known(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("already known")
        // safe synonyms across providers:
        || l.contains("alreadyknown")
        || l.contains("known transaction")
        || l.contains("already imported")
}

/// Nonce is at or below the account's on-chain nonce → refresh nonce and retry.
pub fn is_nonce_too_low(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("nonce too low")
        || l.contains("nonce is too low")
        // safe synonyms:
        || l.contains("nonce_too_low")
        || l.contains("oldnonce")
}

/// Fee below the pool minimum or below an existing tx being replaced → bump the
/// fee and retry (replace-by-fee).
pub fn is_underpriced(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("underpriced")
        || l.contains("fee too low")
        // Geth: fee cap below current base fee — also a "raise the fee" case.
        || l.contains("max fee per gas less than block base fee")
}

/// Gas limit below the intrinsic minimum for the calldata (common on L2/Orbit).
pub fn is_intrinsic_gas_too_low(msg: &str) -> bool {
    let l = msg.to_lowercase();
    // Require intrinsic / gas-limit context. Bare "gas too low" was too broad and
    // also matched fee-underpriced phrasings (e.g. "max priority fee per gas too
    // low"), which misdrove the retry loop to bump the gas *limit* instead of the
    // *fee* (audit L10).
    l.contains("intrinsic gas too low")
        || l.contains("gas limit too low")
        || (l.contains("intrinsic") && l.contains("gas") && l.contains("too low"))
}

/// Best-effort extraction of a JSON-RPC error code embedded in a formatted
/// error string (e.g. `... error: {"code":-32000,"message":"..."}`). Returned
/// for logging / observability; classification stays phrase-based because a
/// single code (notably `-32000`) is reused for many distinct conditions.
pub fn json_rpc_code(msg: &str) -> Option<i64> {
    let idx = msg.find("\"code\"").or_else(|| msg.find("code:"))?;
    let rest = &msg[idx..];
    // find the first signed integer after the marker
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !(bytes[i] == b'-' || bytes[i].is_ascii_digit()) {
        i += 1;
    }
    let start = i;
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > start {
        rest[start..i].parse::<i64>().ok()
    } else {
        None
    }
}

/// Classify an error string into a [`TxErrorKind`]. Priority (highest first):
/// funds → fatal contract → already-known → nonce-too-low → underpriced →
/// intrinsic-gas → retryable. The order mirrors how the mint loop branches:
/// unrecoverable conditions win over recoverable ones.
pub fn classify(msg: &str) -> TxErrorKind {
    if is_insufficient_funds(msg) {
        TxErrorKind::InsufficientFunds
    } else if is_fatal_contract(msg) {
        TxErrorKind::FatalContract
    } else if is_already_known(msg) {
        TxErrorKind::AlreadyKnown
    } else if is_nonce_too_low(msg) {
        TxErrorKind::NonceTooLow
    } else if is_underpriced(msg) {
        TxErrorKind::Underpriced
    } else if is_intrinsic_gas_too_low(msg) {
        TxErrorKind::IntrinsicGasTooLow
    } else {
        TxErrorKind::Retryable
    }
}

/// The original mint-loop contract: `"fatal"` (never retry) vs `"retryable"`.
/// Fatal iff funds or a known unrecoverable contract error.
pub fn classify_mint_error(msg: &str) -> &'static str {
    match classify(msg) {
        TxErrorKind::InsufficientFunds | TxErrorKind::FatalContract => "fatal",
        _ => "retryable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insufficient_funds_variants() {
        for s in [
            "insufficient funds for gas * price + value",
            "insufficient funds for transfer",
            "insufficient balance for transfer",
            "err: OutOfFunds",
            "sender doesn't have enough funds to send tx. The upfront cost exceeds balance",
            "overshot the sender account's balance",
        ] {
            assert_eq!(classify(s), TxErrorKind::InsufficientFunds, "{s}");
            assert_eq!(classify_mint_error(s), "fatal", "{s}");
        }
    }

    #[test]
    fn fatal_contract_variants() {
        for s in [
            "execution reverted: InvalidProof",
            "IncorrectPayment()",
            "MintQuantityExceedsMaxSupply",
            "MintQuantityExceedsMaxMintedPerWallet",
            "SignatureAlreadyUsed()",
            "PayerNotAllowed",
        ] {
            assert_eq!(classify(s), TxErrorKind::FatalContract, "{s}");
            assert_eq!(classify_mint_error(s), "fatal", "{s}");
        }
    }

    #[test]
    fn generic_revert_is_retryable() {
        for s in [
            "execution reverted",
            "execution reverted: unknown reason",
            "RPC eth_estimateGas error: {\"message\":\"execution reverted\"}",
            "",
            "timeout",
        ] {
            assert_eq!(classify(s), TxErrorKind::Retryable, "{s}");
            assert_eq!(classify_mint_error(s), "retryable", "{s}");
        }
    }

    #[test]
    fn already_known_variants() {
        for s in [
            "already known",
            "ALREADY KNOWN",
            "txpool: already known transaction",
            "known transaction: 0xabc",
            "AlreadyKnown",
        ] {
            assert!(is_already_known(s), "{s}");
            assert_eq!(classify(s), TxErrorKind::AlreadyKnown, "{s}");
        }
    }

    #[test]
    fn nonce_too_low_variants() {
        for s in [
            "nonce too low",
            "Nonce is too low",
            "err code -32000: nonce_too_low",
            "OldNonce",
        ] {
            assert!(is_nonce_too_low(s), "{s}");
            assert_eq!(classify(s), TxErrorKind::NonceTooLow, "{s}");
        }
    }

    #[test]
    fn underpriced_variants() {
        for s in [
            "replacement transaction underpriced",
            "transaction underpriced",
            "fee too low",
            "max fee per gas less than block base fee",
        ] {
            assert!(is_underpriced(s), "{s}");
            assert_eq!(classify(s), TxErrorKind::Underpriced, "{s}");
        }
    }

    #[test]
    fn intrinsic_gas_variants() {
        for s in ["intrinsic gas too low", "gas limit too low"] {
            assert!(is_intrinsic_gas_too_low(s), "{s}");
            assert_eq!(classify(s), TxErrorKind::IntrinsicGasTooLow, "{s}");
        }
        // Bare "gas too low" no longer forces intrinsic (audit L10): a fee-ish
        // phrase must not be misclassified as an intrinsic-gas (gas-limit) error.
        assert!(!is_intrinsic_gas_too_low(
            "max priority fee per gas too low"
        ));
    }

    #[test]
    fn priority_funds_beats_revert() {
        // A funds error that also mentions a revert stays fatal.
        let s = "execution reverted; insufficient funds for gas";
        assert_eq!(classify(s), TxErrorKind::InsufficientFunds);
    }

    #[test]
    fn json_rpc_code_extraction() {
        assert_eq!(
            json_rpc_code("RPC error: {\"code\":-32000,\"message\":\"nonce too low\"}"),
            Some(-32000)
        );
        assert_eq!(json_rpc_code("weird code: 3 here"), Some(3));
        assert_eq!(json_rpc_code("no code present"), None);
    }
}
