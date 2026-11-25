//! Shared GALF on-disk layout, unlock, and structural checks.
//!
//! Kernel and host tools must agree on these constants (STYLE: no
//! duplicated magic). The host `galfs-fsck` binary walks a dual-slot
//! image; the kernel keeps its live table path but must match versions.

#![no_std]
#![deny(clippy::all)]

extern crate alloc;

pub mod cards;
#[cfg(test)]
mod cards_test;
#[cfg(test)]
mod slots_test;
pub mod v13;
mod volume;

use alloc::vec::Vec;
use galexy_core::crc32;
use galexy_crypto::{
    derive_key, open, seal, wipe_bytes, HASH_LEN, KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN,
};

/// ATA / GALF sector size.
pub const SECTOR: usize = 512;
/// Dual-slot image sectors per slot (64 KiB superblock).
pub const DISK_SECTORS: usize = 128;
pub const DISK_SLOT_COUNT: usize = 2;
/// On-disk magic.
pub const DISK_MAGIC: [u8; 4] = *b"GALF";
/// Inodes live in the pool. A sealed slot holds actors, shares, and the
/// inode map. Older images are refused.
pub const DISK_VERSION: u16 = 18;

pub use volume::{
    inode_address, map_address, map_ptr_offset, payload_len, read_actor, read_free,
    read_geometry_head, read_inode, read_map_ptrs, read_share, volume_limits, write_actor,
    write_free, write_geometry, write_inode, write_map_ptrs, write_share, Geometry, VolumeLimits,
    ACTOR_ON_DISK, DESK_COMPILE_FILES, FILE_CAP, FREE_EXTENTS, GEOMETRY_ON_DISK, INODES_PER_BLOCK,
    INODE_ON_DISK, MAP_PER_BLOCK, MAP_POINTERS, OBJECT_CAP, SHARE_ON_DISK,
};
/// Format ceiling. A live volume's object table is [`VolumeLimits::inode_slots`].
pub const OBJECT_SLOTS: usize = OBJECT_CAP as usize;
pub const ACTOR_SLOTS: usize = 32;
pub const SHARE_SLOTS: usize = 32;
/// File data block. One block is eight sectors of ciphertext plus a
/// private tag sector, so a torn write cannot share a sector with
/// another block.
pub const BLOCK_SIZE: usize = 4096;
/// Extents stored on one inode. A ninth extent is `NoResource`.
pub const EXTENTS_PER_FILE: usize = 8;
/// Format ceiling for one file. The live cap is [`VolumeLimits::file_max`].
pub const FILE_BYTES: u64 = FILE_CAP;
/// Sectors per stored block: one tag sector, then eight ciphertext sectors.
pub const BLOCK_STRIDE: usize = 9;
/// A volume smaller than this stays RAM-only.
pub const MIN_DISK_BYTES: usize = 8 * 1024 * 1024;
pub const NAME_CAP: usize = 64;
pub const ACTOR_NAME: usize = 32;
/// AEAD domain so an inode block cannot be swapped with a file block.
pub const DOMAIN_DATA: u8 = 1;
pub const DOMAIN_INODE: u8 = 2;
pub const DOMAIN_MAP: u8 = 3;

pub const DISK_HEADER: usize = 128;
pub const OBJECT_ON_DISK: usize = INODE_ON_DISK;
pub const PAYLOAD_LEN: usize = payload_len();
/// Byte offset of pool block `index`'s tag sector.
pub const fn block_tag_offset(index: u32) -> usize {
    DISK_SLOT_COUNT * DISK_SECTORS * SECTOR + index as usize * BLOCK_STRIDE * SECTOR
}
/// Byte offset of pool block `index`'s ciphertext.
pub const fn data_byte_offset(index: usize) -> usize {
    block_tag_offset(index as u32) + SECTOR
}
/// How many pool blocks fit on a disk of `disk_bytes`, after the two slots.
pub fn pool_blocks_for(disk_bytes: usize) -> u32 {
    volume_limits(disk_bytes as u64).pool_blocks
}

const _: () = assert!(DISK_HEADER + PAYLOAD_LEN <= DISK_SECTORS * SECTOR);
const _: () = assert!(OBJECT_ON_DISK == 192);
const _: () = assert!(FILE_CAP > 16 * 1024 * 1024 * 1024);

pub const KIND_EMPTY: u8 = 0;
pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;
pub const NO_PARENT: u32 = u32::MAX;
pub const NO_BLOCK: u32 = u32::MAX;

/// Bring-up volume passphrase (matches kernel `VOLUME_PASSPHRASE`).
pub const DEFAULT_VOLUME_PASSPHRASE: &[u8] = b"galfs";
/// Immortal boot actor name.
pub const ADMIN_NAME: &str = "admin";

/// Max issue lines a check will record.
pub const MAX_ISSUES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issue {
    NoValidSlot,
    BothSlotsCorrupt,
    MissingAdmin,
    BadActorRoot { actor: u16 },
    BadObjectKind { object: u32 },
    OrphanObject { object: u32 },
    BadParent { object: u32 },
    Cycle { object: u32 },
    BadActorRef { object: u32 },
    BlockLeak { block: u32 },
    BlockDuplicate { block: u32 },
    BlockMissing { block: u32 },
    FileTooLarge { object: u32 },
    DirHasLength { object: u32 },
    BadShare { share: u16 },
}

impl Issue {
    /// Short stable label for CLI / tests.
    pub fn label(self) -> &'static str {
        match self {
            Issue::NoValidSlot => "no_valid_slot",
            Issue::BothSlotsCorrupt => "both_slots_corrupt",
            Issue::MissingAdmin => "missing_admin",
            Issue::BadActorRoot { .. } => "bad_actor_root",
            Issue::BadObjectKind { .. } => "bad_object_kind",
            Issue::OrphanObject { .. } => "orphan_object",
            Issue::BadParent { .. } => "bad_parent",
            Issue::Cycle { .. } => "cycle",
            Issue::BadActorRef { .. } => "bad_actor_ref",
            Issue::BlockLeak { .. } => "block_leak",
            Issue::BlockDuplicate { .. } => "block_duplicate",
            Issue::BlockMissing { .. } => "block_missing",
            Issue::FileTooLarge { .. } => "file_too_large",
            Issue::DirHasLength { .. } => "dir_has_length",
            Issue::BadShare { .. } => "bad_share",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Actor {
    pub used: bool,
    pub name: [u8; ACTOR_NAME],
    pub name_len: u8,
    pub root: u32,
    pub salt: [u8; SALT_LEN],
    pub pass_hash: [u8; HASH_LEN],
    /// PBKDF2-HMAC-SHA256 iteration count used for `pass_hash`.
    pub kdf_iters: u32,
    pub max_objects: u32,
    pub max_bytes: u64,
}

impl Actor {
    pub const fn empty() -> Self {
        Self {
            used: false,
            name: [0; ACTOR_NAME],
            name_len: 0,
            root: NO_PARENT,
            salt: [0; SALT_LEN],
            pass_hash: [0; HASH_LEN],
            kdf_iters: 0,
            max_objects: 0,
            max_bytes: 0,
        }
    }

    pub fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

/// A run of data blocks. `start` is a pool index.
#[derive(Clone, Copy)]
pub struct Extent {
    pub start: u32,
    pub count: u32,
    /// Generation that sealed these blocks. Zero means the extent is empty.
    pub gen: u32,
}

impl Extent {
    pub const fn empty() -> Self {
        Self {
            start: 0,
            count: 0,
            gen: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Object {
    pub kind: u8,
    pub parent: u32,
    pub actor: u8,
    pub name: [u8; NAME_CAP],
    pub name_len: u8,
    pub len: u64,
    pub extent_count: u8,
    pub extents: [Extent; EXTENTS_PER_FILE],
    /// First child, when `kind` is a directory. [`NO_PARENT`] if none.
    pub child: u32,
    /// Next sibling under the same parent. [`NO_PARENT`] if none.
    pub next: u32,
}

impl Object {
    pub const fn empty() -> Self {
        Self {
            kind: KIND_EMPTY,
            parent: NO_PARENT,
            actor: 0,
            name: [0; NAME_CAP],
            name_len: 0,
            len: 0,
            extent_count: 0,
            extents: [Extent::empty(); EXTENTS_PER_FILE],
            child: NO_PARENT,
            next: NO_PARENT,
        }
    }
}

/// Durable home share: object + rights re-applied at grantee's login.
#[derive(Clone, Copy)]
pub struct Share {
    pub used: bool,
    pub rights: u8,
    pub grantee: u8,
    pub object: u32,
}

impl Share {
    pub const fn empty() -> Self {
        Self {
            used: false,
            rights: 0,
            grantee: 0,
            object: NO_PARENT,
        }
    }
}

/// Decrypted in-memory GALF table (large — host should heap-allocate).
///
/// File bytes are not stored here. Each data block lives in the region
/// after the two metadata slots.
pub struct Table {
    pub actors: [Actor; ACTOR_SLOTS],
    pub objects: Vec<Object>,
    pub shares: [Share; SHARE_SLOTS],
    pub geo: volume::Geometry,
}

impl Table {
    pub fn empty() -> Self {
        Self {
            actors: [Actor::empty(); ACTOR_SLOTS],
            objects: Vec::new(),
            shares: [Share::empty(); SHARE_SLOTS],
            geo: volume::Geometry::empty(),
        }
    }

    pub fn pool_blocks(&self) -> u32 {
        self.geo.pool_blocks
    }
}

/// Result of checking a dual-slot image.
pub struct Report {
    pub ok: bool,
    pub slot: Option<u32>,
    pub generation: Option<u64>,
    pub issue_count: usize,
    pub issues: [Issue; MAX_ISSUES],
}

impl Report {
    fn push(&mut self, issue: Issue) {
        self.ok = false;
        if self.issue_count < MAX_ISSUES {
            self.issues[self.issue_count] = issue;
            self.issue_count += 1;
        }
    }
}

/// A v13 image is at least [`v13::MIN_DISK_BYTES`] and one metadata slot
/// still carries magic, version 13, and a 4 KiB block size.
fn is_v13_image(image: &[u8]) -> bool {
    if image.len() < v13::MIN_DISK_BYTES {
        return false;
    }
    for slot in 0..v13::SLOT_COUNT {
        let off = slot * v13::META_BYTES;
        if off + 8 > image.len() {
            break;
        }
        if image[off..off + 4] != DISK_MAGIC {
            continue;
        }
        let version = u16::from_le_bytes([image[off + 4], image[off + 5]]);
        let block = u16::from_le_bytes([image[off + 6], image[off + 7]]) as usize;
        if version == v13::VERSION && block == v13::BLOCK {
            return true;
        }
    }
    false
}

/// Unlock the newest valid sealed slot in `image` and run structural checks.
///
/// `image` must be at least [`DISK_SLOT_COUNT`] × [`DISK_SECTORS`] × [`SECTOR`]
/// bytes. Callers supply heap (or static) scratch: `slot_buf` length
/// [`DISK_SECTORS`]×[`SECTOR`], plus `best`/`cand` tables (too large for
/// typical stacks).
pub fn check_image(
    image: &[u8],
    passphrase: &[u8],
    slot_buf: &mut [u8],
    best: &mut Table,
    cand: &mut Table,
) -> Report {
    let mut report = Report {
        ok: true,
        slot: None,
        generation: None,
        issue_count: 0,
        issues: [Issue::NoValidSlot; MAX_ISSUES],
    };
    // v13 disks are at least 8 MiB and name a 4 KiB block in the header.
    // A smaller image, including a v12 slot whose version byte was patched
    // to 13, stays on the v12 path (foreign versions are ignored, not
    // corrupt). Either metadata slot may be the one that still says v13.
    if is_v13_image(image) {
        return v13::check(image, passphrase);
    }
    let need = DISK_SLOT_COUNT * DISK_SECTORS * SECTOR;
    if image.len() < need || slot_buf.len() < DISK_SECTORS * SECTOR {
        report.push(Issue::NoValidSlot);
        return report;
    }
    let scratch = &mut slot_buf[..DISK_SECTORS * SECTOR];

    let mut best_gen = 0u64;
    let mut best_slot: Option<u32> = None;
    let mut saw_corrupt_current = false;

    for slot in 0..DISK_SLOT_COUNT as u32 {
        let off = slot as usize * DISK_SECTORS * SECTOR;
        scratch.copy_from_slice(&image[off..off + DISK_SECTORS * SECTOR]);
        let magic = scratch[0..4] == DISK_MAGIC;
        let version = if magic {
            u16::from_le_bytes([scratch[4], scratch[5]])
        } else {
            0
        };
        if magic && version != DISK_VERSION {
            continue;
        }
        match decode_slot(scratch, passphrase, cand) {
            Some(decoded) => {
                let inodes_ok = load_inodes(image, cand, &decoded.volume_key);
                let blocks_ok = inodes_ok && verify_data_blocks(image, cand, &decoded.volume_key);
                let mut key = decoded.volume_key;
                wipe_bytes(&mut key);
                if !blocks_ok {
                    saw_corrupt_current = true;
                    continue;
                }
                if best_slot.is_none() || decoded.generation >= best_gen {
                    best_gen = decoded.generation;
                    best_slot = Some(slot);
                    copy_table(cand, best);
                }
            }
            None => {
                if magic && version == DISK_VERSION {
                    saw_corrupt_current = true;
                }
            }
        }
    }

    let Some(slot) = best_slot else {
        report.push(if saw_corrupt_current {
            Issue::BothSlotsCorrupt
        } else {
            Issue::NoValidSlot
        });
        return report;
    };
    report.slot = Some(slot);
    report.generation = Some(best_gen);
    collect_issues(best, &mut report);
    if report.issue_count == 0 {
        report.ok = true;
    }
    report
}

/// What [`repair_image`] did to the inactive slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repair {
    /// Slot that now holds the corrected metadata.
    pub slot: u32,
    /// Generation sealed into that slot. It is one past the slot that was read.
    pub generation: u64,
    /// Share rows cleared. File bytes and inode blocks are not rewritten.
    pub cleared_shares: u16,
}

/// Why [`repair_image`] left the image as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairStop {
    /// Neither slot decoded.
    NoValidSlot,
    /// A current-version slot was present and neither slot decoded.
    BothSlotsCorrupt,
    /// The newest slot has a problem this repair does not touch.
    Unrepairable,
    /// The corrected slot could not be sealed.
    Encode,
}

/// Clears bad share rows and seals the result into the other metadata slot.
///
/// A bad share names a missing actor, an empty object, or no rights. The
/// previous slot stays. A torn copy of the new slot fails to decode, so
/// the next check still selects the older generation. Any other issue
/// (a cycle, a missing block, a missing admin) is left in place: this
/// function does not invent a tree or file bytes.
///
/// `Ok(None)` means the newest slot was already clean and nothing was written.
pub fn repair_image(
    image: &mut [u8],
    passphrase: &[u8],
    nonces: &SealNonces,
    slot_buf: &mut [u8],
    best: &mut Table,
    cand: &mut Table,
) -> Result<Option<Repair>, RepairStop> {
    let slot_len = DISK_SECTORS * SECTOR;
    if image.len() < DISK_SLOT_COUNT * slot_len || slot_buf.len() < slot_len {
        return Err(RepairStop::NoValidSlot);
    }
    let scratch = &mut slot_buf[..slot_len];
    let mut best_gen = 0u64;
    let mut best_slot: Option<u32> = None;
    let mut best_key = [0u8; KEY_LEN];
    let mut saw_corrupt_current = false;

    for slot in 0..DISK_SLOT_COUNT as u32 {
        let off = slot as usize * slot_len;
        scratch.copy_from_slice(&image[off..off + slot_len]);
        let magic = scratch[0..4] == DISK_MAGIC;
        let version = if magic {
            u16::from_le_bytes([scratch[4], scratch[5]])
        } else {
            0
        };
        if magic && version != DISK_VERSION {
            continue;
        }
        match decode_slot(scratch, passphrase, cand) {
            Some(decoded) => {
                let inodes_ok = load_inodes(image, cand, &decoded.volume_key);
                let blocks_ok = inodes_ok && verify_data_blocks(image, cand, &decoded.volume_key);
                if !blocks_ok {
                    let mut key = decoded.volume_key;
                    wipe_bytes(&mut key);
                    saw_corrupt_current = true;
                    continue;
                }
                if best_slot.is_none() || decoded.generation >= best_gen {
                    wipe_bytes(&mut best_key);
                    best_gen = decoded.generation;
                    best_slot = Some(slot);
                    best_key = decoded.volume_key;
                    copy_table(cand, best);
                } else {
                    let mut key = decoded.volume_key;
                    wipe_bytes(&mut key);
                }
            }
            None => {
                if magic && version == DISK_VERSION {
                    saw_corrupt_current = true;
                }
            }
        }
    }

    let Some(slot) = best_slot else {
        wipe_bytes(&mut best_key);
        return Err(if saw_corrupt_current {
            RepairStop::BothSlotsCorrupt
        } else {
            RepairStop::NoValidSlot
        });
    };

    let mut report = Report {
        ok: true,
        slot: Some(slot),
        generation: Some(best_gen),
        issue_count: 0,
        issues: [Issue::NoValidSlot; MAX_ISSUES],
    };
    collect_issues(best, &mut report);
    if report.issue_count == 0 {
        wipe_bytes(&mut best_key);
        return Ok(None);
    }
    let only_shares = report.issues[..report.issue_count]
        .iter()
        .all(|issue| matches!(issue, Issue::BadShare { .. }));
    if !only_shares {
        wipe_bytes(&mut best_key);
        return Err(RepairStop::Unrepairable);
    }

    let mut bad = [false; SHARE_SLOTS];
    for (i, share) in best.shares.iter().enumerate() {
        bad[i] = share_is_bad(best, share);
    }
    let mut cleared = 0u16;
    for (share, drop) in best.shares.iter_mut().zip(bad) {
        if drop {
            *share = Share::empty();
            cleared = cleared.saturating_add(1);
        }
    }
    if cleared == 0 || !table_ok(best) {
        wipe_bytes(&mut best_key);
        return Err(RepairStop::Unrepairable);
    }
    let Some(generation) = best_gen.checked_add(1) else {
        wipe_bytes(&mut best_key);
        return Err(RepairStop::Encode);
    };
    let flat = &mut slot_buf[..slot_len];
    if !encode_slot(best, generation, passphrase, &best_key, nonces, flat) {
        wipe_bytes(&mut best_key);
        return Err(RepairStop::Encode);
    }
    wipe_bytes(&mut best_key);
    let dest = (slot ^ 1) as usize * slot_len;
    image[dest..dest + slot_len].copy_from_slice(&flat[..slot_len]);
    Ok(Some(Repair {
        slot: slot ^ 1,
        generation,
        cleared_shares: cleared,
    }))
}

fn share_is_bad(table: &Table, share: &Share) -> bool {
    if !share.used {
        return false;
    }
    share.rights == 0
        || share.grantee as usize >= ACTOR_SLOTS
        || !table.actors[share.grantee as usize].used
        || share.object as usize >= table.objects.len()
        || table.objects[share.object as usize].kind == KIND_EMPTY
}

fn copy_table(src: &Table, dst: &mut Table) {
    dst.actors = src.actors;
    dst.shares = src.shares;
    dst.geo = src.geo;
    dst.objects.clone_from(&src.objects);
}

fn seal_aad(generation: u64) -> [u8; 14] {
    let mut aad = [0u8; 14];
    aad[0..4].copy_from_slice(&DISK_MAGIC);
    aad[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    aad[6..14].copy_from_slice(&generation.to_le_bytes());
    aad
}

/// Nonce for data block `index` allocated at `gen`. Unique per write
/// because a rewritten block gets a new generation (and, while either
/// metadata slot still names the old block, a new index).
pub fn block_nonce(domain: u8, index: usize, gen: u32) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[0] = domain;
    nonce[1..5].copy_from_slice(&(index as u32).to_le_bytes());
    nonce[5..9].copy_from_slice(&gen.to_le_bytes());
    nonce
}

/// AAD binding a pool block to this GALF version, its domain, index, and generation.
pub fn block_aad(domain: u8, index: usize, gen: u32) -> [u8; 15] {
    let mut aad = [0u8; 15];
    aad[0..4].copy_from_slice(&DISK_MAGIC);
    aad[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    aad[6] = domain;
    aad[7..11].copy_from_slice(&(index as u32).to_le_bytes());
    aad[11..15].copy_from_slice(&gen.to_le_bytes());
    aad
}

/// Encrypts one plaintext pool block in place. `gen` must be non-zero.
pub fn seal_block(
    volume_key: &[u8; KEY_LEN],
    domain: u8,
    index: usize,
    gen: u32,
    block: &mut [u8],
    tag: &mut [u8; TAG_LEN],
) -> bool {
    if block.len() != BLOCK_SIZE || gen == 0 {
        return false;
    }
    let nonce = block_nonce(domain, index, gen);
    let aad = block_aad(domain, index, gen);
    seal(volume_key, &nonce, &aad, block, tag);
    true
}

/// Decrypts one pool block in place. Returns false on a bad tag or generation.
pub fn open_block(
    volume_key: &[u8; KEY_LEN],
    domain: u8,
    index: usize,
    gen: u32,
    block: &mut [u8],
    tag: &[u8; TAG_LEN],
) -> bool {
    if block.len() != BLOCK_SIZE || gen == 0 {
        return false;
    }
    let nonce = block_nonce(domain, index, gen);
    let aad = block_aad(domain, index, gen);
    open(volume_key, &nonce, &aad, block, tag)
}

/// One opened metadata slot. The caller wipes [`DecodedSlot::volume_key`].
pub struct DecodedSlot {
    pub generation: u64,
    pub volume_key: [u8; KEY_LEN],
}

/// Extent bounds only. File ciphertext is checked on read, not at mount:
/// a multi-gigabyte file is not decrypted to prove the slot.
pub fn verify_data_blocks(image: &[u8], table: &Table, volume_key: &[u8; KEY_LEN]) -> bool {
    let _ = (image, volume_key);
    for obj in &table.objects {
        if obj.kind != KIND_FILE {
            continue;
        }
        for ext in obj.extents.iter().take(obj.extent_count as usize) {
            let end = ext.start.saturating_add(ext.count);
            if ext.count == 0 || ext.gen == 0 || end > table.pool_blocks() {
                return false;
            }
        }
    }
    true
}

/// Decrypts one stored data block. The caller wipes nothing; plaintext
/// dies with the local buffer.
pub fn open_stored_block(image: &[u8], index: u32, volume_key: &[u8; KEY_LEN]) -> bool {
    let tag_off = block_tag_offset(index);
    let data_off = data_byte_offset(index as usize);
    if image.len() < data_off + BLOCK_SIZE || tag_off + 20 > image.len() {
        return false;
    }
    let gen = u32::from_le_bytes(image[tag_off..tag_off + 4].try_into().unwrap_or([0; 4]));
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&image[tag_off + 4..tag_off + 4 + TAG_LEN]);
    let mut block = [0u8; BLOCK_SIZE];
    block.copy_from_slice(&image[data_off..data_off + BLOCK_SIZE]);
    let ok = open_block(
        volume_key,
        DOMAIN_DATA,
        index as usize,
        gen,
        &mut block,
        &tag,
    );
    wipe_bytes(&mut block);
    ok
}

/// Randomness one sealed slot consumes. The kernel draws these from
/// `arch::rand`; tests pass fixed bytes so images reproduce.
#[derive(Clone, Copy)]
pub struct SealNonces {
    pub kdf_salt: [u8; SALT_LEN],
    pub wrap_nonce: [u8; NONCE_LEN],
    pub data_nonce: [u8; NONCE_LEN],
}

/// Serialises and seals `table` at `generation` into `flat` (one slot,
/// at least `DISK_HEADER + PAYLOAD_LEN` bytes; the rest is zeroed).
///
/// The slot holds actors, shares, quotas, the free list, and the inode
/// map. Inode records themselves live in pool blocks. Returns `false`
/// when `flat` is too short.
pub fn encode_slot(
    table: &Table,
    generation: u64,
    passphrase: &[u8],
    volume_key: &[u8; KEY_LEN],
    nonces: &SealNonces,
    flat: &mut [u8],
) -> bool {
    if flat.len() < DISK_HEADER + PAYLOAD_LEN {
        return false;
    }
    flat.fill(0);
    flat[0..4].copy_from_slice(&DISK_MAGIC);
    flat[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    flat[6..8].copy_from_slice(&(ACTOR_SLOTS as u16).to_le_bytes());
    flat[8..10].copy_from_slice(&(SHARE_SLOTS as u16).to_le_bytes());
    flat[14] = 1; // sealed
    flat[16..24].copy_from_slice(&generation.to_le_bytes());

    write_geometry(
        &mut flat[DISK_HEADER..DISK_HEADER + GEOMETRY_ON_DISK],
        &table.geo,
    );
    let mut off = DISK_HEADER + GEOMETRY_ON_DISK;
    for actor in &table.actors {
        write_actor(&mut flat[off..off + ACTOR_ON_DISK], actor);
        off += ACTOR_ON_DISK;
    }
    for share in &table.shares {
        write_share(&mut flat[off..off + SHARE_ON_DISK], share);
        off += SHARE_ON_DISK;
    }
    write_free(&mut flat[off..off + FREE_EXTENTS * 12], &table.geo.free);
    off += FREE_EXTENTS * 12;
    write_map_ptrs(
        &mut flat[off..off + MAP_POINTERS * 8],
        &table.geo.map_at,
        &table.geo.map_gen,
    );
    debug_assert_eq!(off + MAP_POINTERS * 8, DISK_HEADER + PAYLOAD_LEN);

    let aad = seal_aad(generation);
    let mut kek = [0u8; KEY_LEN];
    derive_key(passphrase, &nonces.kdf_salt, &mut kek);
    let mut wrapped = *volume_key;
    let mut wrap_tag = [0u8; TAG_LEN];
    seal(&kek, &nonces.wrap_nonce, &aad, &mut wrapped, &mut wrap_tag);
    wipe_bytes(&mut kek);
    flat[32..40].copy_from_slice(&nonces.kdf_salt);
    flat[40..52].copy_from_slice(&nonces.wrap_nonce);
    flat[52..84].copy_from_slice(&wrapped);
    flat[84..100].copy_from_slice(&wrap_tag);
    wipe_bytes(&mut wrapped);

    let mut data_tag = [0u8; TAG_LEN];
    seal(
        volume_key,
        &nonces.data_nonce,
        &aad,
        &mut flat[DISK_HEADER..DISK_HEADER + PAYLOAD_LEN],
        &mut data_tag,
    );
    flat[100..112].copy_from_slice(&nonces.data_nonce);
    flat[112..128].copy_from_slice(&data_tag);

    let sum = crc32(&flat[DISK_HEADER..DISK_HEADER + PAYLOAD_LEN]);
    flat[24..28].copy_from_slice(&sum.to_le_bytes());
    true
}

/// Decrypt one slot buffer into `table`. File bytes stay in the data region;
/// [`verify_data_blocks`] checks them. The returned key must be wiped.
pub fn decode_slot(flat: &mut [u8], passphrase: &[u8], table: &mut Table) -> Option<DecodedSlot> {
    if flat.len() < DISK_HEADER + PAYLOAD_LEN {
        return None;
    }
    if flat[0..4] != DISK_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([flat[4], flat[5]]);
    let actors = u16::from_le_bytes([flat[6], flat[7]]) as usize;
    let shares = u16::from_le_bytes([flat[8], flat[9]]) as usize;
    if version != DISK_VERSION
        || actors != ACTOR_SLOTS
        || shares != SHARE_SLOTS
        || flat[14] & 1 == 0
    {
        return None;
    }
    let generation = u64::from_le_bytes(flat[16..24].try_into().ok()?);
    let expect = u32::from_le_bytes(flat[24..28].try_into().ok()?);
    if crc32(&flat[DISK_HEADER..DISK_HEADER + PAYLOAD_LEN]) != expect {
        return None;
    }

    let aad = seal_aad(generation);
    let kdf_salt: [u8; SALT_LEN] = flat[32..40].try_into().ok()?;
    let wrap_nonce: [u8; NONCE_LEN] = flat[40..52].try_into().ok()?;
    let mut wrapped: [u8; KEY_LEN] = flat[52..84].try_into().ok()?;
    let wrap_tag: [u8; TAG_LEN] = flat[84..100].try_into().ok()?;
    let data_nonce: [u8; NONCE_LEN] = flat[100..112].try_into().ok()?;
    let data_tag: [u8; TAG_LEN] = flat[112..128].try_into().ok()?;

    let mut kek = [0u8; KEY_LEN];
    derive_key(passphrase, &kdf_salt, &mut kek);
    if !open(&kek, &wrap_nonce, &aad, &mut wrapped, &wrap_tag) {
        wipe_bytes(&mut kek);
        wipe_bytes(&mut wrapped);
        return None;
    }
    wipe_bytes(&mut kek);
    let vk = wrapped;

    if !open(
        &vk,
        &data_nonce,
        &aad,
        &mut flat[DISK_HEADER..DISK_HEADER + PAYLOAD_LEN],
        &data_tag,
    ) {
        let mut gone = vk;
        wipe_bytes(&mut gone);
        return None;
    }

    let geo = read_geometry_head(&flat[DISK_HEADER..DISK_HEADER + GEOMETRY_ON_DISK])?;
    table.geo = geo;
    let mut off = DISK_HEADER + GEOMETRY_ON_DISK;
    for actor in &mut table.actors {
        *actor = read_actor(&flat[off..off + ACTOR_ON_DISK])?;
        off += ACTOR_ON_DISK;
    }
    for share in &mut table.shares {
        *share = read_share(&flat[off..off + SHARE_ON_DISK])?;
        off += SHARE_ON_DISK;
    }
    if !read_free(&flat[off..off + FREE_EXTENTS * 12], &mut table.geo.free) {
        let mut gone = vk;
        wipe_bytes(&mut gone);
        return None;
    }
    off += FREE_EXTENTS * 12;
    if !read_map_ptrs(
        &flat[off..off + MAP_POINTERS * 8],
        &mut table.geo.map_at,
        &mut table.geo.map_gen,
    ) {
        let mut gone = vk;
        wipe_bytes(&mut gone);
        return None;
    }
    table.objects.clear();
    Some(DecodedSlot {
        generation,
        volume_key: vk,
    })
}

/// Reads inode blocks named by the superblock map into `table.objects`.
pub fn load_inodes(image: &[u8], table: &mut Table, volume_key: &[u8; KEY_LEN]) -> bool {
    let slots = table.geo.inode_slots as usize;
    if slots == 0 || slots > OBJECT_CAP as usize {
        return false;
    }
    table.objects.clear();
    table.objects.resize(slots, Object::empty());
    let inode_blocks = slots.div_ceil(INODES_PER_BLOCK);
    let map_len = table.geo.map_len as usize;
    if map_len == 0 || map_len > MAP_POINTERS {
        return false;
    }
    for m in 0..map_len {
        let gen = table.geo.map_gen[m];
        let index = table.geo.map_at[m];
        if gen == 0 {
            continue;
        }
        let Some(map_plain) = open_pool_block(image, DOMAIN_MAP, index, gen, volume_key) else {
            return false;
        };
        let entries = BLOCK_SIZE / 8;
        for e in 0..entries {
            let ib = m * MAP_PER_BLOCK + e;
            if ib >= inode_blocks {
                break;
            }
            let off = e * 8;
            let at = u32::from_le_bytes(map_plain[off..off + 4].try_into().unwrap_or([0; 4]));
            let igen = u32::from_le_bytes(map_plain[off + 4..off + 8].try_into().unwrap_or([0; 4]));
            if igen == 0 {
                continue;
            }
            let Some(plain) = open_pool_block(image, DOMAIN_INODE, at, igen, volume_key) else {
                return false;
            };
            for n in 0..INODES_PER_BLOCK {
                let id = ib * INODES_PER_BLOCK + n;
                if id >= slots {
                    break;
                }
                let ioff = n * INODE_ON_DISK;
                let Some(obj) = read_inode(&plain[ioff..ioff + INODE_ON_DISK]) else {
                    return false;
                };
                table.objects[id] = obj;
            }
        }
    }
    true
}

fn open_pool_block(
    image: &[u8],
    domain: u8,
    index: u32,
    gen: u32,
    volume_key: &[u8; KEY_LEN],
) -> Option<[u8; BLOCK_SIZE]> {
    let tag_off = block_tag_offset(index);
    let data_off = data_byte_offset(index as usize);
    if gen == 0 || image.len() < data_off + BLOCK_SIZE || tag_off + 20 > image.len() {
        return None;
    }
    let stored_gen = u32::from_le_bytes(image[tag_off..tag_off + 4].try_into().unwrap_or([0; 4]));
    if stored_gen != gen {
        return None;
    }
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&image[tag_off + 4..tag_off + 4 + TAG_LEN]);
    let mut block = [0u8; BLOCK_SIZE];
    block.copy_from_slice(&image[data_off..data_off + BLOCK_SIZE]);
    if !open_block(volume_key, domain, index as usize, gen, &mut block, &tag) {
        return None;
    }
    Some(block)
}

fn collect_issues(table: &Table, report: &mut Report) {
    let mut saw_admin = false;
    for (ai, actor) in table.actors.iter().enumerate() {
        if !actor.used {
            continue;
        }
        if actor.name_len == 0 || actor.name_len as usize > ACTOR_NAME {
            report.push(Issue::BadActorRoot { actor: ai as u16 });
            continue;
        }
        let root = actor.root as usize;
        if root >= table.objects.len() {
            report.push(Issue::BadActorRoot { actor: ai as u16 });
            continue;
        }
        let obj = &table.objects[root];
        if obj.kind != KIND_DIR || obj.parent != NO_PARENT || obj.actor != ai as u8 {
            report.push(Issue::BadActorRoot { actor: ai as u16 });
        }
        if actor.name_is(ADMIN_NAME) {
            saw_admin = true;
        }
    }
    if !saw_admin {
        report.push(Issue::MissingAdmin);
    }

    for (si, share) in table.shares.iter().enumerate() {
        if share_is_bad(table, share) {
            report.push(Issue::BadShare { share: si as u16 });
        }
    }

    let words = (table.pool_blocks() as usize).div_ceil(64);
    let mut seen = alloc::vec![0u64; words];
    let n = table.objects.len();
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.kind != KIND_FILE && obj.kind != KIND_DIR {
            report.push(Issue::BadObjectKind { object: i as u32 });
            continue;
        }
        if obj.actor as usize >= ACTOR_SLOTS || !table.actors[obj.actor as usize].used {
            report.push(Issue::BadActorRef { object: i as u32 });
        }
        if obj.parent == NO_PARENT {
            if !table.actors.iter().any(|a| a.used && a.root == i as u32) {
                report.push(Issue::OrphanObject { object: i as u32 });
            }
        } else {
            let parent = obj.parent as usize;
            if parent >= n || parent == i || table.objects[parent].kind != KIND_DIR {
                report.push(Issue::BadParent { object: i as u32 });
            } else {
                let mut cur = obj.parent;
                for _ in 0..n {
                    if cur == i as u32 {
                        report.push(Issue::Cycle { object: i as u32 });
                        break;
                    }
                    let p = table.objects[cur as usize].parent;
                    if p == NO_PARENT {
                        break;
                    }
                    if p as usize >= n {
                        report.push(Issue::BadParent { object: i as u32 });
                        break;
                    }
                    cur = p;
                }
            }
        }
        check_object_blocks(table, obj, i as u32, &mut seen, report);
    }
    check_directories(table, report);
}

/// Directory membership is a sibling list on the parent, not a scan of
/// every object. The list and the parent pointers must name the same set.
fn check_directories(table: &Table, report: &mut Report) {
    let n = table.objects.len();
    let mut seen = alloc::vec![false; n];
    for i in 0..n {
        let obj = &table.objects[i];
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.kind != KIND_DIR && obj.child != NO_PARENT {
            report.push(Issue::BadParent { object: i as u32 });
        }
        if obj.kind != KIND_DIR {
            continue;
        }
        let mut child = obj.child;
        let mut guard = 0usize;
        while child != NO_PARENT {
            if child as usize >= n || seen[child as usize] {
                report.push(Issue::BadParent { object: i as u32 });
                break;
            }
            seen[child as usize] = true;
            let entry = &table.objects[child as usize];
            if entry.kind == KIND_EMPTY || entry.parent != i as u32 {
                report.push(Issue::BadParent { object: child });
                break;
            }
            child = entry.next;
            guard += 1;
            if guard > n {
                report.push(Issue::Cycle { object: i as u32 });
                break;
            }
        }
    }
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.parent == NO_PARENT {
            if obj.next != NO_PARENT {
                report.push(Issue::BadParent { object: i as u32 });
            }
            continue;
        }
        if !seen[i] {
            report.push(Issue::OrphanObject { object: i as u32 });
        }
    }
}

fn mark_seen(seen: &mut [u64], block: u32, pool: u32, report: &mut Report) {
    if block >= pool {
        report.push(Issue::BlockMissing { block });
        return;
    }
    let i = block as usize;
    let bit = 1u64 << (i % 64);
    if seen[i / 64] & bit != 0 {
        report.push(Issue::BlockDuplicate { block });
    }
    seen[i / 64] |= bit;
}

fn check_object_blocks(
    table: &Table,
    obj: &Object,
    index: u32,
    seen: &mut [u64],
    report: &mut Report,
) {
    if obj.kind != KIND_FILE {
        if obj.len != 0 || obj.extent_count != 0 {
            report.push(Issue::DirHasLength { object: index });
        }
        return;
    }
    if obj.len > table.geo.file_max || obj.len > FILE_CAP {
        report.push(Issue::FileTooLarge { object: index });
        return;
    }
    if obj.extent_count as usize > EXTENTS_PER_FILE {
        report.push(Issue::BlockMissing { block: index });
        return;
    }
    let need = obj.len.div_ceil(BLOCK_SIZE as u64);
    let mut covered = 0u64;
    for ext in obj.extents.iter().take(obj.extent_count as usize) {
        if ext.count == 0 || ext.gen == 0 {
            report.push(Issue::BlockMissing { block: index });
            continue;
        }
        covered += u64::from(ext.count);
        let mut b = ext.start;
        for _ in 0..ext.count {
            mark_seen(seen, b, table.pool_blocks(), report);
            b = b.saturating_add(1);
        }
    }
    if covered < need {
        report.push(Issue::BlockMissing { block: index });
    }
}

/// True when `table` has no structural issues (same bar as kernel `validate_table`).
pub fn table_ok(table: &Table) -> bool {
    let mut report = Report {
        ok: true,
        slot: None,
        generation: None,
        issue_count: 0,
        issues: [Issue::NoValidSlot; MAX_ISSUES],
    };
    collect_issues(table, &mut report);
    report.issue_count == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::boxed::Box;
    use std::vec;

    #[test]
    fn empty_image_reports_no_valid_slot() {
        // Heap via Box — Table is ~144 KiB.
        let image = vec![0u8; DISK_SLOT_COUNT * DISK_SECTORS * SECTOR];
        let mut slot_buf = vec![0u8; DISK_SECTORS * SECTOR];
        let mut best = Box::new(Table::empty());
        let mut cand = Box::new(Table::empty());
        let report = check_image(
            &image,
            DEFAULT_VOLUME_PASSPHRASE,
            &mut slot_buf,
            &mut best,
            &mut cand,
        );
        assert!(!report.ok);
        assert_eq!(report.issues[0], Issue::NoValidSlot);
    }

    #[test]
    fn layout_constants_fit_slot() {
        // Slot fit is a `const _` assertion at the top of the crate.
        assert_eq!(ACTOR_ON_DISK, 80);
        assert_eq!(OBJECT_ON_DISK, 192);
        assert_eq!(SHARE_ON_DISK, 8);
        assert_eq!(SHARE_SLOTS, 32);
        assert_eq!(DISK_VERSION, 18);
        assert_eq!(DESK_COMPILE_FILES, 41);
        assert!(
            volume_limits(MIN_DISK_BYTES as u64).inode_slots >= DESK_COMPILE_FILES,
            "the 8 MiB login image must name a desk compile"
        );
        assert!(pool_blocks_for(MIN_DISK_BYTES) > 256);
        let big = volume_limits(32 * 1024 * 1024 * 1024);
        assert!(big.inode_slots > 1_000_000, "slots {}", big.inode_slots);
        assert!(
            big.file_max > 16 * 1024 * 1024 * 1024,
            "file {}",
            big.file_max
        );
        let mut wide = Object::empty();
        wide.kind = KIND_FILE;
        wide.len = 20 * 1024 * 1024 * 1024;
        wide.extent_count = 1;
        wide.extents[0] = Extent {
            start: 0,
            count: (wide.len / BLOCK_SIZE as u64) as u32,
            gen: 1,
        };
        let mut raw = [0u8; OBJECT_ON_DISK];
        write_inode(&mut raw, &wide);
        let back = read_inode(&raw).expect("inode");
        assert_eq!(back.len, wide.len);
    }
}
