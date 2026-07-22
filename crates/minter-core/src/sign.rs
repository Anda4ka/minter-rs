use alloy::consensus::SignableTransaction;
use alloy::eips::Encodable2718;
use alloy::network::TxSignerSync;
use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct BuiltTx {
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub gas_limit: u64,
    pub max_fee: U256,
    pub max_priority_fee: U256,
}

pub fn sign_transaction(
    signer: &alloy::signers::local::LocalSigner<k256::ecdsa::SigningKey>,
    tx: &BuiltTx,
) -> Result<(Bytes, B256)> {
    let mut eip1559 = alloy::consensus::TxEip1559 {
        chain_id: tx.chain_id,
        nonce: tx.nonce,
        gas_limit: tx.gas_limit,
        // Saturate instead of `to::<u128>()`, which panics on overflow. Fees never
        // legitimately exceed u128, but a bad RPC fee value must not crash signing.
        max_fee_per_gas: tx.max_fee.saturating_to::<u128>(),
        max_priority_fee_per_gas: tx.max_priority_fee.saturating_to::<u128>(),
        to: alloy::primitives::TxKind::Call(tx.to),
        value: tx.value,
        input: tx.data.clone().into(),
        access_list: Default::default(),
    };

    let sig = signer
        .sign_transaction_sync(&mut eip1559)
        .context("failed to sign transaction")?;
    let signed = eip1559.into_signed(sig);
    let envelope: alloy::consensus::TxEnvelope = signed.into();
    let hash = *envelope.hash();
    Ok((envelope.encoded_2718().into(), hash))
}

pub fn shorten_hash(hash: &B256) -> String {
    let hex = hex::encode(hash.as_slice());
    if hex.len() < 18 {
        return hex;
    }
    format!("{}...{}", &hex[..10], &hex[hex.len() - 8..])
}

pub fn shorten_address(addr: &Address) -> String {
    let hex = hex::encode(addr.as_slice());
    format!("{}...{}", &hex[..6], &hex[hex.len() - 4..])
}
