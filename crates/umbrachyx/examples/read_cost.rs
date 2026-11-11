//! The CPU one read of a current value costs inside the process, with no
//! network: from a table that keeps its history (a walk down the prolly
//! tree) and from one that keeps only latest values (a HAMT lookup). The
//! difference is the most a hash index beside the tree could save a GET.
//!
//! `cargo run --release --example read_cost [-- <keys> <reads>]`

use std::io;
use std::time::Instant;

use umbrachyx::log::{Edit, Engine, Storage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock};

#[derive(Default)]
struct Memory(Vec<u8>);

impl Storage for Memory {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.0.clone())
    }
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.truncate(len as usize);
        Ok(())
    }
}

fn by() -> Provenance {
    Provenance {
        who: "bench".to_owned(),
        why: "fill".to_owned(),
        approved_by: None,
    }
}

fn key(table: &str, n: usize) -> Vec<u8> {
    format!("{table}/key:{n:012}").into_bytes()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let keys: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let reads: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);
    let mut store = Store::open(Memory::default(), SystemClock).expect("opens");
    store
        .declare_table(b"latest", Engine::Latest, by())
        .expect("declares");
    for table in ["history", "latest"] {
        for chunk in (0..keys).collect::<Vec<_>>().chunks(1_000) {
            let edits = chunk
                .iter()
                .map(|&n| Edit {
                    key: key(table, n),
                    value: Some(vec![b'x'; 32]),
                })
                .collect();
            store.commit(MAIN_BRANCH, edits, by()).expect("fills");
        }
    }
    let view = store.view();
    for table in ["history", "latest"] {
        let wanted: Vec<Vec<u8>> = {
            let mut x = 0x9E37_79B9_7F4A_7C15_u64;
            (0..reads)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    key(table, x as usize % keys)
                })
                .collect()
        };
        let started = Instant::now();
        let found = wanted
            .iter()
            .filter(|key| view.get(MAIN_BRANCH, key).is_some())
            .count();
        let per = started.elapsed().as_nanos() / reads as u128;
        assert_eq!(found, reads);
        println!("keys={keys} table={table} get_ns={per}");
    }
}
