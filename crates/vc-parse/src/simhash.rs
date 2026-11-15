//! 64-bit simhash (Charikar) of a text, for near-duplicate detection.
//!
//! Features are lower-cased alphanumeric tokens weighted by frequency; each is hashed with
//! FNV-1a 64 followed by the splitmix64 finaliser. The hash is **stored**, so this function
//! must never change; `simhash_is_stable` pins it.

use std::collections::HashMap;

/// The simhash of `text` (0 for a text without tokens).
pub fn simhash64(text: &str) -> u64 {
    let mut tf: HashMap<String, i64> = HashMap::new();
    for tok in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        *tf.entry(tok.to_lowercase()).or_default() += 1;
    }
    if tf.is_empty() {
        return 0;
    }
    let mut v = [0i64; 64];
    for (tok, w) in &tf {
        let h = feature_hash(tok.as_bytes());
        for (bit, acc) in v.iter_mut().enumerate() {
            if h >> bit & 1 == 1 {
                *acc += w;
            } else {
                *acc -= w;
            }
        }
    }
    v.iter()
        .enumerate()
        .filter(|(_, acc)| **acc > 0)
        .fold(0u64, |h, (bit, _)| h | 1 << bit)
}

/// The number of differing bits.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn feature_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // splitmix64 finaliser: spreads FNV's weak high bits.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}
