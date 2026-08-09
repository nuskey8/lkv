use crate::common::{INITIAL_ENTRIES, key, read_indices, value};
use std::collections::HashMap;
use test::{Bencher, black_box};

type Map = HashMap<[u8; crate::common::KEY_SIZE], [u8; crate::common::VALUE_SIZE]>;

fn populate() -> Map {
    let mut map = HashMap::with_capacity(INITIAL_ENTRIES as usize);
    for index in 0..INITIAL_ENTRIES {
        map.insert(key(index), value(index, 0));
    }
    map
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    b.iter(|| black_box(populate()));
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let map = populate();
    let indices = read_indices();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = map.get(&key(index)).unwrap();
            sum.wrapping_add(u64::from(value[0]))
        });
        black_box(checksum)
    });
}
