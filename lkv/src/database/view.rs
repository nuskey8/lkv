use super::state::{OverlayEntry, OverlayMap, SegmentVerifier};
use crate::format::segment::{BASE_HEADER, SLOT_SIZE};
use crate::{Result, VerificationMode};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone, Copy)]
pub struct BaseView<'a> {
    pub mapping: &'a [u8],
    pub verifier: &'a SegmentVerifier,
    pub offset: u64,
    pub slots: u64,
}

#[derive(Clone, Copy)]
pub struct ReadView<'a> {
    pub base: BaseView<'a>,
    pub overlay: &'a OverlayMap<OverlayEntry>,
    pub verification: VerificationMode,
}

impl<'a> ReadView<'a> {
    pub fn get(self, key: &[u8]) -> Result<Option<&'a [u8]>> {
        let key_hash = xxh3_64(key);
        if let Some(entry) = self.overlay.get_hashed(key, key_hash) {
            return Ok(match entry {
                OverlayEntry::Delete => None,
                OverlayEntry::Put(value) => Some(value.as_slice()),
            });
        }
        lookup_segment(
            self.base.mapping,
            self.verification,
            self.base.offset,
            self.base.slots,
            self.base.verifier,
            key,
            key_hash,
        )
    }

    pub fn verify_pair(self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.verification != VerificationMode::OnRead {
            return Ok(());
        }
        if let Some((start, end)) = pair_range(self.base.mapping, key, value) {
            return self
                .base
                .verifier
                .verify_range(self.base.mapping, start, end);
        }
        Ok(())
    }

    pub fn overlay_iter(self) -> hashbrown::hash_table::Iter<'a, (Vec<u8>, OverlayEntry)> {
        self.overlay.iter()
    }
}

fn lookup_segment<'a>(
    mapping: &'a [u8],
    verification: VerificationMode,
    segment_offset: u64,
    slots: u64,
    verifier: &SegmentVerifier,
    key: &[u8],
    key_hash: u64,
) -> Result<Option<&'a [u8]>> {
    if slots == 0 {
        return Ok(None);
    }
    let verify_on_read = verification == VerificationMode::OnRead && !verifier.is_fully_verified();
    for probe in 0..slots {
        let slot = ((key_hash.wrapping_add(probe)) & (slots - 1)) as usize;
        let slot_offset = BASE_HEADER + slot * SLOT_SIZE;
        if verify_on_read {
            verifier.verify_range(mapping, slot_offset, slot_offset + SLOT_SIZE)?;
        }
        let stored_fingerprint = u32_at(mapping, slot_offset);
        let absolute_offset = u64_at(mapping, slot_offset + 4);
        if absolute_offset == 0 {
            return Ok(None);
        }
        if stored_fingerprint == (key_hash >> 32) as u32 {
            let Some(offset) = absolute_offset
                .checked_sub(segment_offset)
                .and_then(|offset| usize::try_from(offset).ok())
            else {
                return Ok(None);
            };
            let Some(header_end) = offset.checked_add(8) else {
                return Ok(None);
            };
            let Some(header) = mapping.get(offset..header_end) else {
                return Ok(None);
            };
            if header_end > verifier.data_size() {
                return Ok(None);
            }
            let key_len = u32_at(header, 0) as usize;
            let value_len = u32_at(header, 4) as usize;
            let key_start = offset + 8;
            let Some(value_start) = key_start.checked_add(key_len) else {
                return Ok(None);
            };
            let Some(value_end) = value_start.checked_add(value_len) else {
                return Ok(None);
            };
            if verify_on_read {
                verifier.verify_range(mapping, offset, value_end)?;
            }
            if value_end <= verifier.data_size() && mapping.get(key_start..value_start) == Some(key)
            {
                return Ok(mapping.get(value_start..value_end));
            }
        }
    }
    Ok(None)
}

#[inline]
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn pair_range(mapping: &[u8], key: &[u8], value: &[u8]) -> Option<(usize, usize)> {
    let mapping_start = mapping.as_ptr() as usize;
    let mapping_end = mapping_start + mapping.len();
    let key_start = key.as_ptr() as usize;
    let value_end = value.as_ptr() as usize + value.len();
    if key_start >= mapping_start && value_end >= key_start && value_end <= mapping_end {
        Some((key_start - mapping_start, value_end - mapping_start))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::pair_range;

    #[test]
    fn pair_range_requires_both_slices_to_belong_to_the_mapping() {
        let mapping = [0; 16];
        let foreign = [0; 8];

        assert_eq!(
            pair_range(&mapping, &mapping[2..5], &mapping[8..12]),
            Some((2, 12))
        );
        assert_eq!(pair_range(&mapping, &mapping[2..5], &foreign), None);
        assert_eq!(pair_range(&mapping, &foreign, &mapping[8..12]), None);
        assert_eq!(pair_range(&mapping, &mapping[8..12], &mapping[2..5]), None);
    }
}
