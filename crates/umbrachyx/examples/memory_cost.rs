//! The memory one single-key commit leaves held, counted by the allocator
//! rather than read from the process's resident size: bytes allocated and
//! not freed, before and after a run of commits that each overwrite one
//! existing key. The log is thrown away as it is written, so what is
//! counted is what the store keeps in memory.
//!
//! `cargo run --release --example memory_cost [-- <keys> <commits> [plain]]`

// Counting what is allocated means standing in for the allocator, and the
// allocator's trait is unsafe. Only this example does so; the library and
// the binary have no unsafe code.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

use umbrachyx::keys::{KeyStorage, Keys};
use umbrachyx::log::{Edit, Storage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock};

/// The system's allocator, counting the bytes handed out and not yet
/// given back.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is passed to the system's allocator unchanged; the
// counter is only read.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A log that keeps nothing: the store only reads it when it opens.
struct Discard;

impl Storage for Discard {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn append(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn truncate(&mut self, _len: u64) -> io::Result<()> {
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
    let keys: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let commits: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(200_000);
    let plain = args.next().as_deref() == Some("plain");
    let mut store = if plain {
        Store::open(Discard, SystemClock).expect("opens")
    } else {
        let storage: Box<dyn KeyStorage + Send> = Box::new(MemoryKeys::default());
        Store::open_sealed(Discard, Keys::open(storage).expect("keys"), SystemClock).expect("opens")
    };
    store.create_branch("bench", MAIN_BRANCH).expect("branches");
    for chunk in (0..keys).collect::<Vec<_>>().chunks(1_000) {
        store
            .commit("bench", chunk.iter().map(|&n| set(n)).collect(), by())
            .expect("fills");
    }
    let filled = LIVE.load(Ordering::Relaxed);
    let cells = store.cells_kept();
    let mut x = 0x9E37_79B9_7F4A_7C15_u64;
    for _ in 0..commits {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        store
            .commit("bench", vec![set(x as usize % keys)], by())
            .expect("commits");
    }
    let held = LIVE.load(Ordering::Relaxed) - filled;
    println!(
        "keys={keys} sealed={} filled_bytes_per_key={} bytes_per_commit={} cells_kept=+{}",
        !plain,
        filled / keys,
        held / commits,
        store.cells_kept() - cells,
    );
}
