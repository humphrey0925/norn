//! Every name of every file, once.
//!
//! A tree's identifiers are few and written everywhere: the kernel's
//! sixty thousand files hold twenty-four million names between them, and
//! under four million different ones. A file keeps a number for each name
//! it uses; the text is here, one copy for the life of the process.
//!
//! A number, once given, always reads as the same text, and reading takes
//! no lock: the text of a name is written before its number is handed out,
//! and nothing is ever moved or freed.

use hashbrown::HashTable;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::Mutex;

const SHARDS: usize = 64;
/// Text is kept in blocks of this size, one being filled per shard.
const BLOCK: usize = 256 << 10;
const CHUNK_BITS: u32 = 16;
const CHUNK: usize = 1 << CHUNK_BITS;
/// Numbers stay below this: a file marks with the top bit what is its own.
pub const LIMIT: u32 = 1 << 31;

struct Shard {
    /// The numbers given, to be found by the hash of their text. Only the
    /// numbers: the text is where [`text`] finds it, and a table that also
    /// held where and how long (and so was three times the size) would be,
    /// for the kernel's four million names, 0.2 GB that says nothing new.
    ids: HashTable<u32>,
    /// Where the block being filled has room, and how much.
    free: *mut u8,
    left: usize,
}

// SAFETY: `free` points into a block only this shard writes to, and only
// under its lock.
unsafe impl Send for Shard {}

static SHARDS_: [Mutex<Option<Shard>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];
static NEXT: AtomicU32 = AtomicU32::new(0);
/// For each number, where its text is: a length (four bytes) and the bytes.
/// In chunks, made as the numbers reach them.
static PLACES: [AtomicPtr<AtomicPtr<u8>>; (LIMIT >> CHUNK_BITS) as usize] = [const { AtomicPtr::new(std::ptr::null_mut()) }; (LIMIT >> CHUNK_BITS) as usize];

fn chunk(id: u32) -> &'static [AtomicPtr<u8>] {
    let slot = &PLACES[(id >> CHUNK_BITS) as usize];
    let mut at = slot.load(Ordering::Acquire);
    if at.is_null() {
        let made: Box<[AtomicPtr<u8>]> = (0..CHUNK).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect();
        let made = Box::into_raw(made) as *mut AtomicPtr<u8>;
        match slot.compare_exchange(std::ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => at = made,
            Err(theirs) => {
                // SAFETY: `made` came from `Box::into_raw` just above and
                // was shown to nobody.
                drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(made, CHUNK)) });
                at = theirs;
            }
        }
    }
    // SAFETY: a chunk is `CHUNK` places, and is never freed once it is in
    // `PLACES`.
    unsafe { std::slice::from_raw_parts(at, CHUNK) }
}

/// The number of `name`: the same for the same text, whoever asks.
pub fn id(name: &str) -> u32 {
    let hash = hash(name);
    // The shard by bits the table does not look at: it places by the low
    // ones and tells apart by the top seven.
    let mut shard = SHARDS_[(hash >> 32) as usize % SHARDS].lock().unwrap_or_else(|e| e.into_inner());
    let shard = shard.get_or_insert_with(|| Shard { ids: HashTable::new(), free: std::ptr::null_mut(), left: 0 });
    if let Some(&id) = shard.ids.find(hash, |&id| text(id) == name) {
        return id;
    }
    let need = 4 + name.len();
    let at = if need > BLOCK / 4 {
        Box::leak(vec![0u8; need].into_boxed_slice()).as_mut_ptr()
    } else {
        if shard.left < need {
            shard.free = Box::leak(vec![0u8; BLOCK].into_boxed_slice()).as_mut_ptr();
            shard.left = BLOCK;
        }
        let at = shard.free;
        // SAFETY: the block has `left >= need` bytes from `free`.
        shard.free = unsafe { at.add(need) };
        shard.left -= need;
        at
    };
    // SAFETY: `at` has room for `need` bytes that nobody else has been
    // shown; they are leaked, so the `str` that `text` makes of them lives
    // forever.
    unsafe {
        std::ptr::copy_nonoverlapping((name.len() as u32).to_le_bytes().as_ptr(), at, 4);
        std::ptr::copy_nonoverlapping(name.as_ptr(), at.add(4), name.len());
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    assert!(id < LIMIT, "too many names");
    chunk(id)[id as usize & (CHUNK - 1)].store(at, Ordering::Release);
    // (When the table grows it asks each number in it for its hash again:
    // every one of them has its text in place by then.)
    shard.ids.insert_unique(hash, id, |&id| self::hash(text(id)));
    id
}

fn hash(name: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(name.as_bytes())
}

/// The text of a number [`id`] gave.
pub fn text(id: u32) -> &'static str {
    let at = chunk(id)[id as usize & (CHUNK - 1)].load(Ordering::Acquire);
    assert!(!at.is_null(), "no name has this number");
    // SAFETY: what `id` wrote before it stored the pointer: a length, and
    // that many bytes of a `str`, never freed.
    unsafe {
        let mut len = [0u8; 4];
        std::ptr::copy_nonoverlapping(at, len.as_mut_ptr(), 4);
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(at.add(4), u32::from_le_bytes(len) as usize))
    }
}

/// One more than the largest number given so far.
pub fn count() -> u32 {
    NEXT.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_has_one_number_and_reads_back() {
        let a = super::id("names_test_alpha");
        let b = super::id("names_test_beta");
        assert_ne!(a, b);
        assert_eq!(super::id("names_test_alpha"), a);
        assert_eq!(super::text(a), "names_test_alpha");
        assert_eq!(super::text(b), "names_test_beta");
        let long = "x".repeat(super::BLOCK);
        assert_eq!(super::text(super::id(&long)), long);
    }

    #[test]
    fn threads_agree() {
        let ids: Vec<Vec<u32>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| (0..2000).map(|i| super::id(&format!("names_test_{i}"))).collect())).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.iter().all(|v| v == &ids[0]));
        assert_eq!(super::text(ids[0][1234]), "names_test_1234");
    }
}
