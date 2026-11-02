//! Node hashes, which name content, and key hashes, which place keys.

pub type Digest = [u8; 32];

/// BLAKE3 over a node's fields, each prefixed with its length so that no two
/// different nodes encode to the same bytes.
pub(crate) struct NodeHasher(blake3::Hasher);

impl NodeHasher {
    pub fn new(tag: u8) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[tag]);
        Self(hasher)
    }

    pub fn field(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u32).to_le_bytes());
        self.0.update(bytes);
    }

    pub fn finish(&self) -> Digest {
        *self.0.finalize().as_bytes()
    }
}

/// A fast 64-bit hash of a key, used to place it: FNV-1a, then the SplitMix64
/// finalizer to spread the bits. It is not keyed, so keys chosen by an
/// attacker could unbalance a tree; a real one would need a keyed hash.
pub(crate) fn key_hash(key: &[u8]) -> u64 {
    let fnv = key.iter().fold(0xCBF2_9CE4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3)
    });
    mix(fnv)
}

/// The SplitMix64 finalizer. Distinct inputs give unrelated outputs, so
/// `mix(hash + salt)` acts as an independent hash per salt.
pub(crate) fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
