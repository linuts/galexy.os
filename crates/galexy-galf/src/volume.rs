//! GALF v16 geometry: inode table and file caps that scale with the disk.
//!
//! The sealed slot holds actors, shares, quotas, and the map of inode
//! blocks. Inode records and file bytes live in the data region, so a
//! large disk can name more than a million objects and hold a file
//! larger than 16 GiB.

use super::{
    ACTOR_NAME, ACTOR_SLOTS, BLOCK_SIZE, BLOCK_STRIDE, DISK_SECTORS, DISK_SLOT_COUNT,
    EXTENTS_PER_FILE, NAME_CAP, SECTOR, SHARE_SLOTS,
};
use galexy_crypto::{HASH_LEN, SALT_LEN};

/// Files and directories one volume may name. A smaller disk holds fewer.
pub const OBJECT_CAP: u32 = 1_048_576;
/// Names one `rustc hello.rs -o /hello` tree uses on the login volume.
///
/// The importer reports `rustc-sysroot: 33 entries` (the target JSON plus
/// the `srlib/` files) and creates four directories (`lib`, `rustlib`,
/// `x86_64-unknown-galexy`, `lib`). The volume already has the admin root
/// and `Desktop`. The compile adds `hello.rs` and the linked `/hello`.
/// `-Cincremental` is not passed, so that command does not write a file
/// per query. This is not the format ceiling.
pub const DESK_COMPILE_FILES: u32 = 33 + 4 + 2 + 1 + 1;

const _: () = assert!(DESK_COMPILE_FILES == 41);
/// Largest durable file. One extent can describe it: the count is a `u32`
/// of 4 KiB blocks (64 GiB is 16,777,216 blocks).
pub const FILE_CAP: u64 = 64 * 1024 * 1024 * 1024;
/// On-disk inode. Parent, child, and sibling links are `u32`.
pub const INODE_ON_DISK: usize = 192;
/// Inodes packed into one 4 KiB plaintext block (21 * 192 = 4032).
pub const INODES_PER_BLOCK: usize = BLOCK_SIZE / INODE_ON_DISK;
/// Map entry: pool index of an inode block, plus the generation that sealed it.
pub const MAP_ENTRY: usize = 8;
pub const MAP_PER_BLOCK: usize = BLOCK_SIZE / MAP_ENTRY;
/// Map blocks a superblock can name. Enough for [`OBJECT_CAP`].
pub const MAP_POINTERS: usize = 128;
/// Freed data extents remembered in the superblock.
pub const FREE_EXTENTS: usize = 512;
pub const FREE_EXTENT_ON_DISK: usize = 12;
pub const GEOMETRY_ON_DISK: usize = 64;
pub const ACTOR_ON_DISK: usize = 80;
pub const SHARE_ON_DISK: usize = 8;

const _: () = assert!(INODES_PER_BLOCK == 21);
const _: () = assert!(MAP_PER_BLOCK == 512);
const _: () = assert!(OBJECT_CAP > 1_000_000);
const _: () = assert!(FILE_CAP > 16 * 1024 * 1024 * 1024);

/// Bytes of one sealed 4 KiB block on disk (tag sector + 8 ciphertext sectors).
pub const fn stride_bytes() -> u64 {
    (BLOCK_STRIDE * SECTOR) as u64
}

/// What a disk of `disk_bytes` can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeLimits {
    pub inode_slots: u32,
    pub pool_blocks: u32,
    pub file_max: u64,
    pub inode_blocks: u32,
    pub map_blocks: u32,
}

impl VolumeLimits {
    pub const fn empty() -> Self {
        Self {
            inode_slots: 0,
            pool_blocks: 0,
            file_max: 0,
            inode_blocks: 0,
            map_blocks: 0,
        }
    }
}

fn blocks_for_inodes(slots: u32) -> u32 {
    let per = INODES_PER_BLOCK as u32;
    slots.div_ceil(per).max(1)
}

fn blocks_for_map(inode_blocks: u32) -> u32 {
    let per = MAP_PER_BLOCK as u32;
    inode_blocks.div_ceil(per).max(1)
}

/// Caps for `disk_bytes`, after the two metadata slots.
///
/// The pool is one index space: inode blocks, map blocks, and file bytes.
/// Inode and map blocks take at most a quarter of the pool, and never more
/// than [`OBJECT_CAP`]. `file_max` is the rest, capped at [`FILE_CAP`].
pub fn volume_limits(disk_bytes: u64) -> VolumeLimits {
    let meta = (DISK_SLOT_COUNT * DISK_SECTORS * SECTOR) as u64;
    let rest = disk_bytes.saturating_sub(meta);
    let stride = stride_bytes();
    let pool = (rest / stride) as u32;
    if pool < 8 {
        return VolumeLimits::empty();
    }
    let mut slots = OBJECT_CAP;
    let mut inode_blocks = blocks_for_inodes(slots);
    let mut map_blocks = blocks_for_map(inode_blocks);
    let budget = (pool / 4).max(2);
    while inode_blocks.saturating_add(map_blocks) > budget || map_blocks > MAP_POINTERS as u32 {
        if slots <= INODES_PER_BLOCK as u32 {
            return VolumeLimits::empty();
        }
        slots /= 2;
        if slots < INODES_PER_BLOCK as u32 {
            slots = INODES_PER_BLOCK as u32;
        }
        inode_blocks = blocks_for_inodes(slots);
        map_blocks = blocks_for_map(inode_blocks);
        if slots == INODES_PER_BLOCK as u32 {
            break;
        }
    }
    if inode_blocks.saturating_add(map_blocks) >= pool {
        return VolumeLimits::empty();
    }
    let data = pool - inode_blocks - map_blocks;
    let file_max = FILE_CAP.min(u64::from(data).saturating_mul(BLOCK_SIZE as u64));
    VolumeLimits {
        inode_slots: slots,
        pool_blocks: pool,
        file_max,
        inode_blocks,
        map_blocks,
    }
}

#[derive(Clone, Copy)]
pub struct FreeExtent {
    pub start: u32,
    pub count: u32,
    /// Allocations may reuse this extent once the volume epoch reaches `ready`.
    pub ready: u32,
}

impl FreeExtent {
    pub const fn empty() -> Self {
        Self {
            start: 0,
            count: 0,
            ready: 0,
        }
    }
}

/// Superblock fields that are not actors or shares.
#[derive(Clone, Copy)]
pub struct Geometry {
    pub inode_slots: u32,
    pub pool_blocks: u32,
    pub file_max: u64,
    pub cursor: u32,
    pub free_len: u32,
    pub alloc_clock: u32,
    pub epoch: u32,
    pub map_len: u32,
    pub free: [FreeExtent; FREE_EXTENTS],
    /// Pool index and seal generation of each map block.
    pub map_at: [u32; MAP_POINTERS],
    pub map_gen: [u32; MAP_POINTERS],
}

impl Geometry {
    pub const fn empty() -> Self {
        Self {
            inode_slots: 0,
            pool_blocks: 0,
            file_max: 0,
            cursor: 0,
            free_len: 0,
            alloc_clock: 1,
            epoch: 0,
            map_len: 0,
            free: [FreeExtent::empty(); FREE_EXTENTS],
            map_at: [0; MAP_POINTERS],
            map_gen: [0; MAP_POINTERS],
        }
    }
}

pub fn write_geometry(flat: &mut [u8], geo: &Geometry) {
    flat[..GEOMETRY_ON_DISK].fill(0);
    flat[0..4].copy_from_slice(&geo.inode_slots.to_le_bytes());
    flat[4..8].copy_from_slice(&geo.pool_blocks.to_le_bytes());
    flat[8..16].copy_from_slice(&geo.file_max.to_le_bytes());
    flat[16..20].copy_from_slice(&geo.cursor.to_le_bytes());
    flat[20..24].copy_from_slice(&geo.free_len.to_le_bytes());
    flat[24..28].copy_from_slice(&geo.alloc_clock.to_le_bytes());
    flat[28..32].copy_from_slice(&geo.epoch.to_le_bytes());
    flat[32..36].copy_from_slice(&geo.map_len.to_le_bytes());
    flat[36..40].copy_from_slice(&DESK_COMPILE_FILES.to_le_bytes());
}

pub fn read_geometry_head(flat: &[u8]) -> Option<Geometry> {
    if flat.len() < GEOMETRY_ON_DISK {
        return None;
    }
    let mut geo = Geometry::empty();
    geo.inode_slots = u32::from_le_bytes(flat[0..4].try_into().ok()?);
    geo.pool_blocks = u32::from_le_bytes(flat[4..8].try_into().ok()?);
    geo.file_max = u64::from_le_bytes(flat[8..16].try_into().ok()?);
    geo.cursor = u32::from_le_bytes(flat[16..20].try_into().ok()?);
    geo.free_len = u32::from_le_bytes(flat[20..24].try_into().ok()?);
    geo.alloc_clock = u32::from_le_bytes(flat[24..28].try_into().ok()?);
    geo.epoch = u32::from_le_bytes(flat[28..32].try_into().ok()?);
    geo.map_len = u32::from_le_bytes(flat[32..36].try_into().ok()?);
    let desk = u32::from_le_bytes(flat[36..40].try_into().ok()?);
    if desk != DESK_COMPILE_FILES {
        return None;
    }
    if geo.inode_slots == 0
        || geo.inode_slots > OBJECT_CAP
        || geo.file_max > FILE_CAP
        || geo.free_len > FREE_EXTENTS as u32
        || geo.map_len > MAP_POINTERS as u32
        || geo.alloc_clock == 0
    {
        return None;
    }
    Some(geo)
}

pub fn write_free(flat: &mut [u8], free: &[FreeExtent; FREE_EXTENTS]) {
    for (i, ext) in free.iter().enumerate() {
        let off = i * FREE_EXTENT_ON_DISK;
        flat[off..off + 4].copy_from_slice(&ext.start.to_le_bytes());
        flat[off + 4..off + 8].copy_from_slice(&ext.count.to_le_bytes());
        flat[off + 8..off + 12].copy_from_slice(&ext.ready.to_le_bytes());
    }
}

pub fn read_free(flat: &[u8], free: &mut [FreeExtent; FREE_EXTENTS]) -> bool {
    if flat.len() < FREE_EXTENTS * FREE_EXTENT_ON_DISK {
        return false;
    }
    for (i, ext) in free.iter_mut().enumerate() {
        let off = i * FREE_EXTENT_ON_DISK;
        ext.start = u32::from_le_bytes(flat[off..off + 4].try_into().unwrap_or([0; 4]));
        ext.count = u32::from_le_bytes(flat[off + 4..off + 8].try_into().unwrap_or([0; 4]));
        ext.ready = u32::from_le_bytes(flat[off + 8..off + 12].try_into().unwrap_or([0; 4]));
    }
    true
}

pub fn write_map_ptrs(flat: &mut [u8], at: &[u32; MAP_POINTERS], gen: &[u32; MAP_POINTERS]) {
    for i in 0..MAP_POINTERS {
        let off = i * 8;
        flat[off..off + 4].copy_from_slice(&at[i].to_le_bytes());
        flat[off + 4..off + 8].copy_from_slice(&gen[i].to_le_bytes());
    }
}

pub fn read_map_ptrs(
    flat: &[u8],
    at: &mut [u32; MAP_POINTERS],
    gen: &mut [u32; MAP_POINTERS],
) -> bool {
    if flat.len() < MAP_POINTERS * 8 {
        return false;
    }
    for i in 0..MAP_POINTERS {
        let off = i * 8;
        at[i] = u32::from_le_bytes(flat[off..off + 4].try_into().unwrap_or([0; 4]));
        gen[i] = u32::from_le_bytes(flat[off + 4..off + 8].try_into().unwrap_or([0; 4]));
    }
    true
}

/// Where the variable superblock tail sits, after geometry + actors + shares.
pub const fn free_list_offset() -> usize {
    GEOMETRY_ON_DISK + ACTOR_ON_DISK * ACTOR_SLOTS + SHARE_ON_DISK * SHARE_SLOTS
}

pub const fn map_ptr_offset() -> usize {
    free_list_offset() + FREE_EXTENTS * FREE_EXTENT_ON_DISK
}

pub const fn payload_len() -> usize {
    map_ptr_offset() + MAP_POINTERS * 8
}

/// Packs one inode at `dest` (192 bytes).
pub fn write_inode(dest: &mut [u8], obj: &super::Object) {
    dest[..INODE_ON_DISK].fill(0);
    dest[0] = obj.kind;
    dest[1] = obj.actor;
    dest[2] = obj.name_len;
    dest[3] = obj.extent_count;
    dest[4..8].copy_from_slice(&obj.parent.to_le_bytes());
    dest[8..12].copy_from_slice(&obj.child.to_le_bytes());
    dest[12..16].copy_from_slice(&obj.next.to_le_bytes());
    dest[16..24].copy_from_slice(&obj.len.to_le_bytes());
    dest[24..24 + NAME_CAP].copy_from_slice(&obj.name);
    for (e, ext) in obj.extents.iter().enumerate() {
        let off = 24 + NAME_CAP + e * 12;
        dest[off..off + 4].copy_from_slice(&ext.start.to_le_bytes());
        dest[off + 4..off + 8].copy_from_slice(&ext.count.to_le_bytes());
        dest[off + 8..off + 12].copy_from_slice(&ext.gen.to_le_bytes());
    }
}

pub fn read_inode(src: &[u8]) -> Option<super::Object> {
    if src.len() < INODE_ON_DISK {
        return None;
    }
    let mut obj = super::Object::empty();
    obj.kind = src[0];
    obj.actor = src[1];
    obj.name_len = src[2].min(NAME_CAP as u8);
    obj.extent_count = src[3].min(EXTENTS_PER_FILE as u8);
    obj.parent = u32::from_le_bytes(src[4..8].try_into().ok()?);
    obj.child = u32::from_le_bytes(src[8..12].try_into().ok()?);
    obj.next = u32::from_le_bytes(src[12..16].try_into().ok()?);
    obj.len = u64::from_le_bytes(src[16..24].try_into().ok()?);
    obj.name.copy_from_slice(&src[24..24 + NAME_CAP]);
    for (e, ext) in obj.extents.iter_mut().enumerate() {
        let off = 24 + NAME_CAP + e * 12;
        ext.start = u32::from_le_bytes(src[off..off + 4].try_into().ok()?);
        ext.count = u32::from_le_bytes(src[off + 4..off + 8].try_into().ok()?);
        ext.gen = u32::from_le_bytes(src[off + 8..off + 12].try_into().ok()?);
    }
    if obj.len > FILE_CAP {
        return None;
    }
    Some(obj)
}

pub fn write_actor(dest: &mut [u8], actor: &super::Actor) {
    dest[..ACTOR_ON_DISK].fill(0);
    dest[0] = u8::from(actor.used);
    dest[1] = actor.name_len;
    dest[2..2 + ACTOR_NAME].copy_from_slice(&actor.name);
    let root_at = 2 + ACTOR_NAME;
    dest[root_at..root_at + 4].copy_from_slice(&actor.root.to_le_bytes());
    let salt_at = root_at + 4;
    dest[salt_at..salt_at + SALT_LEN].copy_from_slice(&actor.salt);
    let hash_at = salt_at + SALT_LEN;
    dest[hash_at..hash_at + HASH_LEN].copy_from_slice(&actor.pass_hash);
    let koff = hash_at + HASH_LEN;
    dest[koff..koff + 4].copy_from_slice(&actor.kdf_iters.to_le_bytes());
    let qoff = koff + 4;
    dest[qoff..qoff + 4].copy_from_slice(&actor.max_objects.to_le_bytes());
    dest[qoff + 4..qoff + 12].copy_from_slice(&actor.max_bytes.to_le_bytes());
}

pub fn read_actor(src: &[u8]) -> Option<super::Actor> {
    if src.len() < ACTOR_ON_DISK {
        return None;
    }
    let mut actor = super::Actor::empty();
    actor.used = src[0] != 0;
    actor.name_len = src[1].min(ACTOR_NAME as u8);
    actor.name.copy_from_slice(&src[2..2 + ACTOR_NAME]);
    let root_at = 2 + ACTOR_NAME;
    actor.root = u32::from_le_bytes(src[root_at..root_at + 4].try_into().ok()?);
    let salt_at = root_at + 4;
    actor
        .salt
        .copy_from_slice(&src[salt_at..salt_at + SALT_LEN]);
    let hash_at = salt_at + SALT_LEN;
    actor
        .pass_hash
        .copy_from_slice(&src[hash_at..hash_at + HASH_LEN]);
    let koff = hash_at + HASH_LEN;
    actor.kdf_iters = u32::from_le_bytes(src[koff..koff + 4].try_into().ok()?);
    let qoff = koff + 4;
    actor.max_objects = u32::from_le_bytes(src[qoff..qoff + 4].try_into().ok()?);
    actor.max_bytes = u64::from_le_bytes(src[qoff + 4..qoff + 12].try_into().ok()?);
    Some(actor)
}

pub fn write_share(dest: &mut [u8], share: &super::Share) {
    dest[..SHARE_ON_DISK].fill(0);
    dest[0] = u8::from(share.used);
    dest[1] = share.rights;
    dest[2] = share.grantee;
    dest[4..8].copy_from_slice(&share.object.to_le_bytes());
}

pub fn read_share(src: &[u8]) -> Option<super::Share> {
    if src.len() < SHARE_ON_DISK {
        return None;
    }
    let mut share = super::Share::empty();
    share.used = src[0] != 0;
    share.rights = src[1];
    share.grantee = src[2];
    share.object = u32::from_le_bytes(src[4..8].try_into().ok()?);
    Some(share)
}

/// Inode `id` lives in inode-block `block` at byte `offset` within the plaintext.
pub fn inode_address(id: u32) -> (u32, usize) {
    let per = INODES_PER_BLOCK as u32;
    (id / per, (id % per) as usize * INODE_ON_DISK)
}

/// Map slot for inode-block `inode_block`.
pub fn map_address(inode_block: u32) -> (u32, usize) {
    let per = MAP_PER_BLOCK as u32;
    (inode_block / per, (inode_block % per) as usize * MAP_ENTRY)
}
