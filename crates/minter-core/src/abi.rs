use alloy_primitives::{Bytes, U256, keccak256};
use anyhow::{Context, Result, bail};

pub const KNOWN_MINT_SELECTORS: &[(&[u8], &str)] = &[
    (b"\x40\xc1\x0f\x19", "mint(address,uint256)"),
    (b"\xa0\x71\x2d\x68", "mint(uint256)"),
    (b"\x6a\x62\x78\x42", "mint(address)"),
    (b"\x12\x49\xc5\x8b", "mint()"),
    (b"\xb6\xb0\xe5\x1c", "claim(uint256)"),
    (b"\x4e\x71\xd1\xad", "claim()"),
    (b"\xd0\x95\xb5\xde", "publicMint(uint256)"),
    (b"\xc3\xfb\xdd\x0f", "claimTo(address,uint256)"),
    (b"\x8e\xf6\x45\x98", "claimTo(address)"),
];

pub fn extract_selectors(bytecode: &[u8]) -> Vec<[u8; 4]> {
    let mut selectors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i + 1 < bytecode.len() {
        if bytecode[i] == 0x63 && i + 5 <= bytecode.len() {
            let sel: [u8; 4] = bytecode[i + 1..i + 5].try_into().unwrap();
            if sel != [0u8; 4] && seen.insert(sel) {
                selectors.push(sel);
            }
            i += 5;
        } else {
            i += 1;
        }
    }
    selectors
}

pub async fn lookup_4byte(selector: &[u8; 4]) -> Vec<String> {
    let hex_sel = format!("0x{}", hex::encode(selector));
    let url = format!(
        "https://www.4byte.directory/api/v1/signatures/?hex_signature={}",
        hex_sel
    );
    let client = reqwest::Client::new();
    let resp = match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let data: serde_json::Value = match resp.json().await {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    data.get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    r.get("text_signature")
                        .and_then(|s| s.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_function_signature(sig: &str) -> Result<(String, Vec<String>)> {
    let paren = sig.find('(').context("invalid signature: no (")?;
    let name = sig[..paren].to_string();
    let inner = &sig[paren + 1..sig.rfind(')').context("invalid signature: no )")?];
    if inner.trim().is_empty() {
        return Ok((name, vec![]));
    }
    let mut types = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for ch in inner.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                types.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        types.push(current.trim().to_string());
    }
    Ok((name, types))
}

pub fn function_selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    let mut sel = [0u8; 4];
    sel.copy_from_slice(&hash[..4]);
    sel
}

fn pad32_right(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    let pad = (32 - (out.len() % 32)) % 32;
    out.extend(std::iter::repeat(0u8).take(pad));
    out
}

fn word_from_u256(v: U256) -> [u8; 32] {
    v.to_be_bytes::<32>()
}

fn word_offset(offset: usize) -> [u8; 32] {
    word_from_u256(U256::from(offset))
}

/// True for ABI dynamic types we support (string, bytes) and unsupported
/// dynamic forms (arrays/tuples) that must be rejected rather than mis-encoded.
pub fn is_dynamic_type(ty: &str) -> bool {
    let ty = ty.trim();
    if ty == "string" || ty == "bytes" {
        return true;
    }
    if ty.ends_with("[]") {
        return true;
    }
    // tuple components e.g. (address,uint256)
    if ty.starts_with('(') {
        return true;
    }
    false
}

fn is_supported_type(ty: &str) -> bool {
    let ty = ty.trim();
    if ty == "string" || ty == "bytes" || ty == "bool" || ty == "address" {
        return true;
    }
    if let Some(n) = ty.strip_prefix("bytes") {
        if n.is_empty() {
            return true; // "bytes" already handled
        }
        if let Ok(size) = n.parse::<usize>() {
            return (1..=32).contains(&size);
        }
        return false;
    }
    if let Some(rest) = ty.strip_prefix("uint") {
        return rest.is_empty()
            || rest
                .parse::<u32>()
                .map(|b| b > 0 && b <= 256 && b % 8 == 0)
                .unwrap_or(false);
    }
    if let Some(rest) = ty.strip_prefix("int") {
        return rest.is_empty()
            || rest
                .parse::<u32>()
                .map(|b| b > 0 && b <= 256 && b % 8 == 0)
                .unwrap_or(false);
    }
    false
}

/// Parse unsigned integer from decimal or 0x-hex into U256 word.
pub fn encode_uint256(val: &str) -> Result<[u8; 32]> {
    let val = val.trim();
    if val.is_empty() {
        bail!("empty uint value");
    }
    let u = if let Some(hex) = val
        .strip_prefix("0x")
        .or_else(|| val.strip_prefix("0X"))
    {
        if hex.is_empty() {
            bail!("empty hex uint");
        }
        U256::from_str_radix(hex, 16).context("invalid hex uint")?
    } else {
        if val.starts_with('-') {
            bail!("negative uint not allowed");
        }
        U256::from_str_radix(val, 10).context("invalid decimal uint")?
    };
    Ok(word_from_u256(u))
}

pub fn encode_address(val: &str) -> Result<[u8; 32]> {
    let addr = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X")).unwrap_or(val);
    let decoded = hex::decode(addr).context("invalid address hex")?;
    if decoded.len() != 20 {
        bail!(
            "invalid address length: expected 20 bytes, got {}",
            decoded.len()
        );
    }
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&decoded);
    Ok(out)
}

pub fn encode_bool(val: &str) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    let v = val.trim();
    if v.eq_ignore_ascii_case("true") || v == "1" {
        out[31] = 1;
    } else if v.eq_ignore_ascii_case("false") || v == "0" {
        // zero
    } else {
        bail!("invalid bool: {}", val);
    }
    Ok(out)
}

/// Fixed-size `bytesN` (1..=32): left-aligned in a 32-byte word.
pub fn encode_bytes_fixed(val: &str, n: usize) -> Result<[u8; 32]> {
    if !(1..=32).contains(&n) {
        bail!("bytesN size must be 1..=32, got {}", n);
    }
    let hex_str = val
        .strip_prefix("0x")
        .or_else(|| val.strip_prefix("0X"))
        .unwrap_or(val);
    let decoded = hex::decode(hex_str).context("invalid bytes hex")?;
    if decoded.len() != n {
        bail!(
            "bytes{} expects {} bytes, got {}",
            n,
            n,
            decoded.len()
        );
    }
    let mut out = [0u8; 32];
    out[..n].copy_from_slice(&decoded);
    Ok(out)
}

/// Dynamic `bytes` / `string` tail: uint256 length + data + right-pad to 32.
pub fn encode_dynamic_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + data.len() + 32);
    out.extend_from_slice(&word_from_u256(U256::from(data.len())));
    out.extend_from_slice(&pad32_right(data));
    out
}

pub fn encode_string(val: &str) -> Result<Vec<u8>> {
    Ok(encode_dynamic_bytes(val.as_bytes()))
}

pub fn encode_bytes_dynamic(val: &str) -> Result<Vec<u8>> {
    let hex_str = val
        .strip_prefix("0x")
        .or_else(|| val.strip_prefix("0X"))
        .unwrap_or(val);
    if hex_str.is_empty() {
        return Ok(encode_dynamic_bytes(&[]));
    }
    let decoded = hex::decode(hex_str).context("invalid bytes")?;
    Ok(encode_dynamic_bytes(&decoded))
}

/// Encode a single static type into a 32-byte word.
pub fn encode_static_parameter(ty: &str, val: &str) -> Result<[u8; 32]> {
    let ty = ty.trim();
    if ty == "address" {
        return encode_address(val);
    }
    if ty == "bool" {
        return encode_bool(val);
    }
    if let Some(n_str) = ty.strip_prefix("bytes") {
        if !n_str.is_empty() {
            let n: usize = n_str.parse().context("invalid bytesN size")?;
            return encode_bytes_fixed(val, n);
        }
    }
    if ty.starts_with("uint") || ty == "uint" {
        return encode_uint256(val);
    }
    if ty.starts_with("int") || ty == "int" {
        // Positive ints only via same path; reject negatives for simplicity.
        let v = val.trim();
        if v.starts_with('-') {
            bail!("negative int encoding not supported: {}", val);
        }
        return encode_uint256(val);
    }
    bail!("not a static type: {}", ty);
}

/// Encode parameter for legacy callers; dynamic types return tail-only payload
/// (length+data). Prefer [`build_calldata`] for full ABI layout.
pub fn encode_parameter(ty: &str, val: &str) -> Result<Vec<u8>> {
    let ty = ty.trim();
    if !is_supported_type(ty) {
        if ty.ends_with("[]") || ty.starts_with('(') {
            bail!(
                "Unsupported type: {} (arrays/tuples not implemented)",
                ty
            );
        }
        bail!("Unsupported type: {}", ty);
    }
    if ty == "string" {
        return encode_string(val);
    }
    if ty == "bytes" {
        return encode_bytes_dynamic(val);
    }
    Ok(encode_static_parameter(ty, val)?.to_vec())
}

/// Build full ABI calldata: selector || head || tail with correct dynamic offsets.
pub fn build_calldata(signature: &str, param_values: &[String]) -> Result<Bytes> {
    let (_name, types) = parse_function_signature(signature)?;
    if types.len() != param_values.len() {
        bail!(
            "parameter count mismatch: signature expects {}, got {}",
            types.len(),
            param_values.len()
        );
    }

    for ty in &types {
        if !is_supported_type(ty) {
            if ty.ends_with("[]") || ty.contains('[') || ty.starts_with('(') {
                bail!(
                    "Unsupported type: {} (arrays/tuples not implemented)",
                    ty
                );
            }
            bail!("Unsupported type: {}", ty);
        }
    }

    let selector = function_selector(signature);
    if types.is_empty() {
        return Ok(Bytes::from(selector.to_vec()));
    }

    let head_size = types.len() * 32;
    let mut head = Vec::with_capacity(head_size);
    let mut tail = Vec::new();

    for (ty, val) in types.iter().zip(param_values.iter()) {
        if is_dynamic_type(ty) {
            let offset = head_size + tail.len();
            head.extend_from_slice(&word_offset(offset));
            let dynamic = match ty.as_str() {
                "string" => encode_string(val)?,
                "bytes" => encode_bytes_dynamic(val)?,
                other => bail!("internal: unhandled dynamic type {}", other),
            };
            tail.extend_from_slice(&dynamic);
        } else {
            let word = encode_static_parameter(ty, val)?;
            head.extend_from_slice(&word);
        }
    }

    let mut data = Vec::with_capacity(4 + head.len() + tail.len());
    data.extend_from_slice(&selector);
    data.extend_from_slice(&head);
    data.extend_from_slice(&tail);
    Ok(Bytes::from(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_known() {
        let sel = function_selector("transfer(address,uint256)");
        assert_eq!(sel, [0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn encode_addr_20bytes() {
        let result = encode_address("0x1234567890abcdef1234567890abcdef12345678");
        assert!(result.is_ok());
        let arr = result.unwrap();
        assert_eq!(
            arr[12..],
            [
                0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x90,
                0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78
            ]
        );
    }

    #[test]
    fn encode_addr_too_long() {
        let result = encode_address(
            "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef12345678",
        );
        assert!(result.is_err());
    }

    #[test]
    fn encode_addr_short_rejected() {
        assert!(encode_address("0x1234").is_err());
    }

    #[test]
    fn build_calldata_param_count() {
        let result = build_calldata("mint(address,uint256)", &["0x1234".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn build_calldata_ok() {
        let result = build_calldata(
            "mint(address,uint256)",
            &[
                "0x0000000000000000000000000000000000000000".to_string(),
                "1".to_string(),
            ],
        );
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.len(), 4 + 32 + 32);
    }

    #[test]
    fn parse_sig_simple() {
        let (name, types) = parse_function_signature("mint(uint256)").unwrap();
        assert_eq!(name, "mint");
        assert_eq!(types, vec!["uint256"]);
    }

    #[test]
    fn parse_sig_multi() {
        let (name, types) = parse_function_signature("transfer(address,uint256)").unwrap();
        assert_eq!(name, "transfer");
        assert_eq!(types, vec!["address", "uint256"]);
    }

    #[test]
    fn encode_uint_large_decimal() {
        // 2^128 — beyond old u128 path without fitting as u128 parse of full value as amount in word
        let v = encode_uint256("340282366920938463463374607431768211456").unwrap(); // 2^128
        assert_eq!(v[15], 1); // byte at position for 2^128
        assert_eq!(&v[16..], &[0u8; 16]);
    }

    #[test]
    fn encode_uint_hex() {
        let v = encode_uint256("0xff").unwrap();
        assert_eq!(v[31], 0xff);
    }

    #[test]
    fn dynamic_string_layout() {
        // setName(string) — selector + offset(0x20) + len + "hi" + pad
        let data = build_calldata("setName(string)", &["hi".to_string()]).unwrap();
        assert_eq!(&data[..4], &function_selector("setName(string)"));
        // offset = 32 (0x20)
        assert_eq!(&data[4..36], &word_offset(32));
        // length = 2
        assert_eq!(&data[36..68], &word_from_u256(U256::from(2u64)));
        // "hi" + pad
        assert_eq!(&data[68..70], b"hi");
        assert!(data[70..].iter().all(|&b| b == 0));
        assert_eq!(data.len(), 4 + 32 + 32 + 32); // sel + offset + len + padded data word
    }

    #[test]
    fn dynamic_bytes_empty() {
        let data = build_calldata("setData(bytes)", &["0x".to_string()]).unwrap();
        assert_eq!(&data[4..36], &word_offset(32));
        assert_eq!(&data[36..68], &word_from_u256(U256::ZERO));
        assert_eq!(data.len(), 4 + 32 + 32);
    }

    #[test]
    fn mixed_static_and_dynamic() {
        // foo(uint256,string,address)
        let addr = "0x1111111111111111111111111111111111111111";
        let data = build_calldata(
            "foo(uint256,string,address)",
            &["42".to_string(), "ab".to_string(), addr.to_string()],
        )
        .unwrap();
        // head: 3 * 32
        // word0 = 42
        assert_eq!(&data[4..36], &word_from_u256(U256::from(42u64)));
        // word1 = offset to string = 96 (3*32)
        assert_eq!(&data[36..68], &word_offset(96));
        // word2 = address
        assert_eq!(&data[68..100], &encode_address(addr).unwrap());
        // tail: len=2, "ab"
        assert_eq!(&data[100..132], &word_from_u256(U256::from(2u64)));
        assert_eq!(&data[132..134], b"ab");
    }

    #[test]
    fn transfer_calldata_matches_known_encoding() {
        // transfer(address,uint256) to 0x0000...0001 value 1
        let data = build_calldata(
            "transfer(address,uint256)",
            &[
                "0x0000000000000000000000000000000000000001".to_string(),
                "1".to_string(),
            ],
        )
        .unwrap();
        assert_eq!(&data[..4], &[0xa9, 0x05, 0x9c, 0xbb]);
        let mut expected_to = [0u8; 32];
        expected_to[31] = 1;
        assert_eq!(&data[4..36], &expected_to);
        let mut expected_amt = [0u8; 32];
        expected_amt[31] = 1;
        assert_eq!(&data[36..68], &expected_amt);
    }

    #[test]
    fn bytes4_static() {
        let w = encode_bytes_fixed("0x80ac58cd", 4).unwrap();
        assert_eq!(&w[..4], &[0x80, 0xac, 0x58, 0xcd]);
        assert!(w[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn arrays_rejected() {
        let err = build_calldata("mint(uint256[])", &["1".to_string()]).unwrap_err();
        assert!(err.to_string().contains("not implemented") || err.to_string().contains("Unsupported"));
    }

    #[test]
    fn pad32_right_helper() {
        assert_eq!(pad32_right(&[1, 2]).len(), 32);
        assert_eq!(pad32_right(&[0u8; 32]).len(), 32);
        assert_eq!(pad32_right(&[0u8; 33]).len(), 64);
    }
}
