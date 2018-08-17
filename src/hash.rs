use arrayvec::ArrayVec;
use blake2b_simd;
use byteorder::{ByteOrder, LittleEndian};
use crossbeam_channel as channel;
use num_cpus;
use rayon;
use std::cmp;
use std::collections::VecDeque;
use std::io;
use std::mem;

pub const HASH_SIZE: usize = 32;
pub const PARENT_SIZE: usize = 2 * HASH_SIZE;
pub const HEADER_SIZE: usize = 8;
pub const CHUNK_SIZE: usize = 4096;
pub const MAX_DEPTH: usize = 64;
pub const MAX_SINGLE_THREADED: usize = 4 * CHUNK_SIZE;

pub type Hash = [u8; HASH_SIZE];
pub type ParentNode = [u8; 2 * HASH_SIZE];

pub(crate) fn encode_len(len: u64) -> [u8; HEADER_SIZE] {
    debug_assert_eq!(mem::size_of_val(&len), HEADER_SIZE);
    let mut len_bytes = [0; HEADER_SIZE];
    LittleEndian::write_u64(&mut len_bytes, len);
    len_bytes
}

pub(crate) fn decode_len(bytes: [u8; HEADER_SIZE]) -> u64 {
    LittleEndian::read_u64(&bytes)
}

pub(crate) fn new_blake2b_state() -> blake2b_simd::State {
    blake2b_simd::Params::new()
        .hash_length(HASH_SIZE)
        .to_state()
}

// The root node is hashed differently from interior nodes. It gets suffixed
// with the length of the entire input, and we set the Blake2 final node flag.
// That means that no root hash can ever collide with an interior hash, or with
// the root of a different size tree.
#[derive(Clone, Copy, Debug)]
pub enum Finalization {
    NotRoot,
    Root(u64),
}
use self::Finalization::{NotRoot, Root};

pub(crate) fn finalize_hash(state: &mut blake2b_simd::State, finalization: Finalization) -> Hash {
    // For the root node, we hash in the length as a suffix, and we set the
    // Blake2 last node flag. One of the reasons for this design is that we
    // don't need to know a given node is the root until the very end, so we
    // don't always need a chunk buffer.
    if let Root(root_len) = finalization {
        state.update(&encode_len(root_len));
        state.set_last_node(true);
    }
    let blake_digest = state.finalize();
    *array_ref!(blake_digest.as_bytes(), 0, HASH_SIZE)
}

pub(crate) fn hash_node(chunk: &[u8], finalization: Finalization) -> Hash {
    debug_assert!(chunk.len() <= CHUNK_SIZE);
    let mut state = new_blake2b_state();
    state.update(chunk);
    finalize_hash(&mut state, finalization)
}

pub(crate) fn parent_hash(left_hash: &Hash, right_hash: &Hash, finalization: Finalization) -> Hash {
    let mut state = new_blake2b_state();
    state.update(left_hash);
    state.update(right_hash);
    finalize_hash(&mut state, finalization)
}

// Find the largest power of two that's less than or equal to `n`. We use this
// for computing subtree sizes below.
pub(crate) fn largest_power_of_two(n: u64) -> u64 {
    debug_assert!(n != 0);
    1 << (63 - n.leading_zeros())
}

// Given some input larger than one chunk, find the largest perfect tree of
// chunks that can go on the left.
pub(crate) fn left_len(content_len: u64) -> u64 {
    debug_assert!(content_len > CHUNK_SIZE as u64);
    // Subtract 1 to reserve at least one byte for the right side.
    let full_chunks = (content_len - 1) / CHUNK_SIZE as u64;
    largest_power_of_two(full_chunks) * CHUNK_SIZE as u64
}

pub fn hash_recurse(input: &[u8], finalization: Finalization) -> Hash {
    if input.len() <= CHUNK_SIZE {
        return hash_node(input, finalization);
    }
    // If we have more than one chunk of input, recursively hash the left and
    // right sides. The left_len() function determines the shape of the tree.
    let (left, right) = input.split_at(left_len(input.len() as u64) as usize);
    // Child nodes are never the root.
    let left_hash = hash_recurse(left, NotRoot);
    let right_hash = hash_recurse(right, NotRoot);
    parent_hash(&left_hash, &right_hash, finalization)
}

pub fn hash_recurse_rayon(input: &[u8], finalization: Finalization) -> Hash {
    if input.len() <= CHUNK_SIZE {
        return hash_node(input, finalization);
    }
    let (left, right) = input.split_at(left_len(input.len() as u64) as usize);
    let (left_hash, right_hash) = rayon::join(
        || hash_recurse_rayon(left, NotRoot),
        || hash_recurse_rayon(right, NotRoot),
    );
    parent_hash(&left_hash, &right_hash, finalization)
}

/// Hash a slice of input bytes all at once. Above about 16 kilobytes, this will parallelize using
/// [Rayon](https://crates.io/crates/rayon).
pub fn hash(input: &[u8]) -> Hash {
    // Below about 4 chunks, the overhead of parallelizing isn't worth it.
    if input.len() <= MAX_SINGLE_THREADED {
        hash_recurse(input, Root(input.len() as u64))
    } else {
        hash_recurse_rayon(input, Root(input.len() as u64))
    }
}

/// Mostly for benchmarks.
pub fn hash_single_threaded(input: &[u8]) -> Hash {
    hash_recurse(input, Finalization::Root(input.len() as u64))
}

/// A minimal state object for incrementally hashing input. Most callers should use the `Writer`
/// interface instead.
///
/// This is designed to be useful for as many callers as possible, including `no_std` callers. It
/// handles merging subtrees and keeps track of subtrees assembled so far. It takes only hashes as
/// input, rather than raw input bytes, so it can be used with e.g. multiple threads hashing chunks
/// in parallel. Callers that need `ParentNode` bytes for building the encoded tree, can use the
/// optional `merge_parent` and `merge_finish` interfaces.
///
/// This struct contains a relatively large buffer on the stack for holding partial subtree hashes:
/// 64 hashes at 32 bytes apiece, 2048 bytes in total. This is enough state space for the largest
/// possible input, `2^64 - 1` bytes or about 18 exabytes. That's impractically large for anything
/// that could be hashed in the real world, and implementations that are starved for stack space
/// could cut that buffer in half and still be able to hash about 17 terabytes (`2^32` times the
/// 4096-byte chunk size).
#[derive(Clone, Debug)]
pub struct State {
    subtrees: ArrayVec<[Hash; MAX_DEPTH]>,
    subtree_count: u64,
}

impl State {
    pub fn new() -> Self {
        Self {
            subtrees: ArrayVec::new(),
            subtree_count: 0,
        }
    }

    fn merge_inner(&mut self, finalization: Finalization) -> ParentNode {
        let right_child = self.subtrees.pop().unwrap();
        let left_child = self.subtrees.pop().unwrap();
        let mut parent_node = [0; PARENT_SIZE];
        parent_node[..HASH_SIZE].copy_from_slice(&left_child);
        parent_node[HASH_SIZE..].copy_from_slice(&right_child);
        let parent_hash = parent_hash(&left_child, &right_child, finalization);
        self.subtrees.push(parent_hash);
        parent_node
    }

    // We keep the subtree hashes in an array without storing their size, and we use this cute
    // trick to figure out when we should merge them. Because every subtree (prior to the
    // finalization step) is a power of two times the chunk size, adding a new subtree to the
    // right/small end is a lot like adding a 1 to a binary number, and merging subtrees is like
    // propagating the carry bit. Each carry represents a place where two subtrees need to be
    // merged, and the final number of 1 bits is the same as the final number of subtrees.
    fn needs_merge(&self) -> bool {
        self.subtrees.len() > self.subtree_count.count_ones() as usize
    }

    /// Add a subtree hash to the state.
    ///
    /// For most callers, this will always be the hash of a `CHUNK_SIZE` chunk of input bytes, with
    /// the final chunk possibly having fewer (but never zero) bytes. It's possible to use input
    /// subtrees larger than a single chunk, as long as the size is a power of 2 times `CHUNK_SIZE`
    /// and again kept constant until the final chunk. This might be helpful in elaborate
    /// multi-threaded settings with layers of `State` objects, but most callers should stick with
    /// single chunks.
    ///
    /// In cases where the total input is a single chunk or less, including the case with no input
    /// bytes at all, callers are expected to finalize that chunk and return the result *without*
    /// calling into this state object. It's of course impossible to back out the input bytes and
    /// re-finalize them.
    pub fn push_subtree(&mut self, hash: Hash) {
        // Merge any subtrees that need to be merged before pushing. In the encoding case, the
        // caller will already have done this via merge_parent(), but in the hashing case the
        // caller doesn't care about the parent nodes.
        while self.needs_merge() {
            self.merge_inner(NotRoot);
        }
        self.subtrees.push(hash);
        self.subtree_count += 1;
    }

    /// Returns a `ParentNode` corresponding to a just-completed subtree, if any.
    ///
    /// Callers that want parent node bytes (to build an encoded tree) must call `merge_parent` in
    /// a loop, until it returns `None`. Parent nodes are yielded in smallest-to-largest order.
    /// Callers that only want the final root hash can ignore this function; the next call to
    /// `push_subtree` will take care of merging in that case.
    ///
    /// After the final call to `push_subtree`, you must call `merge_finish` in a loop instead of
    /// this function.
    pub fn merge_parent(&mut self) -> Option<ParentNode> {
        if !self.needs_merge() {
            return None;
        }
        Some(self.merge_inner(NotRoot))
    }

    /// Returns a tuple of `ParentNode` bytes and (in the last call only) the root hash. Callers
    /// who need `ParentNode` bytes must call `merge_finish` in a loop after pushing the final
    /// subtree, until the second return value is `Some`. Callers who don't need parent nodes
    /// should use the simpler `finish` interface instead.
    pub fn merge_finish(&mut self, finalization: Finalization) -> (ParentNode, Option<Hash>) {
        assert!(self.subtrees.len() >= 2, "not enough subtrees");
        if self.subtrees.len() > 2 {
            (self.merge_inner(NotRoot), None)
        } else {
            let root_node = self.merge_inner(finalization);
            let root_hash = self.subtrees.pop().unwrap();
            (root_node, Some(root_hash))
        }
    }

    /// A wrapper around `merge_finish` for callers who don't need the parent
    /// nodes.
    pub fn finish(&mut self, finalization: Finalization) -> Hash {
        loop {
            if let (_, Some(root_hash)) = self.merge_finish(finalization) {
                return root_hash;
            }
        }
    }
}

/// A `std::io::Writer` interface to the incremental hasher. Most callers that can't use the
/// all-at-once `hash` function should use this interface.
#[derive(Clone, Debug)]
pub struct Writer {
    chunk: blake2b_simd::State,
    chunk_len: usize,
    total_len: u64,
    state: State,
}

impl Writer {
    pub fn new() -> Self {
        Self {
            chunk: new_blake2b_state(),
            chunk_len: 0,
            total_len: 0,
            state: State::new(),
        }
    }

    /// After feeding all the input bytes to `write`, return the root hash. The writer cannot be
    /// used after this.
    pub fn finish(&mut self) -> Hash {
        let finalization = Root(self.total_len);
        if self.total_len <= CHUNK_SIZE as u64 {
            return finalize_hash(&mut self.chunk, finalization);
        }
        let last_chunk_hash = finalize_hash(&mut self.chunk, NotRoot);
        self.state.push_subtree(last_chunk_hash);
        self.state.finish(finalization)
    }
}

impl io::Write for Writer {
    fn write(&mut self, mut input: &[u8]) -> io::Result<usize> {
        let input_len = input.len();
        while !input.is_empty() {
            if self.chunk_len == CHUNK_SIZE {
                let chunk_hash = finalize_hash(&mut self.chunk, NotRoot);
                self.state.push_subtree(chunk_hash);
                self.chunk = new_blake2b_state();
                self.chunk_len = 0;
            }

            let want = CHUNK_SIZE - self.chunk_len;
            let take = cmp::min(want, input.len());
            self.chunk.update(&input[..take]);
            self.chunk_len += take;
            self.total_len += take as u64;
            input = &input[take..];
        }
        Ok(input_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// benchmark_job_params.rs helps to tune these parameters.
lazy_static! {
    pub static ref MAX_JOBS: usize = 8 * num_cpus::get();
    pub static ref JOB_SIZE: usize = 65536; // 2^16
}

// TODO: Manually implement Clone by draining the receivers.
#[derive(Debug)]
pub struct RayonWriter {
    state: State,
    total_len: u64,
    current_buf: Vec<u8>,
    free_bufs: Vec<Vec<u8>>,
    receivers: VecDeque<channel::Receiver<(Hash, Vec<u8>)>>,
    job_size: usize,
    max_jobs: usize,
}

impl RayonWriter {
    pub fn new() -> Self {
        Self {
            state: State::new(),
            total_len: 0,
            // Use new() instead of with_capacity() to avoid a big allocation in the small case.
            current_buf: Vec::new(),
            free_bufs: Vec::new(),
            receivers: VecDeque::new(),
            job_size: *JOB_SIZE,
            max_jobs: *MAX_JOBS,
        }
    }

    pub fn new_benchmarking(job_size: usize, max_jobs: usize) -> Self {
        assert_eq!(0, job_size % CHUNK_SIZE);
        assert_eq!(1, (job_size / CHUNK_SIZE).count_ones());
        let mut writer = Self::new();
        writer.job_size = job_size;
        writer.max_jobs = max_jobs;
        writer
    }

    // Blocking on the very first channel would mean frequent context switches to the calling
    // thread, which is substantial overhead. But blocking on the very last channel would starve
    // the other worker threads for input, as they all slept waiting for the last one to finish.
    // Instead, block on the middle receiver.
    fn await_half(&mut self) -> Vec<u8> {
        debug_assert_eq!(self.max_jobs, self.receivers.len());
        let halfway = self.receivers.len() / 2;
        // Await the halfway receiver.
        let (halfway_hash, halfway_buf) = self.receivers[halfway].recv().expect("worker hung up");
        // Remove and process all the prior receivers.
        for receiver in self.receivers.drain(0..halfway) {
            let (hash, mut received_buf) = receiver.recv().expect("worker hung up");
            self.state.push_subtree(hash);
            self.free_bufs.push(received_buf);
        }
        // Remove the halfway receiver and process the results we got from it above. Return its
        // buffer directly to the caller.
        self.receivers.pop_front();
        self.state.push_subtree(halfway_hash);
        halfway_buf
    }

    /// After feeding all the input bytes to `write`, return the root hash. The writer cannot be
    /// used after this.
    pub fn finish(&mut self) -> Hash {
        if self.total_len <= self.job_size as u64 {
            return hash_recurse(&mut self.current_buf, Root(self.total_len));
        }
        let last_job_hash = hash_recurse(&mut self.current_buf, NotRoot);
        for receiver in self.receivers.drain(..) {
            let (hash, _) = receiver.recv().expect("worker hung up");
            self.state.push_subtree(hash);
        }
        self.state.push_subtree(last_job_hash);
        self.state.finish(Root(self.total_len))
    }
}

impl io::Write for RayonWriter {
    fn write(&mut self, mut input: &[u8]) -> io::Result<usize> {
        let input_len = input.len();
        while !input.is_empty() {
            if self.current_buf.len() == self.job_size {
                // First, get our hands on a new buffer. If we have a free one, or if we have
                // capacity to create a new one, use that. Otherwise, await half the outstanding
                // receivers to free up buffers.
                let mut new_buf;
                if let Some(buf) = self.free_bufs.pop() {
                    // A free buffer is already available.
                    new_buf = buf;
                } else if self.receivers.len() < self.max_jobs {
                    // We have space to create new buffers.
                    new_buf = Vec::with_capacity(self.job_size);
                } else {
                    // We need to await some existing buffers.
                    new_buf = self.await_half();
                }
                new_buf.clear();

                // Now swap the buffers and send the full one to a new job.
                let full_buf = mem::replace(&mut self.current_buf, new_buf);
                // Performance: crossbeam-channel seems to beat std::mpsc here.
                let (sender, receiver) = channel::bounded(1);
                self.receivers.push_back(receiver);
                rayon::spawn(move || {
                    // Performance: hash_recursive_rayon seems to be slower here.
                    let hash = hash_recurse(&full_buf, NotRoot);
                    sender.send((hash, full_buf));
                });
            }

            let want = self.job_size - self.current_buf.len();
            let take = cmp::min(want, input.len());
            self.current_buf.extend_from_slice(&input[..take]);
            self.total_len += take as u64;
            input = &input[take..];
        }
        Ok(input_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Interesting input lengths to run tests on.
#[cfg(test)]
pub(crate) const TEST_CASES: &[usize] = &[
    0,
    1,
    10,
    CHUNK_SIZE - 1,
    CHUNK_SIZE,
    CHUNK_SIZE + 1,
    2 * CHUNK_SIZE - 1,
    2 * CHUNK_SIZE,
    2 * CHUNK_SIZE + 1,
    3 * CHUNK_SIZE - 1,
    3 * CHUNK_SIZE,
    3 * CHUNK_SIZE + 1,
    4 * CHUNK_SIZE - 1,
    4 * CHUNK_SIZE,
    4 * CHUNK_SIZE + 1,
    16 * CHUNK_SIZE - 1,
    16 * CHUNK_SIZE,
    16 * CHUNK_SIZE + 1,
];

#[cfg(test)]
mod test {
    use super::*;
    use hex;
    use std::io::prelude::*;

    #[test]
    fn test_power_of_two() {
        let input_output = &[
            (1, 1),
            (2, 2),
            (3, 2),
            (4, 4),
            (5, 4),
            (6, 4),
            (7, 4),
            (8, 8),
            // the largest possible u64
            (0xffffffffffffffff, 0x8000000000000000),
        ];
        for &(input, output) in input_output {
            assert_eq!(
                output,
                largest_power_of_two(input),
                "wrong output for n={}",
                input
            );
        }
    }

    #[test]
    fn test_left_subtree_len() {
        let s = CHUNK_SIZE as u64;
        let input_output = &[(s + 1, s), (2 * s - 1, s), (2 * s, s), (2 * s + 1, 2 * s)];
        for &(input, output) in input_output {
            println!("testing {} and {}", input, output);
            assert_eq!(left_len(input), output);
        }
    }

    #[test]
    fn test_compare_python() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let hash_hex = hex::encode(hash(&input));

            // Have the Python implementation hash the same input, and make
            // sure the result is identical.
            let python_hash = cmd!("python3", "./python/bao.py", "hash")
                .input(input.clone())
                .read()
                .expect("is python3 installed?");
            assert_eq!(hash_hex, python_hash, "hashes don't match");
        }
    }

    #[test]
    fn test_serial_vs_parallel() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let hash_serial = hash_recurse(&input, Root(case as u64));
            let hash_parallel = hash_recurse_rayon(&input, Root(case as u64));
            let hash_highlevel = hash(&input);
            let hash_highlevel_single = hash_single_threaded(&input);
            assert_eq!(hash_serial, hash_parallel, "hashes don't match");
            assert_eq!(hash_serial, hash_highlevel, "hashes don't match");
            assert_eq!(hash_serial, hash_highlevel_single, "hashes don't match");
        }
    }

    fn drive_state(input: &[u8]) -> Hash {
        let finalization = Root(input.len() as u64);
        if input.len() <= CHUNK_SIZE {
            return hash_node(input, finalization);
        }
        let mut state = State::new();
        let chunk_hashes = input
            .chunks(CHUNK_SIZE)
            .map(|chunk| hash_node(chunk, NotRoot));
        for chunk_hash in chunk_hashes {
            state.push_subtree(chunk_hash);
        }
        state.finish(finalization)
    }

    #[test]
    fn test_state() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let expected = hash(&input);
            let found = drive_state(&input);
            assert_eq!(expected, found, "hashes don't match");
        }
    }

    #[test]
    fn test_writer() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let expected = hash(&input);

            let mut writer = Writer::new();
            writer.write_all(&input).unwrap();
            let found = writer.finish();
            assert_eq!(expected, found, "hashes don't match");
        }
    }

    #[test]
    fn test_rayon_writer() {
        let mut cases = TEST_CASES.to_vec();
        cases.push(*JOB_SIZE - 1);
        cases.push(*JOB_SIZE);
        cases.push(*JOB_SIZE + 1);
        cases.push(*MAX_JOBS * *JOB_SIZE - 1);
        cases.push(*MAX_JOBS * *JOB_SIZE);
        cases.push(*MAX_JOBS * *JOB_SIZE + 1);
        cases.push(2 * *MAX_JOBS * *JOB_SIZE - 1);
        cases.push(2 * *MAX_JOBS * *JOB_SIZE);
        cases.push(2 * *MAX_JOBS * *JOB_SIZE + 1);
        for case in cases {
            println!("case {}", case);
            let input = vec![0x42; case];
            let expected = hash(&input);

            let mut rayon_writer = RayonWriter::new();
            rayon_writer.write_all(&input).unwrap();
            let rayon_found = rayon_writer.finish();
            assert_eq!(expected, rayon_found, "hashes don't match");
        }
    }
}
