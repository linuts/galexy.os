//! GALF v13: 4 KiB blocks, 32-bit lengths, extent maps, pool sized to the disk.
//!
//! A v12 image (512-byte blocks, 32 KiB files, the whole pool inside one
//! sealed payload) stays readable. v13 keeps that dual-slot commit — two
//! sealed metadata slots, then a data region of 4 KiB blocks covering the
//! rest of the disk. File bytes are written before the metadata slot that
//! names them, so a crash mid-write leaves the previous slot's extents.
//!
//! The sealed payload is the extent map, not the file bytes. `fsck`
//! rebuilds the allocation bitmap from the extents.

use galexy_core::crc32;
use galexy_crypto::{derive_key, open, seal, wipe_bytes, KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN};

use crate::{Issue, Report, SealNonces, DISK_MAGIC, MAX_ISSUES};

/// On-disk version.
pub const VERSION: u16 = 13;
/// Data block size.
pub const BLOCK: usize = 4096;
/// Bytes in one metadata slot (header + sealed extent map).
pub const META_BYTES: usize = 128 * 512;
/// Slots. The inactive one is the crash copy.
pub const SLOT_COUNT: usize = 2;
/// Largest file length the map records.
pub const FILE_MAX: u32 = 64 * 1024 * 1024;
/// A disk at least this big is a v13 volume (smaller images stay v12).
pub const MIN_DISK_BYTES: usize = 8 * 1024 * 1024;
/// Extents stored per file.
pub const EXTENTS_PER_FILE: usize = 8;
/// Files named in one metadata slot.
pub const MAX_FILES: usize = 32;
const HEADER: usize = 128;
const ADMIN_NAME: &[u8] = b"admin";

const FILE_BYTES: usize = 1 + 64 + 4 + 1 + EXTENTS_PER_FILE * 8;
const PAYLOAD: usize = 36 + MAX_FILES * FILE_BYTES;

const _: () = assert!(HEADER + PAYLOAD <= META_BYTES);
const _: () = assert!(FILE_MAX / BLOCK as u32 <= (BLOCK as u32 / 4) * (BLOCK as u32 / 4));

/// One extent: `count` blocks starting at `start` in the data pool.
#[derive(Clone, Copy)]
pub struct Extent {
    /// First data-pool block.
    pub start: u32,
    /// Block count.
    pub count: u32,
}

/// One file in the sealed map.
#[derive(Clone, Copy)]
pub struct FileRec {
    /// Object name.
    pub name: [u8; 64],
    /// Name length.
    pub name_len: u8,
    /// Byte length (`u32`, so a file can pass 64 KiB).
    pub len: u32,
    /// Live prefix of [`FileRec::extents`].
    pub extent_count: u8,
    /// Extent list.
    pub extents: [Extent; EXTENTS_PER_FILE],
}

impl FileRec {
    /// Empty record.
    pub const fn empty() -> Self {
        Self {
            name: [0; 64],
            name_len: 0,
            len: 0,
            extent_count: 0,
            extents: [Extent { start: 0, count: 0 }; EXTENTS_PER_FILE],
        }
    }
}

/// How many 4 KiB blocks fit after the two metadata slots.
pub fn pool_blocks_for(disk_bytes: usize) -> u32 {
    let data = disk_bytes.saturating_sub(SLOT_COUNT * META_BYTES);
    (data / BLOCK) as u32
}

/// Byte offset of data block `index`.
pub fn data_offset(index: u32) -> usize {
    SLOT_COUNT * META_BYTES + index as usize * BLOCK
}

fn aad(generation: u64) -> [u8; 14] {
    let mut aad = [0u8; 14];
    aad[0..4].copy_from_slice(&DISK_MAGIC);
    aad[4..6].copy_from_slice(&VERSION.to_le_bytes());
    aad[6..14].copy_from_slice(&generation.to_le_bytes());
    aad
}

/// Seals `files` into metadata slot bytes `meta` (length [`META_BYTES`]).
///
/// `pool_blocks` is the data region that follows both slots. Returns
/// false when `meta` is short or `files` does not fit.
pub fn encode_meta(
    generation: u64,
    pool_blocks: u32,
    passphrase: &[u8],
    volume_key: &[u8; KEY_LEN],
    nonces: &SealNonces,
    files: &[FileRec],
    meta: &mut [u8],
) -> bool {
    if meta.len() < META_BYTES || files.len() > MAX_FILES {
        return false;
    }
    meta[..META_BYTES].fill(0);
    meta[0..4].copy_from_slice(&DISK_MAGIC);
    meta[4..6].copy_from_slice(&VERSION.to_le_bytes());
    meta[6..8].copy_from_slice(&(BLOCK as u16).to_le_bytes());
    meta[8..12].copy_from_slice(&pool_blocks.to_le_bytes());
    meta[12] = 1;
    meta[16..24].copy_from_slice(&generation.to_le_bytes());

    let payload = &mut meta[HEADER..HEADER + PAYLOAD];
    payload[0] = ADMIN_NAME.len() as u8;
    payload[1..1 + ADMIN_NAME.len()].copy_from_slice(ADMIN_NAME);
    payload[32..36].copy_from_slice(&(files.len() as u32).to_le_bytes());
    for (i, file) in files.iter().enumerate() {
        let off = 36 + i * FILE_BYTES;
        payload[off] = file.name_len.min(64);
        let n = file.name_len.min(64) as usize;
        payload[off + 1..off + 1 + n].copy_from_slice(&file.name[..n]);
        payload[off + 65..off + 69].copy_from_slice(&file.len.to_le_bytes());
        payload[off + 69] = file.extent_count.min(EXTENTS_PER_FILE as u8);
        for (e, ext) in file.extents.iter().enumerate() {
            let eo = off + 70 + e * 8;
            payload[eo..eo + 4].copy_from_slice(&ext.start.to_le_bytes());
            payload[eo + 4..eo + 8].copy_from_slice(&ext.count.to_le_bytes());
        }
    }

    let aad = aad(generation);
    let mut kek = [0u8; KEY_LEN];
    derive_key(passphrase, &nonces.kdf_salt, &mut kek);
    let mut wrapped = *volume_key;
    let mut wrap_tag = [0u8; TAG_LEN];
    seal(&kek, &nonces.wrap_nonce, &aad, &mut wrapped, &mut wrap_tag);
    wipe_bytes(&mut kek);
    meta[32..40].copy_from_slice(&nonces.kdf_salt);
    meta[40..52].copy_from_slice(&nonces.wrap_nonce);
    meta[52..84].copy_from_slice(&wrapped);
    meta[84..100].copy_from_slice(&wrap_tag);
    wipe_bytes(&mut wrapped);

    let mut data_tag = [0u8; TAG_LEN];
    seal(
        volume_key,
        &nonces.data_nonce,
        &aad,
        &mut meta[HEADER..HEADER + PAYLOAD],
        &mut data_tag,
    );
    meta[100..112].copy_from_slice(&nonces.data_nonce);
    meta[112..128].copy_from_slice(&data_tag);
    let sum = crc32(&meta[HEADER..HEADER + PAYLOAD]);
    meta[24..28].copy_from_slice(&sum.to_le_bytes());
    true
}

struct Opened {
    generation: u64,
    pool_blocks: u32,
    files: [FileRec; MAX_FILES],
    file_count: usize,
    admin: bool,
}

fn decode_meta(meta: &mut [u8], passphrase: &[u8]) -> Option<Opened> {
    if meta.len() < META_BYTES || meta[0..4] != DISK_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([meta[4], meta[5]]);
    let block = u16::from_le_bytes([meta[6], meta[7]]) as usize;
    if version != VERSION || block != BLOCK || meta[12] & 1 == 0 {
        return None;
    }
    let pool_blocks = u32::from_le_bytes(meta[8..12].try_into().ok()?);
    let generation = u64::from_le_bytes(meta[16..24].try_into().ok()?);
    let expect = u32::from_le_bytes(meta[24..28].try_into().ok()?);
    if crc32(&meta[HEADER..HEADER + PAYLOAD]) != expect {
        return None;
    }
    let aad = aad(generation);
    let kdf_salt: [u8; SALT_LEN] = meta[32..40].try_into().ok()?;
    let wrap_nonce: [u8; NONCE_LEN] = meta[40..52].try_into().ok()?;
    let mut wrapped: [u8; KEY_LEN] = meta[52..84].try_into().ok()?;
    let wrap_tag: [u8; TAG_LEN] = meta[84..100].try_into().ok()?;
    let data_nonce: [u8; NONCE_LEN] = meta[100..112].try_into().ok()?;
    let data_tag: [u8; TAG_LEN] = meta[112..128].try_into().ok()?;
    let mut kek = [0u8; KEY_LEN];
    derive_key(passphrase, &kdf_salt, &mut kek);
    if !open(&kek, &wrap_nonce, &aad, &mut wrapped, &wrap_tag) {
        wipe_bytes(&mut kek);
        wipe_bytes(&mut wrapped);
        return None;
    }
    wipe_bytes(&mut kek);
    if !open(
        &wrapped,
        &data_nonce,
        &aad,
        &mut meta[HEADER..HEADER + PAYLOAD],
        &data_tag,
    ) {
        wipe_bytes(&mut wrapped);
        return None;
    }
    wipe_bytes(&mut wrapped);
    let payload = &meta[HEADER..HEADER + PAYLOAD];
    let admin_len = payload[0] as usize;
    let admin = admin_len == ADMIN_NAME.len() && &payload[1..1 + admin_len] == ADMIN_NAME;
    let file_count = u32::from_le_bytes(payload[32..36].try_into().ok()?) as usize;
    if file_count > MAX_FILES {
        return None;
    }
    let mut files = [FileRec::empty(); MAX_FILES];
    for (rec, i) in files.iter_mut().zip(0..file_count) {
        let off = 36 + i * FILE_BYTES;
        rec.name_len = payload[off].min(64);
        let n = rec.name_len as usize;
        rec.name[..n].copy_from_slice(&payload[off + 1..off + 1 + n]);
        rec.len = u32::from_le_bytes(payload[off + 65..off + 69].try_into().ok()?);
        rec.extent_count = payload[off + 69].min(EXTENTS_PER_FILE as u8);
        for (ext, e) in rec.extents.iter_mut().zip(0..EXTENTS_PER_FILE) {
            let eo = off + 70 + e * 8;
            *ext = Extent {
                start: u32::from_le_bytes(payload[eo..eo + 4].try_into().ok()?),
                count: u32::from_le_bytes(payload[eo + 4..eo + 8].try_into().ok()?),
            };
        }
    }
    Some(Opened {
        generation,
        pool_blocks,
        files,
        file_count,
        admin,
    })
}

/// Checks a v13 image. `image` holds both metadata slots and the data pool.
pub fn check(image: &[u8], passphrase: &[u8]) -> Report {
    let mut report = Report {
        ok: true,
        slot: None,
        generation: None,
        issue_count: 0,
        issues: [Issue::NoValidSlot; MAX_ISSUES],
    };
    if image.len() < SLOT_COUNT * META_BYTES {
        report.push(Issue::NoValidSlot);
        return report;
    }
    let mut best: Option<Opened> = None;
    let mut best_slot = 0u32;
    let mut saw_magic = false;
    let mut bad = 0u32;
    for slot in 0..SLOT_COUNT as u32 {
        let off = slot as usize * META_BYTES;
        let mut meta = [0u8; META_BYTES];
        meta.copy_from_slice(&image[off..off + META_BYTES]);
        if meta[0..4] == DISK_MAGIC {
            saw_magic = true;
        }
        match decode_meta(&mut meta, passphrase) {
            Some(opened) => {
                let replace = match &best {
                    None => true,
                    Some(prev) => opened.generation >= prev.generation,
                };
                if replace {
                    best_slot = slot;
                    best = Some(opened);
                }
            }
            None => {
                if meta[0..4] == DISK_MAGIC {
                    bad += 1;
                }
            }
        }
    }
    let Some(opened) = best else {
        report.push(if saw_magic && bad > 0 {
            Issue::BothSlotsCorrupt
        } else {
            Issue::NoValidSlot
        });
        return report;
    };
    report.slot = Some(best_slot);
    report.generation = Some(opened.generation);
    if !opened.admin {
        report.push(Issue::MissingAdmin);
    }
    let need = data_offset(opened.pool_blocks);
    if image.len() < need {
        report.push(Issue::BlockMissing { block: 0 });
    }
    let mut seen_words = [0u64; (FILE_MAX as usize / BLOCK) / 64 + 8];
    for (i, file) in opened.files.iter().take(opened.file_count).enumerate() {
        if file.len > FILE_MAX {
            report.push(Issue::FileTooLarge { object: i as u16 });
        }
        let mut covered = 0u64;
        for ext in file.extents.iter().take(file.extent_count as usize) {
            let end = ext.start as u64 + ext.count as u64;
            if ext.count == 0 || end > u64::from(opened.pool_blocks) {
                report.push(Issue::BlockMissing { block: i as u16 });
                continue;
            }
            covered += u64::from(ext.count) * BLOCK as u64;
            for b in ext.start..ext.start.saturating_add(ext.count) {
                let word = (b as usize) / 64;
                let bit = 1u64 << (b % 64);
                if word >= seen_words.len() || seen_words[word] & bit != 0 {
                    report.push(Issue::BlockDuplicate { block: i as u16 });
                    break;
                }
                if word < seen_words.len() {
                    seen_words[word] |= bit;
                }
            }
        }
        if u64::from(file.len) > covered {
            report.push(Issue::BlockMissing { block: i as u16 });
        }
    }
    if report.issue_count == 0 {
        report.ok = true;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    fn nonces() -> SealNonces {
        SealNonces {
            kdf_salt: [9; SALT_LEN],
            wrap_nonce: [1; NONCE_LEN],
            data_nonce: [2; NONCE_LEN],
        }
    }

    fn image_with(files: &[FileRec], pool: u32) -> Vec<u8> {
        let mut image = vec![0u8; data_offset(pool)];
        let key = [7u8; KEY_LEN];
        let mut meta = [0u8; META_BYTES];
        assert!(encode_meta(
            1,
            pool,
            b"galfs",
            &key,
            &nonces(),
            files,
            &mut meta
        ));
        image[..META_BYTES].copy_from_slice(&meta);
        image
    }

    #[test]
    fn empty_v13_checks_clean() {
        let pool = pool_blocks_for(MIN_DISK_BYTES);
        assert!(pool > 0);
        let image = image_with(&[], pool);
        let report = check(&image, b"galfs");
        assert!(report.ok, "issues {}", report.issue_count);
        assert_eq!(report.generation, Some(1));
    }

    #[test]
    fn sixty_four_mib_file_checks_clean() {
        let blocks = FILE_MAX / BLOCK as u32;
        let mut file = FileRec::empty();
        file.name[..3].copy_from_slice(b"big");
        file.name_len = 3;
        file.len = FILE_MAX;
        file.extent_count = 1;
        file.extents[0] = Extent {
            start: 0,
            count: blocks,
        };
        let image = image_with(&[file], blocks);
        let report = check(&image, b"galfs");
        assert!(report.ok, "issues {}", report.issue_count);
        assert!(image.len() >= FILE_MAX as usize);
    }

    #[test]
    fn overlapping_extents_fail() {
        let mut file = FileRec::empty();
        file.name_len = 1;
        file.name[0] = b'a';
        file.len = BLOCK as u32;
        file.extent_count = 2;
        file.extents[0] = Extent { start: 0, count: 2 };
        file.extents[1] = Extent { start: 1, count: 1 };
        let image = image_with(&[file], 8);
        let report = check(&image, b"galfs");
        assert!(!report.ok);
    }

    #[test]
    fn check_image_accepts_v13() {
        let pool = pool_blocks_for(MIN_DISK_BYTES);
        let image = image_with(&[], pool);
        let mut slot = vec![0u8; crate::DISK_SECTORS * crate::SECTOR];
        let mut best = crate::Table::empty();
        let mut cand = crate::Table::empty();
        let report = crate::check_image(&image, b"galfs", &mut slot, &mut best, &mut cand);
        assert!(report.ok, "issues {}", report.issue_count);
        assert_eq!(report.generation, Some(1));
    }

    #[test]
    fn wrong_passphrase_is_not_a_slot() {
        let pool = pool_blocks_for(MIN_DISK_BYTES);
        let image = image_with(&[], pool);
        let report = check(&image, b"nope");
        assert!(!report.ok);
        assert_eq!(report.issues[0], Issue::BothSlotsCorrupt);
    }
}
