//! The memory allocator of the `umbrachyx` binary, and what it is told
//! when the process starts. The in-process benchmark includes this file
//! too, so what it measures is what the server does. The library and its
//! tests keep the system's allocator.

/// jemalloc rather than the system's allocator: when nodes were hashed on
/// every write, each hash filled a buffer and glibc's malloc took twice the
/// time jemalloc does (`docs/decisions.md`). Nodes are now hashed only for
/// diffs; the allocators were not compared again since.
#[cfg(not(windows))]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Asks Linux for huge pages for the heap and for the allocator's own
/// records. A write copies a path of tree nodes spread over the whole
/// heap, and with 4 KiB pages most of its time went into the processor
/// looking up where those pages are: on the release machine a commit took
/// 14.5 µs without this and 11.2 µs with it, for 1.3% more memory
/// (`docs/decisions.md`). It does nothing where the kernel's transparent
/// huge pages are set to `never`, and `_RJEM_MALLOC_CONF=thp:default,
/// metadata_thp:disabled` turns it off for one run.
///
/// jemalloc finds this by its name in the finished program, which is why
/// the name is spelled out and the lint is allowed here; a test asks the
/// allocator whether it took effect (`tests/allocator.rs`).
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
#[used]
#[unsafe(export_name = "_rjem_malloc_conf")]
static ALLOCATOR_SETTINGS: &[u8; 31] = b"thp:always,metadata_thp:always\0";
