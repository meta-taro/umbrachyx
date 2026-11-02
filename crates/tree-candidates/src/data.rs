//! Synthetic data: a game's item inventory, one record per player and slot.
//! No real service's data is used, so anyone can rerun the numbers.

use crate::Bytes;

pub const SLOTS_PER_PLAYER: u64 = 100;
const VALUE_LEN: usize = 32;

/// The key of record `index`. Records use even slot numbers only, so
/// [`new_key`] can add records in the middle of the key space, not only at
/// its end.
pub fn key(index: u64) -> Bytes {
    slot_key(index / SLOTS_PER_PLAYER, 2 * (index % SLOTS_PER_PLAYER))
}

/// A key not in [`dataset`]: an odd slot of an existing player.
pub fn new_key(rng: &mut Rng, records: u64) -> Bytes {
    let index = rng.below(records);
    slot_key(index / SLOTS_PER_PLAYER, 2 * (index % SLOTS_PER_PLAYER) + 1)
}

fn slot_key(player: u64, slot: u64) -> Bytes {
    format!("player/{player:08}/slot/{slot:03}")
        .into_bytes()
        .into()
}

/// An item record: id, count and acquired-at, padded to a fixed length.
pub fn value(rng: &mut Rng) -> Bytes {
    rng.bytes(VALUE_LEN).into()
}

/// `records` entries sorted by key.
pub fn dataset(records: u64, rng: &mut Rng) -> Vec<(Bytes, Bytes)> {
    (0..records).map(|index| (key(index), value(rng))).collect()
}

/// SplitMix64. The same generator as the database's simulation tests, kept
/// separate so this crate does not depend on test-only code there.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        crate::hash::mix(self.state)
    }

    /// A number in `0..bound`. `bound` must not be zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sort_in_record_order() {
        let keys: Vec<Bytes> = (0..250).map(key).collect();

        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn new_keys_are_never_in_the_dataset() {
        let mut rng = Rng::new(1);
        let keys: Vec<Bytes> = (0..1_000).map(key).collect();

        for _ in 0..1_000 {
            assert!(keys.binary_search(&new_key(&mut rng, 1_000)).is_err());
        }
    }

    #[test]
    fn stream_matches_splitmix64() {
        assert_eq!(Rng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }
}
