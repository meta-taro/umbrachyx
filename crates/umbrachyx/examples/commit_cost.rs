//! The CPU one single-key commit costs with the disk taken out: storage
//! that keeps appends in memory and syncs for free. What is left is what
//! the store does around the sync.
//!
//! With a group size, the commits are written that many to a sync, as the
//! server writes what many clients sent together: a group's cells go into
//! the tree once.
//!
//! `cargo run --release --example commit_cost [-- <keys> <commits> [plain|sealed [group]]]`

use std::io;
use std::time::Instant;

use umbrachyx::keys::{KeyStorage, Keys};
use umbrachyx::log::{Edit, Storage};
use umbrachyx::store::{CommitRequest, MAIN_BRANCH, Provenance, Store, SystemClock};

/// The allocator the server uses, so the cost here is the cost there.
#[cfg(not(windows))]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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

#[derive(Default)]
struct MemoryKeys(Vec<u8>);

impl KeyStorage for MemoryKeys {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.0.clone())
    }
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn replace(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0 = bytes.to_vec();
        Ok(())
    }
}

fn by() -> Provenance {
    Provenance {
        who: "bench".to_owned(),
        why: "SET".to_owned(),
        approved_by: None,
    }
}

fn set(n: usize) -> Edit {
    Edit {
        key: format!("key:{n:012}").into_bytes(),
        value: Some(vec![b'x'; 32]),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let keys: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200_000);
    let commits: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let plain = args.next().as_deref() == Some("plain");
    let group: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1);
    let mut store = if plain {
        Store::open(Memory::default(), SystemClock).expect("opens")
    } else {
        let storage: Box<dyn KeyStorage + Send> = Box::new(MemoryKeys::default());
        Store::open_sealed(
            Memory::default(),
            Keys::open(storage).expect("keys"),
            SystemClock,
        )
        .expect("opens")
    };
    store.create_branch("bench", MAIN_BRANCH).expect("branches");
    for chunk in (0..keys).collect::<Vec<_>>().chunks(1_000) {
        store
            .commit("bench", chunk.iter().map(|&n| set(n)).collect(), by())
            .expect("fills");
    }
    let mut x = 0x9E37_79B9_7F4A_7C15_u64;
    let started = Instant::now();
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        set(x as usize % keys)
    };
    for _ in 0..commits / group {
        if group == 1 {
            store.commit("bench", vec![next()], by()).expect("commits");
            continue;
        }
        let requests = (0..group)
            .map(|_| CommitRequest {
                branch: "bench".to_owned(),
                edits: vec![next()],
                provenance: by(),
            })
            .collect();
        store.commit_all(requests).expect("commits");
    }
    let per = started.elapsed().as_nanos() / commits as u128;
    let started = Instant::now();
    for _ in 0..commits {
        std::hint::black_box(store.view());
    }
    let view = started.elapsed().as_nanos() / commits as u128;
    println!(
        "keys={keys} sealed={} group={group} commit_ns={per} view_ns={view}",
        !plain
    );
}
