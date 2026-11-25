//! AES-SIV (RFC 5297) for one header, with what depends only on the key
//! done once.
//!
//! The `aes-siv` crate expands the key of the counter mode inside every
//! call, and starts S2V from a block it works out again each time. For a
//! value of a few tens of bytes that setup is most of the cost: on a
//! laptop, sealing 32 bytes took 185 to 191 ns and opening them 181 to
//! 196, and here 56 to 57 and 72 to 73 (the ignored test
//! `what_sealing_costs` below prints both;
//! `bench/results/2026-11-25-laptop-siv/`).
//!
//! This is the same construction from the same RustCrypto parts (`aes`,
//! `cmac`, `ctr`, `dbl`), and gives the same bytes: the tests seal and
//! open against `aes-siv` itself. It takes exactly one header, which is
//! all the store uses: the key a value is stored under.

use aes::Aes256;
use cipher::consts::U16;
use cipher::{Array, InnerIvInit, KeyInit, StreamCipherCore};
use cmac::Cmac;
use ctr::CtrCore;
use ctr::flavors::Ctr128BE;
use dbl::Dbl;
use digest::{FixedOutputReset, Mac};
use subtle::ConstantTimeEq;

use crate::keys::{KEY_LEN, Key};

/// The length of an AES block, and of the tag in front of a sealed value.
pub(crate) const TAG_LEN: usize = 16;

type Block = Array<u8, U16>;

pub(crate) struct Siv {
    mac: Cmac<Aes256>,
    cipher: Aes256,
    /// The CMAC of a block of zeros, which S2V starts from for every
    /// value.
    start: Block,
}

impl Siv {
    /// A key is two AES-256 keys: the first for the tag, the second for
    /// the counter mode.
    pub(crate) fn new(key: &Key) -> Self {
        let (mac_key, cipher_key) = key.split_at(KEY_LEN / 2);
        let mut mac =
            <Cmac<Aes256> as KeyInit>::new_from_slice(mac_key).expect("half the key is 32 bytes");
        Mac::update(&mut mac, &[0; TAG_LEN]);
        let start = mac.finalize_fixed_reset();
        Self {
            mac,
            cipher: Aes256::new_from_slice(cipher_key).expect("half the key is 32 bytes"),
            start,
        }
    }

    /// The tag in front of `value` sealed for `header`, then `value`
    /// encrypted under it.
    pub(crate) fn seal(&mut self, header: &[u8], value: &[u8]) -> Vec<u8> {
        let tag = self.s2v(header, value);
        let mut sealed = Vec::with_capacity(TAG_LEN + value.len());
        sealed.extend_from_slice(&tag);
        sealed.extend_from_slice(value);
        self.apply_keystream(tag, &mut sealed[TAG_LEN..]);
        sealed
    }

    /// What `sealed` holds, if it was sealed under this key for `header`
    /// and has not changed since.
    pub(crate) fn open(&mut self, header: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let (tag, body) = sealed.split_first_chunk::<TAG_LEN>()?;
        let mut value = body.to_vec();
        self.apply_keystream(Block::from(*tag), &mut value);
        let expected = self.s2v(header, &value);
        // Compared in the same time wherever they differ, so that how
        // long a refusal takes says nothing about the tag expected.
        if bool::from(expected.ct_eq(tag)) {
            return Some(value);
        }
        // Sealed again before it is let go: bytes sealed for another
        // header open to the value, and it is not left in memory in the
        // clear for having been refused.
        self.apply_keystream(Block::from(*tag), &mut value);
        None
    }

    /// The tag of `value` under `header`: S2V of RFC 5297, section 2.4,
    /// for one header.
    fn s2v(&mut self, header: &[u8], value: &[u8]) -> Block {
        let mut state = self.start.dbl();
        Mac::update(&mut self.mac, header);
        xor(&mut state, &self.mac.finalize_fixed_reset());
        match value.len().checked_sub(TAG_LEN) {
            Some(before) => {
                Mac::update(&mut self.mac, &value[..before]);
                xor(&mut state, &value[before..]);
            }
            None => {
                state = state.dbl();
                xor(&mut state, value);
                state[value.len()] ^= 0x80;
            }
        }
        Mac::update(&mut self.mac, &state);
        self.mac.finalize_fixed_reset()
    }

    /// Encrypts or decrypts `bytes` with the counter started from `tag`.
    fn apply_keystream(&self, mut tag: Block, bytes: &mut [u8]) {
        // The top bit of each of the tag's last two 32-bit words is
        // cleared before it starts the counter (RFC 5297, section 2.6).
        tag[8] &= 0x7f;
        tag[12] &= 0x7f;
        CtrCore::<&Aes256, Ctr128BE>::inner_iv_init(&self.cipher, &tag)
            .apply_keystream_partial(bytes.into());
    }
}

fn xor(state: &mut Block, bytes: &[u8]) {
    for (held, byte) in state.iter_mut().zip(bytes) {
        *held ^= byte;
    }
}

#[cfg(test)]
mod tests {
    use aes_siv::siv::Aes256Siv;

    use super::*;

    /// Bytes that differ from run to run of the generator, and are the
    /// same every time the tests run.
    struct Source(u64);

    impl Source {
        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len)
                .map(|_| {
                    self.0 = self
                        .0
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (self.0 >> 56) as u8
                })
                .collect()
        }

        fn key(&mut self) -> Key {
            self.bytes(KEY_LEN).try_into().expect("KEY_LEN bytes")
        }
    }

    /// Every length around a block and around the tag, and some long
    /// ones.
    fn value_lens() -> impl Iterator<Item = usize> {
        (0..=80).chain([127, 128, 129, 255, 256, 257, 4_095, 4_096, 4_097, 70_000])
    }

    const HEADER_LENS: [usize; 9] = [0, 1, 15, 16, 17, 31, 32, 33, 300];

    #[test]
    fn it_seals_to_the_bytes_aes_siv_gives() {
        let mut source = Source(1);
        for _ in 0..3 {
            let key = source.key();
            let mut theirs = Aes256Siv::new_from_slice(&key).expect("a key of 64 bytes");
            let mut ours = Siv::new(&key);
            for len in value_lens() {
                for header_len in HEADER_LENS {
                    let header = source.bytes(header_len);
                    let value = source.bytes(len);

                    let sealed = ours.seal(&header, &value);

                    let expected = theirs.encrypt([&header], &value).expect("seals");
                    assert_eq!(sealed, expected, "value of {len}, header of {header_len}");
                }
            }
        }
    }

    #[test]
    fn it_opens_what_aes_siv_sealed_and_aes_siv_opens_what_it_sealed() {
        let mut source = Source(2);
        let key = source.key();
        let mut theirs = Aes256Siv::new_from_slice(&key).expect("a key of 64 bytes");
        let mut ours = Siv::new(&key);
        for len in value_lens() {
            let header = source.bytes(21);
            let value = source.bytes(len);
            let from_theirs = theirs.encrypt([&header], &value).expect("seals");
            let from_ours = ours.seal(&header, &value);

            assert_eq!(ours.open(&header, &from_theirs), Some(value.clone()));
            assert_eq!(theirs.decrypt([&header], &from_ours).ok(), Some(value));
        }
    }

    #[test]
    fn a_value_changed_in_any_byte_is_refused() {
        let mut source = Source(3);
        let mut ours = Siv::new(&source.key());
        for len in [0, 1, 15, 16, 17, 40] {
            let header = source.bytes(21);
            let sealed = ours.seal(&header, &source.bytes(len));
            for at in 0..sealed.len() {
                for bit in 0..8 {
                    let mut changed = sealed.clone();
                    changed[at] ^= 1 << bit;

                    assert_eq!(ours.open(&header, &changed), None, "byte {at}, bit {bit}");
                }
            }
        }
    }

    #[test]
    fn a_value_is_not_opened_for_another_header_or_under_another_key() {
        let mut source = Source(4);
        let mut ours = Siv::new(&source.key());
        let mut another = Siv::new(&source.key());
        let sealed = ours.seal(b"player/42/gems", b"1200");

        assert_eq!(ours.open(b"player/43/gems", &sealed), None);
        assert_eq!(ours.open(b"", &sealed), None);
        assert_eq!(another.open(b"player/42/gems", &sealed), None);
        assert_eq!(
            ours.open(b"player/42/gems", &sealed),
            Some(b"1200".to_vec())
        );
    }

    #[test]
    fn bytes_shorter_than_a_tag_are_refused() {
        let mut source = Source(5);
        let mut ours = Siv::new(&source.key());
        for len in 0..TAG_LEN {
            assert_eq!(ours.open(b"k", &source.bytes(len)), None, "{len} bytes");
        }
    }

    /// The counter is one number of 128 bits, counted up from the tag
    /// with two of its bits cleared. Tags are chosen here, which sealing
    /// cannot do, so that the count carries from one byte into the
    /// next, into the bytes whose bits were cleared, and round to zero.
    #[test]
    fn the_counter_carries_as_one_number_of_128_bits() {
        use cipher::BlockCipherEncrypt;

        let mut source = Source(8);
        let key = source.key();
        let ours = Siv::new(&key);
        let alone = Aes256::new_from_slice(&key[KEY_LEN / 2..]).expect("32 bytes");
        let mut tags = vec![[0xff; TAG_LEN], [0; TAG_LEN], [0x7f; TAG_LEN]];
        tags.push(std::array::from_fn(|at| if at < 8 { 0x11 } else { 0xff }));
        tags.push(std::array::from_fn(|at| if at < 12 { 0x22 } else { 0xff }));
        tags.push(std::array::from_fn(|at| if at < 15 { 0x33 } else { 0xfe }));
        for tag in tags {
            let value = source.bytes(100 * TAG_LEN + 7);
            let mut cleared = tag;
            cleared[8] &= 0x7f;
            cleared[12] &= 0x7f;
            let first = u128::from_be_bytes(cleared);
            let mut expected = value.clone();
            for (count, part) in expected.chunks_mut(TAG_LEN).enumerate() {
                let mut block = Block::from(first.wrapping_add(count as u128).to_be_bytes());
                alone.encrypt_block(&mut block);
                for (byte, pad) in part.iter_mut().zip(block) {
                    *byte ^= pad;
                }
            }

            let mut bytes = value;
            ours.apply_keystream(Block::from(tag), &mut bytes);

            assert_eq!(bytes, expected, "tag {tag:02x?}");
        }
    }

    /// Two answers `aes-siv` 0.8.0 gave once, kept here: the other tests
    /// compare with whatever version of it is built, and these do not
    /// move with it.
    #[test]
    fn it_seals_two_values_to_bytes_written_down() {
        let key: Key = std::array::from_fn(|at| at as u8);
        let mut ours = Siv::new(&key);
        let hex =
            |bytes: Vec<u8>| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };

        let short = ours.seal(b"player/42/gems", b"1200");
        let long = ours.seal(b"player/42/gems", &[0xa5; 40]);

        assert_eq!(hex(short), "4a226d0690b2f6ac3503ee3a3625271652863384");
        assert_eq!(
            hex(long),
            "486133cda8c25a675cc656fcc4a27fffaf41a5854ad748f394b06a14ccf13ea0\
             6a0ab0d81df3d97f83a5722b99a3a47ad7fd3d2bb87273a7"
        );
    }

    /// What sealing and opening one value take, with `aes-siv` and here.
    /// `cargo test --release -p umbrachyx --lib what_sealing_costs -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn what_sealing_costs() {
        let mut source = Source(7);
        let key = source.key();
        let mut theirs = Aes256Siv::new_from_slice(&key).expect("a key of 64 bytes");
        let mut ours = Siv::new(&key);
        let header = b"player/000000123456/gems";
        println!("value_bytes,operation,aes_siv_ns,here_ns");
        for len in [32, 256, 4_096] {
            let value = source.bytes(len);
            let sealed = ours.seal(header, &value);
            let rounds = 2_000_000 / (1 + len / 256);
            let each = |took: std::time::Duration| took.as_nanos() / rounds as u128;

            let started = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(theirs.encrypt([header], &value).expect("seals"));
            }
            let seal_theirs = each(started.elapsed());
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(ours.seal(header, &value));
            }
            let seal_ours = each(started.elapsed());
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(theirs.decrypt([header], &sealed).expect("opens"));
            }
            let open_theirs = each(started.elapsed());
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(ours.open(header, &sealed));
            }
            let open_ours = each(started.elapsed());

            println!("{len},seal,{seal_theirs},{seal_ours}");
            println!("{len},open,{open_theirs},{open_ours}");
        }
    }

    #[test]
    fn one_cipher_seals_many_values_as_a_new_one_would_each() {
        let mut source = Source(6);
        let key = source.key();
        let mut kept = Siv::new(&key);
        for len in [0, 5, 16, 33, 1_000, 3, 64] {
            let header = source.bytes(12);
            let value = source.bytes(len);

            let sealed = kept.seal(&header, &value);

            assert_eq!(sealed, Siv::new(&key).seal(&header, &value));
            assert_eq!(kept.open(&header, &sealed), Some(value));
        }
    }
}
