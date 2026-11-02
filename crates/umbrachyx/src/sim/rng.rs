//! The one source of randomness in a simulation run.
//!
//! SplitMix64, written out here rather than taken from a crate: a seed must
//! replay the same run for as long as the seed is kept in a bug report, and a
//! dependency upgrade that changed the stream would silently break that.

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
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..bound`. The slight modulo bias does not matter for
    /// choosing faults. `bound` must not be zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// True `percent` times out of 100.
    pub fn percent(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }

    pub fn ascii(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| char::from(b'a' + self.below(26) as u8))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_is_fixed_for_a_seed() {
        // Pinned so a change to the generator cannot pass unnoticed: every
        // recorded failing seed would stop replaying.
        let mut rng = Rng::new(0);

        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }
}
