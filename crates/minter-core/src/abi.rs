use alloy_primitives::{Bytes, U256, keccak256};
use anyhow::{Context, Result, bail};

/// Well-known mint/claim selectors (verified against keccak first-4).
pub const KNOWN_MINT_SELECTORS: &[(&[u8], &str)] = &[
    (b"\x40\xc1\x0f\x19", "mint(address,uint256)"),
    (b"\xa0\x71\x2d\x68", "mint(uint256)"),
    (b"\x6a\x62\x78\x42", "mint(address)"),
    (b"\x12\x49\xc5\x8b", "mint()"),
    (b"\x37\x96\x07\xf5", "claim(uint256)"),
    (b"\x4e\x71\xd9\x2d", "claim()"),
    (b"\x2d\xb1\x15\x44", "publicMint(uint256)"),
    (b"\x12\x04\xfe\x0c", "claimTo(address,uint256)"),
    (b"\xa2\x62\xf5\xf8", "claimTo(address)"),
    (b"\xef\xef\x39\xa1", "purchase(uint256)"),
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
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let resp = match client.get(&url).send().await {
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
                // Drop spam / phishing decoys that share selectors with mint*
                .filter(|s| !s.contains("watch_tg") && !s.contains("invmru"))
                .collect()
        })
        .unwrap_or_default()
}

/// Prefer mint/claim/buy style signatures when ranking discover results.
pub fn is_mint_like_signature(sig: &str) -> bool {
    let l = sig.to_ascii_lowercase();
    l.contains("mint")
        || l.contains("claim")
        || l.contains("purchase")
        || l.contains("buy")
        || l.starts_with("public")
        || l.contains("allowlist")
        || l.contains("whitelist")
}

/// EIP-1167 minimal proxy: `363d3d373d3d3d363d73 <impl20> 5af43d82803e903d91602b57fd5bf3`
pub fn parse_eip1167_implementation(code: &[u8]) -> Option<alloy_primitives::Address> {
    const PREFIX: &[u8] = &[0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
    const SUFFIX: &[u8] = &[
        0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b, 0xf3,
    ];
    if code.len() != PREFIX.len() + 20 + SUFFIX.len() {
        return None;
    }
    if !code.starts_with(PREFIX) || !code.ends_with(SUFFIX) {
        return None;
    }
    let mut raw = [0u8; 20];
    raw.copy_from_slice(&code[PREFIX.len()..PREFIX.len() + 20]);
    Some(alloy_primitives::Address::from(raw))
}

/// EIP-1967 implementation storage slot.
pub const EIP1967_IMPLEMENTATION_SLOT: [u8; 32] = [
    0x36, 0x08, 0x94, 0xa1, 0x3b, 0xa1, 0xa3, 0x21, 0x06, 0x67, 0xc8, 0x28, 0x49, 0x2d, 0xb9, 0x8d,
    0xca, 0x3e, 0x20, 0x76, 0xcc, 0x37, 0x35, 0xa9, 0x20, 0xa3, 0xca, 0x50, 0x5d, 0x38, 0x2b, 0xbc,
];

#[cfg(test)]
mod proxy_parse_tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn eip1167_extracts_impl() {
        // Real Robinhood clone-proxy pattern (45 bytes runtime)
        let impl_addr = address!("73afda8000a14cee4e681cac50e668da81b670a8");
        let mut code = vec![0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
        code.extend_from_slice(impl_addr.as_slice());
        code.extend_from_slice(&[
            0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b,
            0xf3,
        ]);
        assert_eq!(parse_eip1167_implementation(&code), Some(impl_addr));
    }

    #[test]
    fn eip1167_rejects_full_contract() {
        assert!(parse_eip1167_implementation(&[0x60, 0x80, 0x60, 0x40]).is_none());
    }
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
    // Arrays with a static element type: dynamic `T[]` (Phase A) and fixed
    // `T[n]` (Phase B). Dynamic-element arrays (bytes[]/string[]) are not.
    if let Some((elem, _fixed)) = parse_array_type(ty) {
        return is_static_word_type(&elem);
    }
    if ty == "string" || ty == "bytes" {
        return true;
    }
    is_static_word_type(ty)
}

/// True for ABI static types that occupy exactly one 32-byte word:
/// `address`, `bool`, `uint<M>`, `int<M>`, `bytes<N>` (1..=32). Not `bytes` /
/// `string` (dynamic) and not arrays.
fn is_static_word_type(ty: &str) -> bool {
    let ty = ty.trim();
    if ty == "bool" || ty == "address" {
        return true;
    }
    if let Some(n) = ty.strip_prefix("bytes") {
        if n.is_empty() {
            return false; // bare `bytes` is dynamic
        }
        return n.parse::<usize>().map(|s| (1..=32).contains(&s)).unwrap_or(false);
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

/// If `ty` is an array type, return `(element_type, fixed_len)` where
/// `fixed_len` is `Some(n)` for `T[n]` and `None` for a dynamic `T[]`.
/// Non-array types return `None`.
pub fn parse_array_type(ty: &str) -> Option<(String, Option<usize>)> {
    let ty = ty.trim();
    if !ty.ends_with(']') {
        return None;
    }
    let open = ty.rfind('[')?;
    let elem = ty[..open].trim().to_string();
    if elem.is_empty() {
        return None;
    }
    let inner = ty[open + 1..ty.len() - 1].trim();
    if inner.is_empty() {
        Some((elem, None)) // T[]
    } else {
        // T[n] — only a valid array if the inside is a number.
        inner.parse::<usize>().ok().map(|n| (elem, Some(n)))
    }
}

/// Split an array literal into element strings. Accepts `"[a,b,c]"`, `"a,b,c"`,
/// or a single value; surrounding brackets and whitespace are stripped.
pub fn split_array_values(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let s = s
        .strip_prefix('[')
        .map(|inner| inner.strip_suffix(']').unwrap_or(inner))
        .unwrap_or(s)
        .trim();
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Encode a dynamic array of static-word elements (`T[]`): a uint256 length
/// followed by one 32-byte word per element. Returns the tail payload (the
/// head offset is written by [`build_calldata`]).
pub fn encode_static_array_dynamic(elem_ty: &str, values: &[String]) -> Result<Vec<u8>> {
    if !is_static_word_type(elem_ty) {
        bail!(
            "array element type {} is not a supported static type",
            elem_ty
        );
    }
    let mut out = Vec::with_capacity(32 + values.len() * 32);
    out.extend_from_slice(&word_from_u256(U256::from(values.len())));
    for v in values {
        out.extend_from_slice(&encode_static_parameter(elem_ty, v)?);
    }
    Ok(out)
}

/// Encode a fixed-size array of static-word elements (`T[n]`): exactly `n`
/// 32-byte words, inline, **no** length prefix. A fixed static array is itself
/// a static type, so this goes directly in the head.
pub fn encode_static_array_fixed(elem_ty: &str, values: &[String], n: usize) -> Result<Vec<u8>> {
    if !is_static_word_type(elem_ty) {
        bail!(
            "array element type {} is not a supported static type",
            elem_ty
        );
    }
    if values.len() != n {
        bail!(
            "fixed array {}[{}] expects {} element(s), got {}",
            elem_ty,
            n,
            n,
            values.len()
        );
    }
    let mut out = Vec::with_capacity(n * 32);
    for v in values {
        out.extend_from_slice(&encode_static_parameter(elem_ty, v)?);
    }
    Ok(out)
}

/// Bytes this type contributes to the ABI **head**: dynamic types write a single
/// 32-byte offset word; a fixed static array `T[n]` writes `n` words inline;
/// every other static type writes one word.
fn head_word_bytes(ty: &str) -> usize {
    if is_dynamic_type(ty) {
        return 32;
    }
    if let Some((elem, Some(n))) = parse_array_type(ty) {
        if is_static_word_type(&elem) {
            return n * 32;
        }
    }
    32
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

/// A precise error for a type we can parse but not (yet) encode, so the
/// operator sees *why* (fixed array / dynamic-element array / tuple).
fn unsupported_type_error(ty: &str) -> anyhow::Error {
    let ty = ty.trim();
    if let Some((elem, _fixed)) = parse_array_type(ty) {
        if !is_static_word_type(&elem) {
            return anyhow::anyhow!("dynamic-element arrays ({ty}) not yet supported");
        }
    }
    if ty.starts_with('(') {
        return anyhow::anyhow!("tuples ({ty}) not yet supported");
    }
    anyhow::anyhow!("Unsupported type: {ty}")
}

/// Encode parameter for legacy callers; dynamic types return tail-only payload
/// (length+data). Prefer [`build_calldata`] for full ABI layout.
pub fn encode_parameter(ty: &str, val: &str) -> Result<Vec<u8>> {
    let ty = ty.trim();
    if !is_supported_type(ty) {
        return Err(unsupported_type_error(ty));
    }
    if let Some((elem, fixed)) = parse_array_type(ty) {
        let values = split_array_values(val);
        return match fixed {
            Some(n) => encode_static_array_fixed(&elem, &values, n),
            None => encode_static_array_dynamic(&elem, &values),
        };
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
            return Err(unsupported_type_error(ty));
        }
    }

    let selector = function_selector(signature);
    if types.is_empty() {
        return Ok(Bytes::from(selector.to_vec()));
    }

    let head_size: usize = types.iter().map(|t| head_word_bytes(t)).sum();
    let mut head = Vec::with_capacity(head_size);
    let mut tail = Vec::new();

    for (ty, val) in types.iter().zip(param_values.iter()) {
        if is_dynamic_type(ty) {
            let offset = head_size + tail.len();
            head.extend_from_slice(&word_offset(offset));
            let dynamic = if let Some((elem, None)) = parse_array_type(ty) {
                encode_static_array_dynamic(&elem, &split_array_values(val))?
            } else {
                match ty.as_str() {
                    "string" => encode_string(val)?,
                    "bytes" => encode_bytes_dynamic(val)?,
                    other => bail!("internal: unhandled dynamic type {}", other),
                }
            };
            tail.extend_from_slice(&dynamic);
        } else if let Some((elem, Some(n))) = parse_array_type(ty) {
            // Fixed static array `T[n]`: n words inline in the head.
            head.extend_from_slice(&encode_static_array_fixed(&elem, &split_array_values(val), n)?);
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

    /// Every hardcoded selector must equal keccak256(signature)[..4].
    /// (An earlier revision shipped wrong bytes for claim/publicMint/claimTo,
    /// which mislabeled discovered functions and skipped real ones.)
    #[test]
    fn known_mint_selectors_match_keccak() {
        for (sel, sig) in KNOWN_MINT_SELECTORS {
            let expected = function_selector(sig);
            assert_eq!(
                *sel,
                expected.as_slice(),
                "selector table entry for {sig} is wrong: table={} keccak={}",
                hex::encode(sel),
                hex::encode(expected)
            );
        }
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
    fn parse_array_type_forms() {
        assert_eq!(parse_array_type("uint256[]"), Some(("uint256".into(), None)));
        assert_eq!(parse_array_type("address[]"), Some(("address".into(), None)));
        assert_eq!(parse_array_type("bytes32[]"), Some(("bytes32".into(), None)));
        assert_eq!(parse_array_type("uint256[3]"), Some(("uint256".into(), Some(3))));
        assert_eq!(parse_array_type("uint256"), None);
        assert_eq!(parse_array_type("bytes"), None);
    }

    #[test]
    fn split_array_values_forms() {
        assert_eq!(split_array_values("[1,2,3]"), vec!["1", "2", "3"]);
        assert_eq!(split_array_values("1, 2 ,3"), vec!["1", "2", "3"]);
        assert_eq!(split_array_values("[]"), Vec::<String>::new());
        assert_eq!(split_array_values(" 0xabc "), vec!["0xabc"]);
    }

    #[test]
    fn encode_dynamic_address_array() {
        let a1 = "0x1111111111111111111111111111111111111111";
        let a2 = "0x2222222222222222222222222222222222222222";
        let cd = build_calldata("airdrop(address[])", &[format!("[{a1},{a2}]")]).unwrap();
        let body = &cd[4..]; // skip selector
        assert_eq!(U256::from_be_slice(&body[0..32]), U256::from(32)); // offset
        assert_eq!(U256::from_be_slice(&body[32..64]), U256::from(2)); // length
        assert_eq!(&body[64..96], encode_address(a1).unwrap().as_slice());
        assert_eq!(&body[96..128], encode_address(a2).unwrap().as_slice());
        assert_eq!(body.len(), 32 * 4);
    }

    #[test]
    fn encode_bytes32_array_merkle_proof() {
        let proof = format!("[0x{},0x{}]", "11".repeat(32), "22".repeat(32));
        let cd = build_calldata("claim(bytes32[])", &[proof]).unwrap();
        let body = &cd[4..];
        assert_eq!(U256::from_be_slice(&body[0..32]), U256::from(32));
        assert_eq!(U256::from_be_slice(&body[32..64]), U256::from(2));
        assert_eq!(&body[64..96], &[0x11u8; 32]);
        assert_eq!(&body[96..128], &[0x22u8; 32]);
    }

    #[test]
    fn encode_mixed_uint_then_array() {
        // claim(uint256, uint256[]) → head = [uint][offset=0x40], then tail.
        let cd = build_calldata("claim(uint256,uint256[])", &["5".into(), "[1,2,3]".into()]).unwrap();
        let b = &cd[4..];
        assert_eq!(U256::from_be_slice(&b[0..32]), U256::from(5)); // arg 0
        assert_eq!(U256::from_be_slice(&b[32..64]), U256::from(64)); // offset to tail
        assert_eq!(U256::from_be_slice(&b[64..96]), U256::from(3)); // length
        assert_eq!(U256::from_be_slice(&b[96..128]), U256::from(1));
        assert_eq!(U256::from_be_slice(&b[128..160]), U256::from(2));
        assert_eq!(U256::from_be_slice(&b[160..192]), U256::from(3));
    }

    #[test]
    fn empty_array_encodes_length_zero() {
        let cd = build_calldata("claim(bytes32[])", &["[]".into()]).unwrap();
        let b = &cd[4..];
        assert_eq!(U256::from_be_slice(&b[0..32]), U256::from(32));
        assert_eq!(U256::from_be_slice(&b[32..64]), U256::from(0));
        assert_eq!(b.len(), 64);
    }

    #[test]
    fn bracketed_and_bare_forms_are_equivalent() {
        let a = build_calldata("f(uint256[])", &["1,2,3".into()]).unwrap();
        let b = build_calldata("f(uint256[])", &["[1,2,3]".into()]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn encode_fixed_static_arrays() {
        // uint256[3] is a static type: 3 inline words in the head, no length.
        let cd = build_calldata("f(uint256[3])", &["[1,2,3]".into()]).unwrap();
        let b = &cd[4..];
        assert_eq!(b.len(), 3 * 32);
        assert_eq!(U256::from_be_slice(&b[0..32]), U256::from(1));
        assert_eq!(U256::from_be_slice(&b[32..64]), U256::from(2));
        assert_eq!(U256::from_be_slice(&b[64..96]), U256::from(3));

        // Mixed: f(uint256[2], uint256) — head is 2+1 = 3 words, all inline.
        let cd = build_calldata("g(uint256[2],uint256)", &["[7,8]".into(), "9".into()]).unwrap();
        let b = &cd[4..];
        assert_eq!(b.len(), 3 * 32);
        assert_eq!(U256::from_be_slice(&b[0..32]), U256::from(7));
        assert_eq!(U256::from_be_slice(&b[32..64]), U256::from(8));
        assert_eq!(U256::from_be_slice(&b[64..96]), U256::from(9));
    }

    #[test]
    fn fixed_then_dynamic_offset_is_correct() {
        // f(address[2], uint256[]) — head = 2 (fixed) + 1 (offset) = 3 words = 96B.
        // The dynamic array's offset must therefore be 96 (0x60).
        let a1 = "0x1111111111111111111111111111111111111111";
        let a2 = "0x2222222222222222222222222222222222222222";
        let cd = build_calldata("f(address[2],uint256[])", &[format!("[{a1},{a2}]"), "[5]".into()]).unwrap();
        let b = &cd[4..];
        assert_eq!(&b[0..32], encode_address(a1).unwrap().as_slice());
        assert_eq!(&b[32..64], encode_address(a2).unwrap().as_slice());
        assert_eq!(U256::from_be_slice(&b[64..96]), U256::from(96)); // offset
        assert_eq!(U256::from_be_slice(&b[96..128]), U256::from(1)); // len
        assert_eq!(U256::from_be_slice(&b[128..160]), U256::from(5)); // elem
    }

    #[test]
    fn fixed_array_wrong_count_errors() {
        let e = build_calldata("f(uint256[3])", &["[1,2]".into()]).unwrap_err().to_string();
        assert!(e.contains("expects 3"), "{e}");
    }

    #[test]
    fn unsupported_array_and_tuple_forms_have_clear_errors() {
        let dynelem = encode_parameter("bytes[]", "[0x01]").unwrap_err().to_string();
        assert!(dynelem.contains("dynamic-element arrays"), "{dynelem}");
        let tuple = encode_parameter("(address,uint256)", "x").unwrap_err().to_string();
        assert!(tuple.contains("tuples"), "{tuple}");
    }

    #[test]
    fn pad32_right_helper() {
        assert_eq!(pad32_right(&[1, 2]).len(), 32);
        assert_eq!(pad32_right(&[0u8; 32]).len(), 32);
        assert_eq!(pad32_right(&[0u8; 33]).len(), 64);
    }
}
