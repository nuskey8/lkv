use std::fs;
use std::path::Path;
use tempfile::TempDir;

pub const INITIAL_ENTRIES: u64 = 100_000;
pub const READS_PER_ITER: usize = 100_000;
pub const BATCH_SIZE: u64 = 1_000;
pub const KEY_SIZE: usize = 24;
pub const VALUE_SIZE: usize = 150;
pub const BUCKET: &[u8] = b"kv";

pub fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("create benchmark directory")
}

pub fn key(index: u64) -> [u8; KEY_SIZE] {
    let mut key = [0; KEY_SIZE];
    key[..8].copy_from_slice(&index.to_be_bytes());
    let mut state = index ^ 0x9e37_79b9_7f4a_7c15;
    for chunk in key[8..].chunks_mut(8) {
        state = mix(state);
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    key
}

pub fn value(index: u64, generation: u64) -> [u8; VALUE_SIZE] {
    let mut value = [0; VALUE_SIZE];
    let mut state = index ^ generation.rotate_left(17) ^ 0xa076_1d64_78bd_642f;
    for chunk in value.chunks_mut(8) {
        state = mix(state);
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    value
}

pub fn read_indices() -> Vec<u64> {
    let mut indices = (0..INITIAL_ENTRIES).collect::<Vec<_>>();
    let mut state = 3u64;
    for index in (1..indices.len()).rev() {
        state = mix(state);
        indices.swap(index, state as usize % (index + 1));
    }
    indices.truncate(READS_PER_ITER);
    indices
}

pub fn directory_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .expect("read benchmark directory")
        .map(|entry| {
            let entry = entry.expect("read benchmark entry");
            let metadata = entry.metadata().expect("read benchmark metadata");
            if metadata.is_dir() {
                directory_size(&entry.path())
            } else if metadata.is_file() {
                metadata.len()
            } else {
                0
            }
        })
        .sum()
}

pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .expect("read benchmark file metadata")
        .len()
}

pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 20) as f64
}

fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
