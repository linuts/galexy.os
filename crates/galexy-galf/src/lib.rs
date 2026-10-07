//! Shared GALF on-disk layout, unlock, and structural checks.
//!
//! Kernel and host tools must agree on these constants (STYLE: no
//! duplicated magic). The host `galfs-fsck` binary walks a dual-slot
//! image; the kernel keeps its live table path but must match versions.

#![no_std]
#![deny(clippy::all)]

use galexy_core::crc32;
use galexy_crypto::{
    derive_key, open, wipe_bytes, KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN, HASH_LEN,
};

/// ATA / GALF sector size.
pub const SECTOR: usize = 512;
/// Dual-slot image sectors per slot (must cover header + sealed payload).
pub const DISK_SECTORS: usize = 288;
pub const DISK_SLOT_COUNT: usize = 2;
/// On-disk magic.
pub const DISK_MAGIC: [u8; 4] = *b"GALF";
/// Current sealed layout (quotas + shares + single-indirect blocks).
pub const DISK_VERSION: u16 = 11;

pub const OBJECT_SLOTS: usize = 128;
pub const ACTOR_SLOTS: usize = 32;
pub const SHARE_SLOTS: usize = 32;
pub const BLOCK_SIZE: usize = 512;
pub const DIRECT_BLOCKS: usize = 8;
/// u16 pointers in one single-indirect block.
pub const INDIRECT_PTRS: usize = BLOCK_SIZE / 2;
/// 8 directs + single indirect; file `len` stays u16.
pub const FILE_BYTES: usize = 32 * 1024;
pub const BLOCK_SLOTS: usize = 256;
pub const BITMAP_BYTES: usize = BLOCK_SLOTS / 8;
pub const NAME_CAP: usize = 64;
pub const ACTOR_NAME: usize = 32;

pub const DISK_HEADER: usize = 128;
/// used + name_len + name + root + salt + hash + max_objects + max_bytes.
pub const ACTOR_ON_DISK: usize = 66;
/// kind+actor+name_len+pad + parent+len + name + directs + indirect.
pub const OBJECT_ON_DISK: usize = 8 + NAME_CAP + DIRECT_BLOCKS * 2 + 2;
/// used + rights + grantee + pad + object.
pub const SHARE_ON_DISK: usize = 6;
pub const PAYLOAD_LEN: usize = ACTOR_ON_DISK * ACTOR_SLOTS
    + OBJECT_ON_DISK * OBJECT_SLOTS
    + SHARE_ON_DISK * SHARE_SLOTS
    + BITMAP_BYTES
    + BLOCK_SLOTS * BLOCK_SIZE;

const _: () = assert!(DISK_HEADER + PAYLOAD_LEN <= DISK_SECTORS * SECTOR);

pub const KIND_EMPTY: u8 = 0;
pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;
pub const NO_PARENT: u16 = 0xffff;
pub const NO_BLOCK: u16 = 0xffff;

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
    BadObjectKind { object: u16 },
    OrphanObject { object: u16 },
    BadParent { object: u16 },
    Cycle { object: u16 },
    BadActorRef { object: u16 },
    BlockLeak { block: u16 },
    BlockDuplicate { block: u16 },
    BlockMissing { block: u16 },
    FileTooLarge { object: u16 },
    DirHasLength { object: u16 },
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
    pub root: u16,
    pub salt: [u8; SALT_LEN],
    pub pass_hash: [u8; HASH_LEN],
    pub max_objects: u16,
    pub max_bytes: u32,
}

impl Actor {
    pub const fn empty() -> Self {
        Self {
            used: false,
            name: [0; ACTOR_NAME],
            name_len: 0,
            root: 0xffff,
            salt: [0; SALT_LEN],
            pass_hash: [0; HASH_LEN],
            max_objects: 0,
            max_bytes: 0,
        }
    }

    pub fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

#[derive(Clone, Copy)]
pub struct Object {
    pub kind: u8,
    pub parent: u16,
    pub actor: u8,
    pub name: [u8; NAME_CAP],
    pub name_len: u8,
    pub blocks: [u16; DIRECT_BLOCKS],
    pub indirect: u16,
    pub len: u16,
}

impl Object {
    pub const fn empty() -> Self {
        Self {
            kind: KIND_EMPTY,
            parent: NO_PARENT,
            actor: 0,
            name: [0; NAME_CAP],
            name_len: 0,
            blocks: [NO_BLOCK; DIRECT_BLOCKS],
            indirect: NO_BLOCK,
            len: 0,
        }
    }
}

/// Durable home share: object + rights re-applied at grantee's login.
#[derive(Clone, Copy)]
pub struct Share {
    pub used: bool,
    pub rights: u8,
    pub grantee: u8,
    pub object: u16,
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
pub struct Table {
    pub actors: [Actor; ACTOR_SLOTS],
    pub objects: [Object; OBJECT_SLOTS],
    pub shares: [Share; SHARE_SLOTS],
    pub blocks: [[u8; BLOCK_SIZE]; BLOCK_SLOTS],
    pub bitmap: [u8; BITMAP_BYTES],
}

impl Table {
    pub const fn empty() -> Self {
        Self {
            actors: [Actor::empty(); ACTOR_SLOTS],
            objects: [Object::empty(); OBJECT_SLOTS],
            shares: [Share::empty(); SHARE_SLOTS],
            blocks: [[0u8; BLOCK_SIZE]; BLOCK_SLOTS],
            bitmap: [0u8; BITMAP_BYTES],
        }
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
            Some(gen) => {
                if best_slot.is_none() || gen >= best_gen {
                    best_gen = gen;
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

fn copy_table(src: &Table, dst: &mut Table) {
    // SAFETY: distinct owned tables; plain data.
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const Table, dst as *mut Table, 1);
    }
}

fn seal_aad(generation: u64) -> [u8; 14] {
    let mut aad = [0u8; 14];
    aad[0..4].copy_from_slice(&DISK_MAGIC);
    aad[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    aad[6..14].copy_from_slice(&generation.to_le_bytes());
    aad
}

/// Decrypt one slot buffer into `table`. Returns generation on success.
pub fn decode_slot(flat: &mut [u8], passphrase: &[u8], table: &mut Table) -> Option<u64> {
    if flat.len() < DISK_HEADER + PAYLOAD_LEN {
        return None;
    }
    if flat[0..4] != DISK_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([flat[4], flat[5]]);
    let actors = u16::from_le_bytes([flat[6], flat[7]]) as usize;
    let objects = u16::from_le_bytes([flat[8], flat[9]]) as usize;
    let file_bytes = u16::from_le_bytes([flat[10], flat[11]]) as usize;
    let block_slots = u16::from_le_bytes([flat[14], flat[15]]) as usize;
    if version != DISK_VERSION
        || actors != ACTOR_SLOTS
        || objects != OBJECT_SLOTS
        || file_bytes != FILE_BYTES
        || block_slots != BLOCK_SLOTS
        || flat[12] & 1 == 0
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

    let mut off = DISK_HEADER;
    for actor in &mut table.actors {
        *actor = Actor::empty();
        actor.used = flat[off] != 0;
        actor.name_len = flat[off + 1].min(ACTOR_NAME as u8);
        actor.name
            .copy_from_slice(&flat[off + 2..off + 2 + ACTOR_NAME]);
        actor.root = u16::from_le_bytes([
            flat[off + 2 + ACTOR_NAME],
            flat[off + 3 + ACTOR_NAME],
        ]);
        let salt_off = off + 4 + ACTOR_NAME;
        actor.salt
            .copy_from_slice(&flat[salt_off..salt_off + SALT_LEN]);
        actor
            .pass_hash
            .copy_from_slice(&flat[salt_off + SALT_LEN..salt_off + SALT_LEN + HASH_LEN]);
        let qoff = salt_off + SALT_LEN + HASH_LEN;
        actor.max_objects = u16::from_le_bytes([flat[qoff], flat[qoff + 1]]);
        actor.max_bytes = u32::from_le_bytes([
            flat[qoff + 2],
            flat[qoff + 3],
            flat[qoff + 4],
            flat[qoff + 5],
        ]);
        off += ACTOR_ON_DISK;
    }
    for obj in &mut table.objects {
        *obj = Object::empty();
        obj.kind = flat[off];
        obj.actor = flat[off + 1];
        obj.name_len = flat[off + 2].min(NAME_CAP as u8);
        obj.parent = u16::from_le_bytes([flat[off + 4], flat[off + 5]]);
        obj.len = u16::from_le_bytes([flat[off + 6], flat[off + 7]]);
        obj.name
            .copy_from_slice(&flat[off + 8..off + 8 + NAME_CAP]);
        let boff = off + 8 + NAME_CAP;
        for (i, blk) in obj.blocks.iter_mut().enumerate() {
            *blk = u16::from_le_bytes([flat[boff + i * 2], flat[boff + i * 2 + 1]]);
        }
        let ioff = boff + DIRECT_BLOCKS * 2;
        obj.indirect = u16::from_le_bytes([flat[ioff], flat[ioff + 1]]);
        if obj.len as usize > FILE_BYTES {
            let mut gone = vk;
            wipe_bytes(&mut gone);
            return None;
        }
        off += OBJECT_ON_DISK;
    }
    for share in &mut table.shares {
        *share = Share::empty();
        share.used = flat[off] != 0;
        share.rights = flat[off + 1];
        share.grantee = flat[off + 2];
        share.object = u16::from_le_bytes([flat[off + 4], flat[off + 5]]);
        off += SHARE_ON_DISK;
    }
    table.bitmap.copy_from_slice(&flat[off..off + BITMAP_BYTES]);
    off += BITMAP_BYTES;
    for block in &mut table.blocks {
        block.copy_from_slice(&flat[off..off + BLOCK_SIZE]);
        off += BLOCK_SIZE;
    }
    let mut gone = vk;
    wipe_bytes(&mut gone);
    Some(generation)
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
        if root >= OBJECT_SLOTS {
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
        if !share.used {
            continue;
        }
        if share.rights == 0
            || share.grantee as usize >= ACTOR_SLOTS
            || !table.actors[share.grantee as usize].used
            || share.object as usize >= OBJECT_SLOTS
            || table.objects[share.object as usize].kind == KIND_EMPTY
        {
            report.push(Issue::BadShare { share: si as u16 });
        }
    }

    let mut seen = [false; BLOCK_SLOTS];
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.kind != KIND_FILE && obj.kind != KIND_DIR {
            report.push(Issue::BadObjectKind { object: i as u16 });
            continue;
        }
        if obj.actor as usize >= ACTOR_SLOTS || !table.actors[obj.actor as usize].used {
            report.push(Issue::BadActorRef { object: i as u16 });
        }
        if obj.parent == NO_PARENT {
            if !table.actors.iter().any(|a| a.used && a.root == i as u16) {
                report.push(Issue::OrphanObject { object: i as u16 });
            }
        } else {
            let parent = obj.parent as usize;
            if parent >= OBJECT_SLOTS || parent == i || table.objects[parent].kind != KIND_DIR {
                report.push(Issue::BadParent { object: i as u16 });
            } else {
                let mut cur = obj.parent;
                for _ in 0..OBJECT_SLOTS {
                    if cur == i as u16 {
                        report.push(Issue::Cycle { object: i as u16 });
                        break;
                    }
                    let p = table.objects[cur as usize].parent;
                    if p == NO_PARENT {
                        break;
                    }
                    if p as usize >= OBJECT_SLOTS {
                        report.push(Issue::BadParent { object: i as u16 });
                        break;
                    }
                    cur = p;
                }
            }
        }
        check_object_blocks(table, obj, i as u16, &mut seen, report);
    }
    for (i, used) in seen.iter().enumerate() {
        let bit = (table.bitmap[i / 8] >> (i % 8)) & 1 != 0;
        if bit && !*used {
            report.push(Issue::BlockLeak { block: i as u16 });
        }
        if !bit && *used {
            report.push(Issue::BlockMissing { block: i as u16 });
        }
    }
}

fn mark_seen(
    seen: &mut [bool; BLOCK_SLOTS],
    bitmap: &[u8],
    b: u16,
    report: &mut Report,
) {
    if b as usize >= BLOCK_SLOTS {
        report.push(Issue::BlockMissing { block: b });
        return;
    }
    if seen[b as usize] {
        report.push(Issue::BlockDuplicate { block: b });
    }
    if (bitmap[b as usize / 8] >> (b as usize % 8)) & 1 == 0 {
        report.push(Issue::BlockMissing { block: b });
    }
    seen[b as usize] = true;
}

fn check_object_blocks(
    table: &Table,
    obj: &Object,
    index: u16,
    seen: &mut [bool; BLOCK_SLOTS],
    report: &mut Report,
) {
    let bitmap = &table.bitmap;
    let need = if obj.kind == KIND_FILE {
        if obj.len as usize > FILE_BYTES {
            report.push(Issue::FileTooLarge { object: index });
            return;
        }
        (obj.len as usize + BLOCK_SIZE - 1) / BLOCK_SIZE
    } else if obj.len != 0 {
        report.push(Issue::DirHasLength { object: index });
        0
    } else if obj.indirect != NO_BLOCK {
        report.push(Issue::BlockLeak { block: obj.indirect });
        return;
    } else {
        0
    };
    for (i, &b) in obj.blocks.iter().enumerate() {
        if i < need {
            mark_seen(seen, bitmap, b, report);
        } else if b != NO_BLOCK {
            report.push(Issue::BlockLeak { block: b });
        }
    }
    if need > DIRECT_BLOCKS {
        if obj.indirect == NO_BLOCK {
            report.push(Issue::BlockMissing { block: NO_BLOCK });
            return;
        }
        mark_seen(seen, bitmap, obj.indirect, report);
        let ib = obj.indirect as usize;
        if ib >= BLOCK_SLOTS {
            return;
        }
        for i in 0..INDIRECT_PTRS {
            let off = i * 2;
            let b = u16::from_le_bytes([table.blocks[ib][off], table.blocks[ib][off + 1]]);
            let slot = DIRECT_BLOCKS + i;
            if slot < need {
                mark_seen(seen, bitmap, b, report);
            } else if b != NO_BLOCK {
                report.push(Issue::BlockLeak { block: b });
            }
        }
    } else if obj.indirect != NO_BLOCK {
        report.push(Issue::BlockLeak { block: obj.indirect });
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
        assert!(DISK_HEADER + PAYLOAD_LEN <= DISK_SECTORS * SECTOR);
        assert_eq!(ACTOR_ON_DISK, 66);
        assert_eq!(OBJECT_ON_DISK, 90);
        assert_eq!(SHARE_ON_DISK, 6);
        assert_eq!(SHARE_SLOTS, 32);
        assert_eq!(FILE_BYTES, 32 * 1024);
        assert_eq!(DISK_VERSION, 11);
    }
}
