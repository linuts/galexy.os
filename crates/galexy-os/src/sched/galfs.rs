//! galfs: owner-qualified paths and tokens.
//!
//! Each actor has one root directory. `/Desktop` is that actor's child
//! named Desktop. `/dan@Desktop` is dan's. A token names an object and a
//! set of rights; the path is only a lookup. Boot creates one immortal
//! actor, [`ADMIN_NAME`]. Lock order: [`THREADS`] then this table.
//!
//! When a [`BlockDevice`] large enough for both metadata slots and the
//! data region is present, the table is loaded from a GALF v15 image or
//! formatted if both slots are bad. File bytes are 4 KiB extents in an
//! encrypted pool sized to the disk. A small cache is the only copy in
//! RAM. A directory is the sibling list on its inode; a card still names
//! an object and covers descendants through `parent`.
//! Mutates mark the table dirty; a coalesced sync (changed blocks, then
//! the inactive metadata slot, then flush) runs on the 1 Hz main-loop
//! tick, on `Syscall::Sync`, and before power-off. Without a usable disk
//! the table stays RAM-only. Backends: virtio-blk (preferred when
//! present), else ATA primary slave.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::sync::Mutex;

use galexy_abi::SysError;
use galexy_core::{crc32, HASH_LEN, SALT_LEN};
use galexy_crypto::{
    derive_key, hash_eq, hash_password_iters, open, seal, wipe_bytes, KEY_LEN, NONCE_LEN, TAG_LEN,
};

const _: () = assert!(SALT_LEN == galexy_crypto::SALT_LEN);
const _: () = assert!(HASH_LEN == galexy_crypto::HASH_LEN);

use crate::drivers::ata::PrimarySlave;
use crate::drivers::block::{self, BlockDevice};
use crate::drivers::virtio_blk::VirtioBlk;

/// Format ceiling on files and directories. A live volume holds fewer
/// when the disk is smaller; see [`galexy_galf::volume_limits`].
pub const OBJECT_SLOTS: usize = galexy_galf::OBJECT_SLOTS;
/// Actors (users) the table can name.
pub const ACTOR_SLOTS: usize = 32;
/// File data block (eight ciphertext sectors plus a private tag sector).
pub const BLOCK_SIZE: usize = 4096;
/// Extents stored on one inode.
pub const EXTENTS_PER_FILE: usize = 8;
/// Format ceiling for one durable file. The live cap is the volume's
/// `file_max` (what the disk can hold, and never above this).
pub const FILE_BYTES: u64 = galexy_galf::FILE_CAP;
/// Names kept in RAM when no disk is attached.
const DISKLESS_INODES: usize = 8192;
/// Blocks kept in RAM when no disk is attached (bigtest's 20 MiB file).
const DISKLESS_BLOCKS: usize = (32 * 1024 * 1024) / BLOCK_SIZE;
// Host fsck (`galexy-galf`) must stay byte-identical — STYLE: no forked magic.
const _: () = assert!(ACTOR_SLOTS == galexy_galf::ACTOR_SLOTS);
const _: () = assert!(BLOCK_SIZE == galexy_galf::BLOCK_SIZE);
const _: () = assert!(EXTENTS_PER_FILE == galexy_galf::EXTENTS_PER_FILE);
const _: () = assert!(FILE_BYTES == galexy_galf::FILE_BYTES);
const _: () = assert!(FILE_BYTES > 16 * 1024 * 1024 * 1024);
/// Tokens one task may hold.
pub use galexy_galf::cards::TOKEN_SLOTS;
/// Durable home shares recorded in the sealed image (re-applied at login).
pub const SHARE_SLOTS: usize = 32;
/// Default object quota for a new non-admin actor (root + Desktop count).
pub const DEFAULT_MAX_OBJECTS: u32 = 16;
/// Default byte quota for a new non-admin actor (sum of file lengths).
pub const DEFAULT_MAX_BYTES: u64 = 16 * 1024;
/// Path components after the optional owner.
pub const MAX_DEPTH: usize = galexy_core::MAX_DEPTH;
/// One path-component name.
const NAME_CAP: usize = galexy_core::NAME_CAP;
/// Actor name length.
const ACTOR_NAME: usize = 32;

const KIND_EMPTY: u8 = 0;
pub(crate) const KIND_FILE: u8 = 1;
pub(crate) const KIND_DIR: u8 = 2;

/// Parent of an actor root.
const NO_PARENT: u32 = u32::MAX;
pub use galexy_galf::cards::NO_OBJECT;
const CACHE_SLOTS: usize = 8;

pub use galexy_galf::cards::{
    RIGHT_ALL, RIGHT_CREATE, RIGHT_LIST, RIGHT_ONCE, RIGHT_READ, RIGHT_REMOVE, RIGHT_WRITE,
};
/// Stored in bit 31 of `Actor::max_objects`. Set while `admin`'s password
/// is still the default. Not a GALF version bump.
const MUST_CHANGE_BIT: u32 = 0x8000_0000;

pub use galexy_galf::cards::Token;

/// Credentials copied onto a task at spawn.
#[derive(Clone, Copy)]
pub struct FsCred {
    pub root: u32,
    pub tokens: [Token; TOKEN_SLOTS],
}

impl FsCred {
    pub const fn none() -> Self {
        Self {
            root: NO_OBJECT,
            tokens: [Token::empty(); TOKEN_SLOTS],
        }
    }

    /// Full rights on `root`. Used for the interactive shell and test blobs.
    pub fn launcher(root: u32) -> Self {
        let mut tokens = [Token::empty(); TOKEN_SLOTS];
        tokens[0] = Token {
            object: root,
            rights: RIGHT_ALL,
        };
        Self { root, tokens }
    }
}

#[derive(Clone, Copy)]
struct Actor {
    used: bool,
    name: [u8; ACTOR_NAME],
    name_len: u8,
    root: u32,
    salt: [u8; SALT_LEN],
    pass_hash: [u8; HASH_LEN],
    /// PBKDF2 iteration count that produced `pass_hash`.
    kdf_iters: u32,
    /// Max objects (files + dirs, including root) this actor may own.
    /// Bit 31 is [`MUST_CHANGE_BIT`], not part of the count.
    max_objects: u32,
    /// Max sum of file lengths (bytes) this actor may own.
    max_bytes: u64,
    /// Objects this actor currently owns. Rebuilt at load.
    used_objects: u32,
    /// Sum of file lengths this actor currently owns.
    used_bytes: u64,
}

impl Actor {
    const fn empty() -> Self {
        Self {
            used: false,
            name: [0; ACTOR_NAME],
            name_len: 0,
            root: NO_OBJECT,
            salt: [0; SALT_LEN],
            pass_hash: [0; HASH_LEN],
            kdf_iters: 0,
            max_objects: 0,
            max_bytes: 0,
            used_objects: 0,
            used_bytes: 0,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }

    fn object_limit(self) -> u32 {
        self.max_objects & !MUST_CHANGE_BIT
    }

    /// Stores an already-derived credential (see [`PasswordCred`]).
    fn install_password(&mut self, cred: &PasswordCred, password: &[u8]) {
        self.salt = cred.salt;
        self.kdf_iters = cred.iters;
        self.pass_hash = cred.hash;
        // Default admin password keeps the must-change flag across reboot.
        if self.name_is(ADMIN_NAME) && password == ADMIN_DEFAULT_PASSWORD.as_bytes() {
            self.max_objects |= MUST_CHANGE_BIT;
        } else {
            self.max_objects &= !MUST_CHANGE_BIT;
        }
    }

    /// The stored salt/cost/hash, for a check that runs after the table
    /// lock drops ([`verify_password`]).
    fn password_cred(&self) -> PasswordCred {
        PasswordCred {
            salt: self.salt,
            iters: self.kdf_iters,
            hash: self.pass_hash,
        }
    }
}

/// Salt, cost and hash of one password: everything the KDF needs, so the
/// multi-ms PBKDF2 runs with NO table lock held.
///
/// Syscalls run IF=0. Holding `TABLE` across 100 000 iterations stalled
/// every other `TABLE` user — the 1 Hz sync and status bar on the other
/// CPU spin IF=0 for the whole derivation (galexy.os#86). Derive first,
/// then take the lock only to copy 60 bytes.
struct PasswordCred {
    salt: [u8; SALT_LEN],
    iters: u32,
    hash: [u8; HASH_LEN],
}

impl PasswordCred {
    /// Fresh salt + the boot's default cost. `passwd` and format both
    /// re-derive at that cost, not at whatever count the previous hash used.
    fn derive(password: &[u8]) -> Self {
        let mut cred = Self {
            salt: [0; SALT_LEN],
            iters: password_kdf_iters(),
            hash: [0; HASH_LEN],
        };
        crate::arch::rand::fill_bytes(&mut cred.salt);
        hash_password_iters(password, &cred.salt, cred.iters, &mut cred.hash);
        cred
    }

    fn check(&self, password: &[u8]) -> bool {
        // A hostile image can store a huge count. Refuse it instead of
        // wedging the CPU. A stored older cost (including the test-kernel
        // 10 000) still verifies.
        const KDF_ITERS_MAX: u32 = 1_000_000;
        if self.iters == 0 || self.iters > KDF_ITERS_MAX {
            return false;
        }
        let mut got = [0u8; HASH_LEN];
        hash_password_iters(password, &self.salt, self.iters, &mut got);
        let ok = hash_eq(&got, &self.hash);
        wipe_bytes(&mut got);
        ok
    }

    fn wipe(&mut self) {
        wipe_bytes(&mut self.salt);
        wipe_bytes(&mut self.hash);
        self.iters = 0;
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Object {
    pub(crate) kind: u8,
    parent: u32,
    /// Actor that owns this object (its root's actor).
    actor: u8,
    name: [u8; NAME_CAP],
    name_len: u8,
    pub(crate) len: u64,
    extent_count: u8,
    extents: [galexy_galf::Extent; EXTENTS_PER_FILE],
    /// First child of a directory. [`NO_OBJECT`] when this is a file or empty.
    child: u32,
    /// Next object in the parent's directory list.
    next: u32,
}

impl Object {
    const fn empty() -> Self {
        Self {
            kind: KIND_EMPTY,
            parent: NO_PARENT,
            actor: 0,
            name: [0; NAME_CAP],
            name_len: 0,
            len: 0,
            extent_count: 0,
            extents: [galexy_galf::Extent::empty(); EXTENTS_PER_FILE],
            child: NO_OBJECT,
            next: NO_OBJECT,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.kind != KIND_EMPTY && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

/// Durable grant: object + rights installed on `grantee`'s login session.
#[derive(Clone, Copy)]
struct Share {
    used: bool,
    /// Actor index that receives the card at login.
    grantee: u8,
    rights: u8,
    object: u32,
}

impl Share {
    const fn empty() -> Self {
        Self {
            used: false,
            grantee: 0,
            rights: 0,
            object: NO_OBJECT,
        }
    }
}

/// Names live in fixed chunks so a large table never asks the heap for
/// one multi-megabyte block. A single block that big does not come back
/// from the allocator once the second CPU is up.
const OBJECT_CHUNK: usize = 64;

struct ObjectTable {
    chunks: alloc::vec::Vec<alloc::boxed::Box<[Object]>>,
    len: usize,
}

struct ObjectIter<'a> {
    table: &'a ObjectTable,
    idx: usize,
}

impl<'a> Iterator for ObjectIter<'a> {
    type Item = &'a Object;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.table.len {
            return None;
        }
        let i = self.idx;
        self.idx += 1;
        Some(&self.table[i])
    }
}

impl ObjectTable {
    const fn empty() -> Self {
        Self {
            chunks: alloc::vec::Vec::new(),
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn resize(&mut self, n: usize, fill: Object) {
        let need = n.div_ceil(OBJECT_CHUNK);
        while self.chunks.len() < need {
            self.chunks
                .push(alloc::vec![fill; OBJECT_CHUNK].into_boxed_slice());
        }
        if self.chunks.len() > need {
            self.chunks.truncate(need);
        }
        self.len = n;
    }

    fn iter(&self) -> ObjectIter<'_> {
        ObjectIter {
            table: self,
            idx: 0,
        }
    }

    fn get(&self, i: usize) -> Option<&Object> {
        if i >= self.len {
            None
        } else {
            Some(&self[i])
        }
    }

    fn get_mut(&mut self, i: usize) -> Option<&mut Object> {
        if i >= self.len {
            None
        } else {
            Some(&mut self[i])
        }
    }

    fn clone_from(&mut self, src: &Self) {
        self.resize(src.len, Object::empty());
        for i in 0..src.len {
            self[i] = src[i];
        }
    }
}

impl core::ops::Index<usize> for ObjectTable {
    type Output = Object;

    fn index(&self, i: usize) -> &Object {
        &self.chunks[i / OBJECT_CHUNK][i % OBJECT_CHUNK]
    }
}

impl core::ops::IndexMut<usize> for ObjectTable {
    fn index_mut(&mut self, i: usize) -> &mut Object {
        &mut self.chunks[i / OBJECT_CHUNK][i % OBJECT_CHUNK]
    }
}

impl<'a> IntoIterator for &'a ObjectTable {
    type Item = &'a Object;
    type IntoIter = ObjectIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

struct Table {
    actors: [Actor; ACTOR_SLOTS],
    objects: ObjectTable,
    shares: [Share; SHARE_SLOTS],
    geo: galexy_galf::Geometry,
}

const fn empty_table() -> Table {
    Table {
        actors: [Actor::empty(); ACTOR_SLOTS],
        objects: ObjectTable::empty(),
        shares: [Share::empty(); SHARE_SLOTS],
        geo: galexy_galf::Geometry::empty(),
    }
}

static TABLE: Mutex<Table> = Mutex::new(empty_table());
/// Scratch tables for disk load — must not live on the kernel stack.
static LOAD_BEST: Mutex<Table> = Mutex::new(empty_table());
static LOAD_CAND: Mutex<Table> = Mutex::new(empty_table());

static BOOTED: AtomicBool = AtomicBool::new(false);
/// True when the ATA slave accepted a load or format write.
static DISK_LIVE: AtomicBool = AtomicBool::new(false);
/// ATA slave present but both slots failed with GALF magic — refuse format.
static DISK_CORRUPT: AtomicBool = AtomicBool::new(false);
/// Slot (0 or 1) that holds the newest valid image; next sync writes the other.
static ACTIVE_SLOT: AtomicU32 = AtomicU32::new(0);
/// Generation of the active slot (next sync writes gen + 1).
static ACTIVE_GEN: AtomicU64 = AtomicU64::new(0);
/// Times this boot loaded an older slot because a newer one failed checks.
static RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// Name of the immortal boot actor.
pub const ADMIN_NAME: &str = "admin";

/// Admin's root object. Valid after [`init`].
static ADMIN_ROOT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(NO_OBJECT);

/// On-disk image: dual slots of header + actors + objects.
///
/// Each slot is [`DISK_SECTORS`] long. Slot 0 starts at [`disk_lba_base`];
/// slot 1 at base + [`DISK_SECTORS`]. A sync writes the inactive slot with
/// gen+1 and a CRC, then flushes — a crash mid-write leaves the previous
/// slot intact.
pub const DISK_MAGIC: [u8; 4] = *b"GALF";
/// Extent volume (directory lists + 4 KiB blocks outside the seal).
/// Older images are refused with a serial line; there is no silent migrate.
/// Milestone 99 records the desk-compile file count in the geometry and
/// moves this with `galexy-galf`. The slot layout stays two 64 KiB seals.
pub const DISK_VERSION: u16 = 17;
/// Sectors per metadata slot (64 KiB).
pub const DISK_SECTORS: usize = 128;
pub const DISK_SLOT_COUNT: usize = 2;
/// Common first-partition LBA (1 MiB). Tests call [`set_disk_lba_base`]
/// with this so GALF need not sit at absolute LBA 0.
pub const DISK_PART_LBA: u32 = 2048;
/// Clear header + wrap fields + data tag (see `encode_table`).
const DISK_HEADER: usize = 128;
const ACTOR_ON_DISK: usize = galexy_galf::ACTOR_ON_DISK;
const SHARE_ON_DISK: usize = galexy_galf::SHARE_ON_DISK;
const _: () = assert!(DISK_MAGIC[0] == galexy_galf::DISK_MAGIC[0]);
const _: () = assert!(DISK_MAGIC[1] == galexy_galf::DISK_MAGIC[1]);
const _: () = assert!(DISK_MAGIC[2] == galexy_galf::DISK_MAGIC[2]);
const _: () = assert!(DISK_MAGIC[3] == galexy_galf::DISK_MAGIC[3]);
const _: () = assert!(DISK_VERSION == galexy_galf::DISK_VERSION);
const _: () = assert!(DISK_SECTORS == galexy_galf::DISK_SECTORS);
const _: () = assert!(ACTOR_ON_DISK == galexy_galf::ACTOR_ON_DISK);
const _: () = assert!(DISK_HEADER == galexy_galf::DISK_HEADER);
const _: () = assert!(SHARE_SLOTS == galexy_galf::SHARE_SLOTS);
const _: () = assert!(SHARE_ON_DISK == galexy_galf::SHARE_ON_DISK);
const _: () = assert!(galexy_galf::OBJECT_ON_DISK == 192);
const PAYLOAD_LEN: usize = galexy_galf::PAYLOAD_LEN;
const _: () = assert!(DISK_HEADER + PAYLOAD_LEN <= DISK_SECTORS * block::SECTOR);
/// Sectors a volume needs past the LBA base (8 MiB).
const DISK_MIN_SECTORS: u64 = (galexy_galf::MIN_DISK_BYTES / block::SECTOR) as u64;
/// Default password for the immortal admin account at format.
pub const ADMIN_DEFAULT_PASSWORD: &str = "admin";
/// Bring-up volume passphrase (wraps the disk key). Test kernels and the
/// disk harness auto-unlock with this. Production `galexy-os` prompts.
pub const VOLUME_PASSPHRASE: &[u8] = b"galfs";
/// Max bytes accepted by [`unlock_volume`] (matches the syscall staging cap).
const PASSPHRASE_MAX: usize = 64;

/// The slot staging buffer, page-aligned so no sector straddles a page.
///
/// The virtio driver splits a request at every physical discontinuity, so
/// alignment is not what keeps DMA inside the buffer; it keeps every
/// request a single descriptor and makes the layout independent of where
/// KASLR puts `.bss` (galexy.os#86 follow-up: an unaligned buffer crossing
/// a 2 MiB boundary once DMA'd into the page table between its frames).
#[repr(C, align(4096))]
struct SectorBuf([[u8; block::SECTOR]; DISK_SECTORS]);

impl core::ops::Deref for SectorBuf {
    type Target = [[u8; block::SECTOR]; DISK_SECTORS];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for SectorBuf {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

static DISK_BUF: Mutex<SectorBuf> = Mutex::new(SectorBuf([[0u8; block::SECTOR]; DISK_SECTORS]));

/// Allocation state for the data pool. File bytes are not stored here.
/// Per-block arrays are sized to the live disk in [`bind_pool`], not to
/// the format ceiling.
struct Pool {
    blocks: u32,
    disk: bool,
    bitmap: alloc::vec::Vec<u8>,
    gen: alloc::vec::Vec<u32>,
    stable: alloc::vec::Vec<bool>,
    dirty: alloc::vec::Vec<bool>,
    /// Ciphertext for this generation is already on the device.
    sealed: alloc::vec::Vec<bool>,
    hold: alloc::vec::Vec<u8>,
    clock: u32,
    /// Next first-fit index. A fresh sequential file stays one extent.
    cursor: u32,
}

/// Scratch for `validate_table`. Sized in [`bind_pool`], not on the syscall path.
static BLOCK_SEEN: Mutex<alloc::vec::Vec<u64>> = Mutex::new(alloc::vec::Vec::new());
/// Scratch for directory-list checks. Sized when the inode table is.
static DIR_SEEN: Mutex<alloc::vec::Vec<u64>> = Mutex::new(alloc::vec::Vec::new());

static POOL: Mutex<Pool> = Mutex::new(Pool {
    blocks: 0,
    disk: false,
    bitmap: alloc::vec::Vec::new(),
    gen: alloc::vec::Vec::new(),
    stable: alloc::vec::Vec::new(),
    dirty: alloc::vec::Vec::new(),
    sealed: alloc::vec::Vec::new(),
    hold: alloc::vec::Vec::new(),
    clock: 1,
    cursor: 0,
});

/// Plaintext cache. Dirty lines stay until they are sealed to the device.
struct BlockCache {
    index: [u32; CACHE_SLOTS],
    data: [[u8; BLOCK_SIZE]; CACHE_SLOTS],
}

static CACHE: Mutex<BlockCache> = Mutex::new(BlockCache {
    index: [u32::MAX; CACHE_SLOTS],
    data: [[0u8; BLOCK_SIZE]; CACHE_SLOTS],
});

/// Diskless backing store. A booted disk leaves this unused.
#[repr(C, align(4096))]
struct RamPool {
    blocks: [[u8; BLOCK_SIZE]; DISKLESS_BLOCKS],
}

static RAM_POOL: Mutex<RamPool> = Mutex::new(RamPool {
    blocks: [[0u8; BLOCK_SIZE]; DISKLESS_BLOCKS],
});
const _: () = assert!(core::mem::align_of::<SectorBuf>() == 4096);
/// First LBA of slot 0 (partition offset). Default 0; set before [`init`].
static LBA_BASE: AtomicU32 = AtomicU32::new(0);
/// Unwrapped volume key while the disk is mounted. `None` when locked.
static VOLUME_KEY: Mutex<Option<[u8; KEY_LEN]>> = Mutex::new(None);
/// Passphrase that wraps the volume key on the next seal. Wiped with the key.
struct VolumePass {
    bytes: [u8; PASSPHRASE_MAX],
    len: usize,
}
static VOLUME_PASS: Mutex<VolumePass> = Mutex::new(VolumePass {
    bytes: [0; PASSPHRASE_MAX],
    len: 0,
});
/// When false, [`init`] leaves a usable disk locked until [`unlock_volume`].
/// Default true so disk tests mount with [`VOLUME_PASSPHRASE`].
static AUTO_UNLOCK: AtomicBool = AtomicBool::new(true);
/// A locked disk with no GALF magic. The login screen asks the operator
/// to choose a passphrase. Cleared once that passphrase seals the disk.
static VOLUME_FRESH: AtomicBool = AtomicBool::new(false);

/// Production boot calls this before [`init`]. Test kernels leave the default.
pub fn set_auto_unlock(on: bool) {
    AUTO_UNLOCK.store(on, Ordering::Release);
}

fn auto_unlock() -> bool {
    AUTO_UNLOCK.load(Ordering::Acquire)
}

/// Sets the absolute LBA of GALF slot 0. Call before [`init`].
pub fn set_disk_lba_base(lba: u32) {
    LBA_BASE.store(lba, Ordering::Release);
}

/// Absolute LBA of GALF slot 0 (0 unless [`set_disk_lba_base`] ran).
pub fn disk_lba_base() -> u32 {
    LBA_BASE.load(Ordering::Acquire)
}

fn slot_lba(slot: u32) -> u32 {
    disk_lba_base().saturating_add(slot.saturating_mul(DISK_SECTORS as u32))
}

/// The block device galfs uses for durable slots.
///
/// Prefers virtio-blk when QEMU attached one; otherwise the ATA primary
/// slave. Probe order is fixed so IDE-only boots stay unchanged.
fn disk() -> &'static dyn BlockDevice {
    if VirtioBlk.present() {
        &VirtioBlk
    } else {
        &PrimarySlave
    }
}

/// True when the device is present and large enough for base + both slots.
fn disk_usable() -> bool {
    let d = disk();
    if !d.present() {
        return false;
    }
    let need = u64::from(disk_lba_base()) + DISK_MIN_SECTORS;
    d.capacity_sectors() >= need
}

/// Addressable sector count from IDENTIFY (`0` if absent / unknown).
pub fn disk_capacity_sectors() -> u64 {
    disk().capacity_sectors()
}

/// Set when the in-RAM table differs from the last flushed dual slot.
static DIRTY: AtomicBool = AtomicBool::new(false);
/// Set after the "sync skipped: volume locked" line was printed for the
/// current locked period; cleared by the next successful encode.
static LOCKED_SKIP_LOGGED: AtomicBool = AtomicBool::new(false);

/// The login kernel should attach `srlib/` once the live table is the one
/// that will stay. A sealed unlock sets this; the main loop imports with
/// interrupts on. The bodies stay ramdisk mappings, so this is not a
/// ~90 MiB copy inside the syscall.
static COMPILER_SYSROOT_DUE: AtomicBool = AtomicBool::new(false);

/// Ask the main loop to import the compiler sysroot after this unlock.
pub fn note_compiler_sysroot() {
    COMPILER_SYSROOT_DUE.store(true, Ordering::Release);
}

/// `true` once, when an import was requested and the volume is open.
///
/// A locked volume keeps the flag set. The pre-unlock RAM table is
/// replaced by the sealed disk, so importing before that would be discarded.
pub fn take_compiler_sysroot_due() -> bool {
    if !COMPILER_SYSROOT_DUE.load(Ordering::Acquire) {
        return false;
    }
    if volume_locked() {
        return false;
    }
    COMPILER_SYSROOT_DUE.store(false, Ordering::Release);
    true
}

/// Marks the table dirty so the next [`sync_if_dirty`] / [`sync`] flushes.
pub fn mark_dirty() {
    DIRTY.store(true, Ordering::Release);
}

/// True when mutates are waiting on a dual-slot commit.
pub fn is_dirty() -> bool {
    DIRTY.load(Ordering::Acquire)
}

/// Builds actor [`ADMIN_NAME`] with an empty Desktop, or loads the newest
/// valid GALF slot from the block device. Call once.
///
/// Sync policy: mutates call [`mark_dirty`]; the main loop's 1 Hz tick,
/// [`sync_explicit`], and power-off call [`sync_if_dirty`] / [`sync`]
/// (inactive slot + flush). Crash window is up to one second of writes.
pub fn init() {
    if BOOTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if disk().present() && !disk_usable() {
        let need = u64::from(disk_lba_base()) + DISK_MIN_SECTORS;
        crate::serial_println!(
            "[galfs] disk too small ({} sectors; need {} at LBA {}); RAM-only",
            disk().capacity_sectors(),
            need,
            disk_lba_base(),
        );
    } else if disk_usable() && !auto_unlock() {
        format_ram_table();
        let fresh = !disk_has_seal();
        VOLUME_FRESH.store(fresh, Ordering::Release);
        if fresh {
            crate::serial_println!("[galfs] volume locked; choose a passphrase");
        } else {
            crate::serial_println!("[galfs] volume locked; passphrase required");
        }
        return;
    } else if disk_usable() {
        match load_from_disk(VOLUME_PASSPHRASE) {
            DiskLoad::Loaded { recovered } => {
                store_passphrase(VOLUME_PASSPHRASE);
                let slot = ACTIVE_SLOT.load(Ordering::Relaxed);
                let gen = ACTIVE_GEN.load(Ordering::Relaxed);
                if recovered {
                    RECOVERIES.fetch_add(1, Ordering::Relaxed);
                    crate::serial_println!(
                        "[galfs] loaded slot {} gen {} (recovered from bad sibling)",
                        slot,
                        gen,
                    );
                } else {
                    crate::serial_println!("[galfs] loaded slot {} gen {}", slot, gen);
                }
                return;
            }
            DiskLoad::Corrupt => {
                DISK_CORRUPT.store(true, Ordering::Release);
                crate::serial_println!(
                    "[galfs] disk corrupt; refusing silent format (galfs unavailable)"
                );
                return;
            }
            DiskLoad::Empty => {
                // First boot on a zeroed image — format below.
            }
        }
    }
    format_fresh();
}

fn live_caps(on_disk: bool) -> (u32, u32, u64) {
    if on_disk {
        let bytes = disk()
            .capacity_sectors()
            .saturating_sub(u64::from(disk_lba_base()))
            .saturating_mul(block::SECTOR as u64);
        let lim = galexy_galf::volume_limits(bytes);
        (lim.inode_slots, lim.pool_blocks, lim.file_max)
    } else {
        (
            DISKLESS_INODES as u32,
            DISKLESS_BLOCKS as u32,
            (DISKLESS_BLOCKS * BLOCK_SIZE) as u64,
        )
    }
}

/// Object storage for a fresh table.
///
/// Built before [`TABLE`] is taken. Growing the heap while that lock is
/// held deadlocks the other CPU: the 1 Hz path can hold the serial lock
/// and then wait for the table, and the grower is waiting on that CPU.
fn prepare_live_storage(on_disk: bool) -> ObjectTable {
    let (slots, _, _) = live_caps(on_disk);
    let mut objects = ObjectTable::empty();
    objects.resize(slots as usize, Object::empty());
    let inode_blocks = (slots as usize).div_ceil(galexy_galf::INODES_PER_BLOCK);
    {
        let mut loc = META_LOC.lock();
        if loc.inode_at.len() < inode_blocks {
            loc.inode_at.resize(inode_blocks, 0);
            loc.inode_gen.resize(inode_blocks, 0);
        }
    }
    let words = (slots as usize).div_ceil(64);
    let mut seen = DIR_SEEN.lock();
    if seen.len() < words {
        seen.resize(words, 0);
    }
    objects
}

fn install_live_table(table: &mut Table, on_disk: bool, objects: ObjectTable) {
    let (slots, pool, file_max) = live_caps(on_disk);
    table.objects = objects;
    table.geo.inode_slots = slots;
    table.geo.pool_blocks = pool;
    table.geo.file_max = file_max;
    if table.geo.alloc_clock == 0 {
        table.geo.alloc_clock = 1;
    }
}

fn format_ram_table() {
    let on_disk = disk_usable();
    bind_pool(on_disk);
    let objects = prepare_live_storage(on_disk);
    let default = ADMIN_DEFAULT_PASSWORD.as_bytes();
    let cred = PasswordCred::derive(default);
    let mut table = TABLE.lock();
    install_live_table(&mut table, on_disk, objects);
    let admin = add_actor(&mut table, ADMIN_NAME, default, &cred).expect("galfs: admin");
    ADMIN_ROOT.store(admin, Ordering::Relaxed);
    mkdir_locked(&mut table, admin, "Desktop").expect("galfs: admin Desktop");
    drop(table);
    ACTIVE_SLOT.store(0, Ordering::Relaxed);
    ACTIVE_GEN.store(0, Ordering::Relaxed);
    DISK_CORRUPT.store(false, Ordering::Release);
}

fn format_fresh() {
    format_ram_table();
    // Fresh volume key for a sealed format (bring-up passphrase).
    let mut vk = [0u8; KEY_LEN];
    crate::arch::rand::fill_bytes(&mut vk);
    *VOLUME_KEY.lock() = Some(vk);
    wipe_bytes(&mut vk);
    store_passphrase(VOLUME_PASSPHRASE);
    crate::serial_println!(
        "[galfs] desk files {} slots {}",
        galexy_galf::DESK_COMPILE_FILES,
        inode_slots()
    );
    if sync_to_disk() {
        crate::serial_println!(
            "[galfs] formatted sealed disk (LBA base {})",
            disk_lba_base()
        );
    } else if !disk_usable() {
        crate::serial_println!("[galfs] RAM-only (no usable block device)");
    }
}

/// True when this boot is using a block device for the table.
/// Inodes this boot's table can name. A small disk is under the format ceiling.
pub fn inode_slots() -> u32 {
    TABLE.lock().geo.inode_slots
}

pub fn disk_backed() -> bool {
    DISK_LIVE.load(Ordering::Acquire)
}

/// True when the disk looked like GALF but no slot validated.
pub fn disk_corrupt() -> bool {
    DISK_CORRUPT.load(Ordering::Acquire)
}

/// Recovery events observed while loading (bad newer slot, older won).
pub fn recoveries() -> u64 {
    RECOVERIES.load(Ordering::Relaxed)
}

/// Active dual-slot index and generation after load/format.
pub fn disk_slot_info() -> (u32, u64) {
    (
        ACTIVE_SLOT.load(Ordering::Relaxed),
        ACTIVE_GEN.load(Ordering::Relaxed),
    )
}

/// True when a usable disk is present and no volume key is installed.
///
/// A corrupt image is not "locked" — unlock stays [`SysError::Unsupported`].
/// No disk means RAM-only, which is not locked.
pub fn volume_locked() -> bool {
    disk_usable() && !disk_corrupt() && VOLUME_KEY.lock().is_none()
}

/// `0` open or no disk, `1` sealed and locked, `2` no passphrase yet.
pub fn volume_query() -> u64 {
    if !volume_locked() {
        0
    } else if VOLUME_FRESH.load(Ordering::Acquire) {
        2
    } else {
        1
    }
}

/// True when a locked disk has no GALF header yet.
fn volume_fresh() -> bool {
    volume_locked() && VOLUME_FRESH.load(Ordering::Acquire)
}

/// Mount the sealed disk with `passphrase`, or format an empty image with it.
///
/// A wrong passphrase on a current-version GALF image does **not** set
/// [`disk_corrupt`]: the RAM table stays, disk sync fails, and a later
/// passphrase can retry. Already-unlocked and diskless boots succeed.
pub fn unlock_volume(passphrase: &[u8]) -> Result<(), SysError> {
    unlock_with(passphrase, None)
}

/// Like [`unlock_volume`], and on a fresh disk installs `admin_password`
/// before the first seal. The format default is not written to the image.
pub fn unlock_with(passphrase: &[u8], admin_password: Option<&[u8]>) -> Result<(), SysError> {
    if passphrase.is_empty() || passphrase.len() > PASSPHRASE_MAX {
        return Err(SysError::BadValue);
    }
    if let Some(password) = admin_password {
        if !password_ok(password) || password == ADMIN_DEFAULT_PASSWORD.as_bytes() {
            return Err(SysError::BadValue);
        }
    }
    if disk_corrupt() {
        return Err(SysError::Unsupported);
    }
    if !disk_usable() {
        return Ok(());
    }
    if VOLUME_KEY.lock().is_some() {
        return Ok(());
    }
    match load_from_disk(passphrase) {
        DiskLoad::Loaded { recovered } => {
            VOLUME_FRESH.store(false, Ordering::Release);
            store_passphrase(passphrase);
            let (slot, gen) = disk_slot_info();
            if recovered {
                RECOVERIES.fetch_add(1, Ordering::Relaxed);
                crate::serial_println!(
                    "[galfs] unlocked slot {} gen {} (recovered from bad sibling)",
                    slot,
                    gen,
                );
            } else {
                crate::serial_println!("[galfs] unlocked slot {} gen {}", slot, gen);
            }
            note_compiler_sysroot();
            Ok(())
        }
        DiskLoad::Empty => {
            if let Some(password) = admin_password {
                set_password(ADMIN_NAME, password)?;
            }
            store_passphrase(passphrase);
            let mut vk = [0u8; KEY_LEN];
            crate::arch::rand::fill_bytes(&mut vk);
            *VOLUME_KEY.lock() = Some(vk);
            wipe_bytes(&mut vk);
            if sync_to_disk() {
                VOLUME_FRESH.store(false, Ordering::Release);
                crate::serial_println!(
                    "[galfs] formatted sealed disk (LBA base {})",
                    disk_lba_base()
                );
                note_compiler_sysroot();
                Ok(())
            } else {
                // Leave the disk fresh. A key that never reached the
                // slot would make the next prompt look already open.
                wipe_volume_key();
                crate::serial_println!("[galfs] unlock format failed");
                Err(SysError::Unsupported)
            }
        }
        DiskLoad::Corrupt => {
            crate::serial_println!("[galfs] unlock failed; RAM-only");
            Err(SysError::AccessDenied)
        }
    }
}

/// Replace the passphrase that wraps the volume key and commit the slot.
///
/// A fresh locked disk (`current` empty) is formatted under `new`. A
/// sealed volume must already be unlocked, and `current` must match the
/// passphrase from this boot. The caller has already confirmed `new`.
pub fn set_volume_passphrase(current: &[u8], new: &[u8]) -> Result<(), SysError> {
    if new.is_empty() || new.len() > PASSPHRASE_MAX || current.len() > PASSPHRASE_MAX {
        return Err(SysError::BadValue);
    }
    if disk_corrupt() || !disk_usable() {
        return Err(SysError::Unsupported);
    }
    if volume_locked() {
        if volume_fresh() && current.is_empty() {
            return unlock_volume(new);
        }
        return Err(SysError::Unsupported);
    }
    if !passphrase_matches(current) {
        crate::serial_println!("[galfs] volume passphrase rejected");
        return Err(SysError::AccessDenied);
    }
    let mut old = [0u8; PASSPHRASE_MAX];
    let old_len = {
        let slot = VOLUME_PASS.lock();
        let n = slot.len;
        old[..n].copy_from_slice(&slot.bytes[..n]);
        n
    };
    store_passphrase(new);
    mark_dirty();
    if sync_to_disk() {
        wipe_bytes(&mut old);
        crate::serial_println!("[galfs] volume passphrase changed");
        Ok(())
    } else {
        store_passphrase(&old[..old_len]);
        wipe_bytes(&mut old);
        crate::serial_println!("[galfs] volume passphrase change failed");
        Err(SysError::Unsupported)
    }
}

fn passphrase_matches(pass: &[u8]) -> bool {
    let slot = VOLUME_PASS.lock();
    if slot.len == 0 || slot.len > PASSPHRASE_MAX {
        return false;
    }
    let stored = &slot.bytes[..slot.len];
    let mut diff = (stored.len() ^ pass.len()) as u8;
    let n = stored.len().max(pass.len());
    for i in 0..n {
        let a = stored.get(i).copied().unwrap_or(0);
        let b = pass.get(i).copied().unwrap_or(0);
        diff |= a ^ b;
    }
    diff == 0
}

/// True when either slot starts with the GALF magic. A foreign version
/// still counts: unlock refuses it instead of offering a new passphrase.
fn disk_has_seal() -> bool {
    let mut buf = DISK_BUF.lock();
    for slot in 0..DISK_SLOT_COUNT as u32 {
        if disk().read_sectors(slot_lba(slot), &mut buf.0).is_err() {
            continue;
        }
        if buf[0][0..4] == DISK_MAGIC {
            return true;
        }
    }
    false
}

/// Flush a dirty unlocked volume, then zero the key and passphrase.
///
/// No-op without a usable disk. Called when the last session logs out.
pub fn seal_and_lock() {
    if !disk_usable() {
        return;
    }
    let unlocked =
        x86_64::instructions::interrupts::without_interrupts(|| VOLUME_KEY.lock().is_some());
    if unlocked {
        sync_if_dirty();
    }
    wipe_volume_key();
}

/// Zero the volume key and stored passphrase. Disk image is left as last synced.
///
/// Residual RAM remanence after this wipe is an accepted cold-boot risk.
///
/// IRQ-gated: logout reaches here from the syscall path with IF=0,
/// and `VOLUME_KEY` / `VOLUME_PASS` are also taken by syscalls (IF=0) on
/// the same CPU — a preempted holder would wedge them (lock-audit rule).
pub fn wipe_volume_key() {
    x86_64::instructions::interrupts::without_interrupts(wipe_volume_key_gated);
}

fn wipe_volume_key_gated() {
    let mut had = false;
    {
        let mut key = VOLUME_KEY.lock();
        if let Some(bytes) = key.as_mut() {
            wipe_bytes(bytes);
            had = true;
        }
        *key = None;
    }
    {
        let mut pass = VOLUME_PASS.lock();
        if pass.len > 0 {
            wipe_bytes(&mut pass.bytes);
            pass.len = 0;
            had = true;
        }
    }
    if had {
        crate::serial_println!("[galfs] volume key wiped");
    }
}

fn store_passphrase(pass: &[u8]) {
    let mut slot = VOLUME_PASS.lock();
    wipe_bytes(&mut slot.bytes);
    let n = pass.len().min(PASSPHRASE_MAX);
    slot.bytes[..n].copy_from_slice(&pass[..n]);
    slot.len = n;
}

/// Writes the in-RAM table to the block device. No-op when RAM-only.
///
/// Always commits when disk-backed (tests / power / explicit barriers).
pub fn sync() {
    let _ = sync_to_disk();
}

/// Commits only when [`mark_dirty`] ran since the last successful flush.
pub fn sync_if_dirty() {
    if DIRTY.load(Ordering::Acquire) {
        let _ = sync_to_disk();
    }
}

/// Explicit flush for [`Syscall::Sync`]. RAM-only succeeds; corrupt fails.
pub fn sync_explicit() -> Result<(), SysError> {
    if DISK_CORRUPT.load(Ordering::Acquire) {
        return Err(SysError::Unsupported);
    }
    if !disk_usable() {
        DIRTY.store(false, Ordering::Release);
        return Ok(());
    }
    if sync_to_disk() {
        Ok(())
    } else {
        Err(SysError::Unsupported)
    }
}

/// Commits the table: encode under `DISK_BUF` + `TABLE`, then write and
/// flush the inactive slot.
///
/// IRQ-gated as a whole (galexy.os#86, second hang). The BSP main loop
/// calls this with IF=1 once a second when the table is dirty. Holding
/// `TABLE` / `DISK_BUF` preemptibly broke the lock-audit rule: the timer
/// switched the preempted main loop out for the shell thread on the same
/// CPU, whose next galfs syscall (the prompt's `whoami`, or `sync`) spun
/// IF=0 on a lock only the parked main loop could release. With the gate,
/// the encode runs to completion; the block I/O below re-enables
/// interrupts for its halt-wait on its own terms (`XFER_CPU` pins the CPU
/// to the holder), then returns with the gate intact.
fn sync_to_disk() -> bool {
    x86_64::instructions::interrupts::without_interrupts(sync_to_disk_gated)
}

fn sync_to_disk_gated() -> bool {
    if !disk_usable() || DISK_CORRUPT.load(Ordering::Acquire) {
        return false;
    }
    let next_gen = ACTIVE_GEN.load(Ordering::Relaxed).wrapping_add(1);
    let next_slot = 1 - ACTIVE_SLOT.load(Ordering::Relaxed);
    let lba = slot_lba(next_slot);
    let mut buf = DISK_BUF.lock();
    {
        let mut table = TABLE.lock();
        if !seal_dirty_blocks() {
            crate::serial_println!("[galfs] disk data write failed");
            return false;
        }
        if !flush_inodes(&mut table) {
            crate::serial_println!("[galfs] inode write failed");
            return false;
        }
        if !encode_table(&table, next_gen, &mut buf.0) {
            // A dirty RAM table on a locked volume stays dirty (nothing
            // can seal it) and the 1 Hz tick retries forever. Say so once
            // per lock state, not once a second for the rest of the boot.
            if !LOCKED_SKIP_LOGGED.swap(true, Ordering::AcqRel) {
                crate::serial_println!(
                    "[galfs] sync skipped: volume locked (RAM-only until unlock)"
                );
            }
            return false;
        }
    }
    LOCKED_SKIP_LOGGED.store(false, Ordering::Release);
    crate::serial_println!("[galfs] committing slot {}", next_slot);
    let d = disk();
    let reqs_before = crate::drivers::virtio_blk::requests();
    let t0 = crate::arch::timer_ticks();
    if d.write_sectors(lba, &buf.0).is_err() {
        crate::serial_println!("[galfs] disk sync write failed");
        return false;
    }
    let data_reqs = crate::drivers::virtio_blk::requests().saturating_sub(reqs_before);
    if d.flush().is_err() {
        crate::serial_println!("[galfs] disk flush failed");
        return false;
    }
    if crate::drivers::virtio_blk::present() {
        let ms = crate::arch::timer_ticks().saturating_sub(t0);
        crate::serial_println!("[galfs] sync data_reqs={} ms={}", data_reqs, ms);
    }
    publish_stable();
    ACTIVE_SLOT.store(next_slot, Ordering::Release);
    ACTIVE_GEN.store(next_gen, Ordering::Release);
    DISK_LIVE.store(true, Ordering::Release);
    DIRTY.store(false, Ordering::Release);
    true
}

enum DiskLoad {
    Loaded { recovered: bool },
    Empty,
    Corrupt,
}

/// Sizes the pool to the disk, or to the diskless RAM store.
fn bind_pool(on_disk: bool) {
    let mut pool = POOL.lock();
    pool.disk = on_disk;
    pool.blocks = if on_disk {
        let bytes = disk()
            .capacity_sectors()
            .saturating_sub(u64::from(disk_lba_base()))
            .saturating_mul(block::SECTOR as u64);
        galexy_galf::pool_blocks_for(bytes as usize)
    } else {
        DISKLESS_BLOCKS as u32
    };
    let n = pool.blocks as usize;
    pool.bitmap.clear();
    pool.bitmap.resize(n.div_ceil(8), 0);
    pool.gen.clear();
    pool.gen.resize(n, 0);
    pool.stable.clear();
    pool.stable.resize(n, false);
    pool.dirty.clear();
    pool.dirty.resize(n, false);
    pool.sealed.clear();
    pool.sealed.resize(n, false);
    pool.hold.clear();
    pool.hold.resize(n, 0);
    pool.clock = 1;
    pool.cursor = 0;
    drop(pool);
    BLOCK_SEEN.lock().resize(n.div_ceil(64), 0);
    let mut cache = CACHE.lock();
    cache.index = [u32::MAX; CACHE_SLOTS];
}

fn bitmap_set(bitmap: &[u8], index: usize) -> bool {
    let byte = index / 8;
    byte < bitmap.len() && (bitmap[byte] >> (index % 8)) & 1 != 0
}

fn extent_bitmap(table: &Table) -> alloc::vec::Vec<u8> {
    let n = table.geo.pool_blocks as usize;
    let mut bits = alloc::vec![0u8; n.div_ceil(8)];
    for obj in &table.objects {
        if obj.kind != KIND_FILE {
            continue;
        }
        for ext in obj.extents.iter().take(obj.extent_count as usize) {
            let mut b = ext.start;
            for _ in 0..ext.count {
                if (b as usize) < n {
                    bits[b as usize / 8] |= 1 << (b as usize % 8);
                }
                b = b.saturating_add(1);
            }
        }
    }
    bits
}

fn block_lba(index: u32) -> u32 {
    disk_lba_base()
        .saturating_add((DISK_SECTORS * DISK_SLOT_COUNT) as u32)
        .saturating_add(index.saturating_mul(galexy_galf::BLOCK_STRIDE as u32))
}

/// Writes one dirty block's ciphertext. A diskless pool copies into RAM.
fn store_block(pool: &mut Pool, index: u32, plain: &[u8; BLOCK_SIZE], domain: u8) -> bool {
    let i = index as usize;
    if i >= pool.blocks as usize {
        return false;
    }
    if !pool.disk {
        RAM_POOL.lock().blocks[i] = *plain;
        pool.sealed[i] = true;
        return true;
    }
    let Some(vk) = *VOLUME_KEY.lock() else {
        return false;
    };
    let gen = pool.gen[i];
    let mut cipher = *plain;
    let mut tag = [0u8; TAG_LEN];
    let ok = galexy_galf::seal_block(&vk, domain, i, gen, &mut cipher, &mut tag);
    let mut key = vk;
    wipe_bytes(&mut key);
    if !ok {
        return false;
    }
    let mut tag_sector = [0u8; block::SECTOR];
    tag_sector[0..4].copy_from_slice(&gen.to_le_bytes());
    tag_sector[4..4 + TAG_LEN].copy_from_slice(&tag);
    let mut data = [[0u8; block::SECTOR]; 8];
    for (n, sector) in data.iter_mut().enumerate() {
        let off = n * block::SECTOR;
        sector.copy_from_slice(&cipher[off..off + block::SECTOR]);
    }
    let lba = block_lba(index);
    let dev = disk();
    if dev.write_sectors(lba, &[tag_sector]).is_err() {
        return false;
    }
    if dev.write_sectors(lba + 1, &data).is_err() {
        return false;
    }
    pool.sealed[i] = true;
    true
}

fn load_block_bytes(pool: &Pool, index: u32, dst: &mut [u8; BLOCK_SIZE]) -> bool {
    let i = index as usize;
    if i >= pool.blocks as usize || pool.gen[i] == 0 {
        return false;
    }
    if !pool.disk {
        *dst = RAM_POOL.lock().blocks[i];
        return true;
    }
    let Some(vk) = *VOLUME_KEY.lock() else {
        return false;
    };
    let mut tag_sector = [[0u8; block::SECTOR]; 1];
    let mut data = [[0u8; block::SECTOR]; 8];
    let lba = block_lba(index);
    let dev = disk();
    if dev.read_sectors(lba, &mut tag_sector).is_err()
        || dev.read_sectors(lba + 1, &mut data).is_err()
    {
        let mut key = vk;
        wipe_bytes(&mut key);
        return false;
    }
    let gen = u32::from_le_bytes([
        tag_sector[0][0],
        tag_sector[0][1],
        tag_sector[0][2],
        tag_sector[0][3],
    ]);
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&tag_sector[0][4..4 + TAG_LEN]);
    for (n, sector) in data.iter().enumerate() {
        let off = n * block::SECTOR;
        dst[off..off + block::SECTOR].copy_from_slice(sector);
    }
    let ok = gen == pool.gen[i]
        && galexy_galf::open_block(&vk, galexy_galf::DOMAIN_DATA, i, gen, dst, &tag);
    let mut key = vk;
    wipe_bytes(&mut key);
    ok
}

fn cache_drop(cache: &mut BlockCache, slot: usize, pool: &mut Pool) -> bool {
    let index = cache.index[slot];
    if index == u32::MAX {
        return true;
    }
    let i = index as usize;
    if i < pool.dirty.len()
        && pool.dirty[i]
        && !pool.sealed[i]
        && !store_block(pool, index, &cache.data[slot], galexy_galf::DOMAIN_DATA)
    {
        return false;
    }
    cache.index[slot] = u32::MAX;
    true
}

fn cache_get(cache: &mut BlockCache, pool: &mut Pool, index: u32) -> Option<usize> {
    if let Some(slot) = cache.index.iter().position(|i| *i == index) {
        return Some(slot);
    }
    let slot = (0..CACHE_SLOTS)
        .find(|s| cache.index[*s] == u32::MAX)
        .unwrap_or(0);
    if !cache_drop(cache, slot, pool) {
        return None;
    }
    let i = index as usize;
    if pool.gen[i] == 0 || (pool.dirty[i] && !pool.sealed[i]) {
        cache.data[slot] = [0; BLOCK_SIZE];
    } else if !load_block_bytes(pool, index, &mut cache.data[slot]) {
        return None;
    }
    cache.index[slot] = index;
    Some(slot)
}

fn seal_dirty_blocks() -> bool {
    let mut pool = POOL.lock();
    if !pool.disk {
        return true;
    }
    let cache = CACHE.lock();
    for i in 0..pool.blocks as usize {
        if !pool.dirty[i] || pool.sealed[i] {
            continue;
        }
        let Some(slot) = (0..CACHE_SLOTS).find(|s| cache.index[*s] == i as u32) else {
            return false;
        };
        if !store_block(
            &mut pool,
            i as u32,
            &cache.data[slot],
            galexy_galf::DOMAIN_DATA,
        ) {
            return false;
        }
    }
    true
}

fn publish_stable() {
    let mut pool = POOL.lock();
    for i in 0..pool.blocks as usize {
        pool.dirty[i] = false;
        pool.sealed[i] = bitmap_set(&pool.bitmap, i);
        pool.stable[i] = pool.sealed[i];
        if pool.hold[i] > 0 {
            pool.hold[i] -= 1;
        }
    }
    pool.clock = pool.clock.saturating_add(1).max(1);
}

/// Live data blocks the pool will allocate.
pub fn pool_blocks() -> usize {
    POOL.lock().blocks as usize
}

fn load_from_disk(passphrase: &[u8]) -> DiskLoad {
    let mut best_gen = 0u64;
    let mut best_slot: Option<u32> = None;
    // True when a slot had GALF magic but failed decode, or the version
    // is not the current one. Either case refuses silent format.
    let mut saw_corrupt_current = false;
    let mut bad_with_magic = 0u32;
    let mut winner_key: Option<[u8; KEY_LEN]> = None;
    let mut sibling: Option<alloc::vec::Vec<u8>> = None;
    let mut clock = 1u32;
    let mut buf = DISK_BUF.lock();
    let mut best = LOAD_BEST.lock();
    let mut cand = LOAD_CAND.lock();
    for slot in 0..DISK_SLOT_COUNT as u32 {
        let lba = slot_lba(slot);
        if disk().read_sectors(lba, &mut buf.0).is_err() {
            continue;
        }
        let magic = buf[0][0..4] == DISK_MAGIC;
        let version = if magic {
            u16::from_le_bytes([buf[0][4], buf[0][5]])
        } else {
            0
        };
        if magic && version != DISK_VERSION {
            crate::serial_println!(
                "[galfs] slot {} version {} refused (need {}); old images are not migrated",
                slot,
                version,
                DISK_VERSION,
            );
            saw_corrupt_current = true;
            continue;
        }
        let Some((gen, key)) = decode_table(&mut buf.0, &mut cand, passphrase) else {
            if magic && version == DISK_VERSION {
                saw_corrupt_current = true;
                bad_with_magic += 1;
                crate::serial_println!("[galfs] slot {} rejected (decode)", slot);
            }
            continue;
        };
        if !verify_slot_blocks(&cand, &key) {
            let mut key = key;
            wipe_bytes(&mut key);
            saw_corrupt_current = true;
            bad_with_magic += 1;
            crate::serial_println!("[galfs] slot {} rejected (data)", slot);
            continue;
        }
        clock = clock.max(1);
        if !validate_table(&cand) {
            let mut key = key;
            wipe_bytes(&mut key);
            saw_corrupt_current = true;
            bad_with_magic += 1;
            crate::serial_println!("[galfs] slot {} failed validation", slot);
            continue;
        }
        if best_slot.is_none() || gen >= best_gen {
            if best_slot.is_some() {
                sibling = Some(extent_bitmap(&best));
            }
            if let Some(mut old) = winner_key.take() {
                wipe_bytes(&mut old);
            }
            winner_key = Some(key);
            best_gen = gen;
            best_slot = Some(slot);
            // Avoid `*best = *cand` — that materializes a Table on the stack.
            copy_table(&cand, &mut best);
        } else {
            sibling = Some(extent_bitmap(&cand));
            let mut key = key;
            wipe_bytes(&mut key);
        }
    }
    let Some(slot) = best_slot else {
        if let Some(mut key) = winner_key.take() {
            wipe_bytes(&mut key);
        }
        return if saw_corrupt_current {
            DiskLoad::Corrupt
        } else {
            DiskLoad::Empty
        };
    };
    if let Some(key) = winner_key {
        *VOLUME_KEY.lock() = Some(key);
    }
    let mut table = TABLE.lock();
    copy_table(&best, &mut table);
    install_pool(&table, sibling.as_deref(), clock);
    if let Some(key) = VOLUME_KEY.lock().as_ref().copied() {
        let _ = load_inode_blocks(&mut table, &key);
        let mut wipe = key;
        wipe_bytes(&mut wipe);
    }
    pin_meta_blocks(&table);
    refresh_roots(&table);
    drop(table);
    // The pre-unlock import, if any, addressed the RAM table. Those
    // indexes now name whatever the sealed image stored.
    super::large::reset();
    ACTIVE_SLOT.store(slot, Ordering::Release);
    ACTIVE_GEN.store(best_gen, Ordering::Release);
    DISK_LIVE.store(true, Ordering::Release);
    DiskLoad::Loaded {
        recovered: bad_with_magic > 0,
    }
}

/// Structural + bitmap consistency check on the live table (fsck smoke).
pub fn fsck_ok() -> bool {
    let table = TABLE.lock();
    validate_table(&table)
}

fn copy_table(src: &Table, dst: &mut Table) {
    dst.actors = src.actors;
    dst.shares = src.shares;
    dst.geo = src.geo;
    dst.objects.clone_from(&src.objects);
}

/// Iteration count written by `format` and `passwd` on this boot.
///
/// Test kernels (`debug_assertions`) and TCG stay at 10 000 so the suite
/// fits the runner timeout. A `--release` kernel under KVM stores
/// [`galexy_crypto::PBKDF2_ITERS_RELEASE`].
fn password_kdf_iters() -> u32 {
    if cfg!(not(debug_assertions)) && crate::arch::cpu::under_kvm() {
        galexy_crypto::PBKDF2_ITERS_RELEASE
    } else {
        galexy_crypto::PBKDF2_ITERS
    }
}

fn refresh_roots(table: &Table) {
    let mut admin = NO_OBJECT;
    for actor in &table.actors {
        if actor.used && actor.name_is(ADMIN_NAME) {
            admin = actor.root;
            break;
        }
    }
    ADMIN_ROOT.store(admin, Ordering::Relaxed);
}

fn export_actor(actor: &Actor) -> galexy_galf::Actor {
    galexy_galf::Actor {
        used: actor.used,
        name: actor.name,
        name_len: actor.name_len,
        root: actor.root,
        salt: actor.salt,
        pass_hash: actor.pass_hash,
        kdf_iters: actor.kdf_iters,
        max_objects: actor.max_objects,
        max_bytes: actor.max_bytes,
    }
}

fn import_actor(actor: &galexy_galf::Actor) -> Actor {
    Actor {
        used: actor.used,
        name: actor.name,
        name_len: actor.name_len,
        root: actor.root,
        salt: actor.salt,
        pass_hash: actor.pass_hash,
        kdf_iters: actor.kdf_iters,
        max_objects: actor.max_objects,
        max_bytes: actor.max_bytes,
        used_objects: 0,
        used_bytes: 0,
    }
}

fn export_share(share: &Share) -> galexy_galf::Share {
    galexy_galf::Share {
        used: share.used,
        rights: share.rights,
        grantee: share.grantee,
        object: share.object,
    }
}

fn export_object(obj: &Object) -> galexy_galf::Object {
    galexy_galf::Object {
        kind: obj.kind,
        parent: obj.parent,
        actor: obj.actor,
        name: obj.name,
        name_len: obj.name_len,
        len: obj.len,
        extent_count: obj.extent_count,
        extents: obj.extents,
        child: obj.child,
        next: obj.next,
    }
}

fn import_object(obj: galexy_galf::Object) -> Object {
    Object {
        kind: obj.kind,
        parent: obj.parent,
        actor: obj.actor,
        name: obj.name,
        name_len: obj.name_len,
        len: obj.len,
        extent_count: obj.extent_count,
        extents: obj.extents,
        child: obj.child,
        next: obj.next,
    }
}

/// Where the previous commit left inode and map blocks.
struct MetaLoc {
    inode_at: alloc::vec::Vec<u32>,
    inode_gen: alloc::vec::Vec<u32>,
}

static META_LOC: Mutex<MetaLoc> = Mutex::new(MetaLoc {
    inode_at: alloc::vec::Vec::new(),
    inode_gen: alloc::vec::Vec::new(),
});

fn read_pool_plain(
    index: u32,
    gen: u32,
    domain: u8,
    key: &[u8; KEY_LEN],
) -> Option<[u8; BLOCK_SIZE]> {
    let mut tag_sector = [[0u8; block::SECTOR]; 1];
    let mut data = [[0u8; block::SECTOR]; 8];
    let lba = block_lba(index);
    if disk().read_sectors(lba, &mut tag_sector).is_err()
        || disk().read_sectors(lba + 1, &mut data).is_err()
    {
        return None;
    }
    let stored = u32::from_le_bytes([
        tag_sector[0][0],
        tag_sector[0][1],
        tag_sector[0][2],
        tag_sector[0][3],
    ]);
    if stored != gen {
        return None;
    }
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&tag_sector[0][4..4 + TAG_LEN]);
    let mut block = [0u8; BLOCK_SIZE];
    for (n, sector) in data.iter().enumerate() {
        let off = n * block::SECTOR;
        block[off..off + block::SECTOR].copy_from_slice(sector);
    }
    if !galexy_galf::open_block(key, domain, index as usize, gen, &mut block, &tag) {
        return None;
    }
    Some(block)
}

fn load_inode_blocks(table: &mut Table, key: &[u8; KEY_LEN]) -> bool {
    let slots = table.geo.inode_slots as usize;
    if slots == 0 || slots > OBJECT_SLOTS {
        return false;
    }
    table.objects.resize(slots, Object::empty());
    let mut loc = META_LOC.lock();
    loc.inode_at.clear();
    loc.inode_gen.clear();
    let map_len = table.geo.map_len as usize;
    if map_len == 0 {
        return true;
    }
    for m in 0..map_len.min(galexy_galf::MAP_POINTERS) {
        let gen = table.geo.map_gen[m];
        let index = table.geo.map_at[m];
        if gen == 0 {
            continue;
        }
        let Some(map_plain) = read_pool_plain(index, gen, galexy_galf::DOMAIN_MAP, key) else {
            return false;
        };
        for e in 0..galexy_galf::MAP_PER_BLOCK {
            let ib = m * galexy_galf::MAP_PER_BLOCK + e;
            let off = e * 8;
            let at = u32::from_le_bytes(map_plain[off..off + 4].try_into().unwrap_or([0; 4]));
            let igen = u32::from_le_bytes(map_plain[off + 4..off + 8].try_into().unwrap_or([0; 4]));
            if loc.inode_at.len() <= ib {
                loc.inode_at.resize(ib + 1, 0);
                loc.inode_gen.resize(ib + 1, 0);
            }
            loc.inode_at[ib] = at;
            loc.inode_gen[ib] = igen;
            if igen == 0 {
                continue;
            }
            let Some(plain) = read_pool_plain(at, igen, galexy_galf::DOMAIN_INODE, key) else {
                return false;
            };
            for n in 0..galexy_galf::INODES_PER_BLOCK {
                let id = ib * galexy_galf::INODES_PER_BLOCK + n;
                if id >= slots {
                    break;
                }
                let ioff = n * galexy_galf::INODE_ON_DISK;
                let Some(obj) = galexy_galf::read_inode(&plain[ioff..]) else {
                    return false;
                };
                table.objects[id] = import_object(obj);
            }
        }
    }
    true
}

fn drop_cache_line(index: u32) {
    let mut pool = POOL.lock();
    if (index as usize) < pool.dirty.len() {
        pool.dirty[index as usize] = false;
        pool.sealed[index as usize] = true;
    }
    drop(pool);
    let mut cache = CACHE.lock();
    for slot in &mut cache.index {
        if *slot == index {
            *slot = u32::MAX;
        }
    }
}

/// Writes inode blocks that hold a live name, then the map block they live in.
fn flush_inodes(table: &mut Table) -> bool {
    if !POOL.lock().disk {
        return true;
    }
    let slots = table.objects.len();
    let per = galexy_galf::INODES_PER_BLOCK;
    let blocks = slots.div_ceil(per);
    let mut any = false;
    for ib in 0..blocks {
        let base = ib * per;
        let live = (0..per).any(|n| base + n < slots && table.objects[base + n].kind != KIND_EMPTY);
        if !live {
            continue;
        }
        let mut plain = [0u8; BLOCK_SIZE];
        for n in 0..per {
            let id = base + n;
            if id >= slots {
                break;
            }
            let exported = export_object(&table.objects[id]);
            galexy_galf::write_inode(&mut plain[n * galexy_galf::INODE_ON_DISK..], &exported);
        }
        let (old_at, old_gen) = {
            let loc = META_LOC.lock();
            (
                loc.inode_at.get(ib).copied().unwrap_or(0),
                loc.inode_gen.get(ib).copied().unwrap_or(0),
            )
        };
        let reuse = old_gen != 0
            && !POOL
                .lock()
                .stable
                .get(old_at as usize)
                .copied()
                .unwrap_or(true);
        let index = if reuse {
            old_at
        } else {
            let Some(fresh) = alloc_block() else {
                return false;
            };
            if old_gen != 0 {
                free_block(old_at);
            }
            fresh
        };
        let gen = POOL.lock().gen.get(index as usize).copied().unwrap_or(0);
        if gen == 0 {
            return false;
        }
        {
            let mut pool = POOL.lock();
            if !store_block(&mut pool, index, &plain, galexy_galf::DOMAIN_INODE) {
                return false;
            }
        }
        drop_cache_line(index);
        {
            let mut loc = META_LOC.lock();
            if loc.inode_at.len() <= ib {
                loc.inode_at.resize(ib + 1, 0);
                loc.inode_gen.resize(ib + 1, 0);
            }
            loc.inode_at[ib] = index;
            loc.inode_gen[ib] = gen;
        }
        any = true;
    }
    if !any {
        return true;
    }
    let span = galexy_galf::MAP_POINTERS * galexy_galf::MAP_PER_BLOCK;
    if blocks > span {
        return false;
    }
    let old_at = table.geo.map_at;
    let old_gen = table.geo.map_gen;
    let old_len = table.geo.map_len as usize;
    let mut highest = 0usize;
    {
        let loc = META_LOC.lock();
        for (ib, gen) in loc.inode_gen.iter().take(blocks).copied().enumerate() {
            if gen != 0 {
                highest = ib / galexy_galf::MAP_PER_BLOCK + 1;
            }
        }
    }
    let map_count = highest.min(galexy_galf::MAP_POINTERS);
    for m in 0..map_count {
        let mut map = [0u8; BLOCK_SIZE];
        {
            let loc = META_LOC.lock();
            for e in 0..galexy_galf::MAP_PER_BLOCK {
                let ib = m * galexy_galf::MAP_PER_BLOCK + e;
                if ib >= loc.inode_gen.len() || loc.inode_gen[ib] == 0 {
                    continue;
                }
                let off = e * 8;
                map[off..off + 4].copy_from_slice(&loc.inode_at[ib].to_le_bytes());
                map[off + 4..off + 8].copy_from_slice(&loc.inode_gen[ib].to_le_bytes());
            }
        }
        let reuse = m < old_len
            && old_gen[m] != 0
            && !POOL
                .lock()
                .stable
                .get(old_at[m] as usize)
                .copied()
                .unwrap_or(true);
        let index = if reuse {
            old_at[m]
        } else {
            let Some(fresh) = alloc_block() else {
                return false;
            };
            if m < old_len && old_gen[m] != 0 {
                free_block(old_at[m]);
            }
            fresh
        };
        let gen = POOL.lock().gen.get(index as usize).copied().unwrap_or(0);
        if gen == 0 {
            return false;
        }
        {
            let mut pool = POOL.lock();
            if !store_block(&mut pool, index, &map, galexy_galf::DOMAIN_MAP) {
                return false;
            }
        }
        drop_cache_line(index);
        table.geo.map_at[m] = index;
        table.geo.map_gen[m] = gen;
    }
    for m in map_count..old_len.min(galexy_galf::MAP_POINTERS) {
        if old_gen[m] != 0 {
            free_block(old_at[m]);
        }
        table.geo.map_at[m] = 0;
        table.geo.map_gen[m] = 0;
    }
    table.geo.map_len = map_count as u32;
    true
}

fn encode_table(
    table: &Table,
    generation: u64,
    sectors: &mut [[u8; block::SECTOR]; DISK_SECTORS],
) -> bool {
    // KEY then PASS (same order as [`wipe_volume_key`]).
    let (mut vk, mut pass_buf, pass_len) = {
        let key_guard = VOLUME_KEY.lock();
        let Some(key) = *key_guard else {
            return false;
        };
        let pass = VOLUME_PASS.lock();
        let mut buf = [0u8; PASSPHRASE_MAX];
        let n = pass.len.min(PASSPHRASE_MAX);
        if n > 0 {
            buf[..n].copy_from_slice(&pass.bytes[..n]);
        }
        (key, buf, n)
    };
    let flat = sectors_flat_mut(sectors);
    flat.fill(0);
    flat[0..4].copy_from_slice(&DISK_MAGIC);
    flat[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    flat[6..8].copy_from_slice(&(ACTOR_SLOTS as u16).to_le_bytes());
    flat[8..10].copy_from_slice(&(SHARE_SLOTS as u16).to_le_bytes());
    flat[14] = 1; // sealed
    flat[16..24].copy_from_slice(&generation.to_le_bytes());

    galexy_galf::write_geometry(
        &mut flat[DISK_HEADER..DISK_HEADER + galexy_galf::GEOMETRY_ON_DISK],
        &table.geo,
    );
    let mut off = DISK_HEADER + galexy_galf::GEOMETRY_ON_DISK;
    for actor in &table.actors {
        galexy_galf::write_actor(&mut flat[off..off + ACTOR_ON_DISK], &export_actor(actor));
        off += ACTOR_ON_DISK;
    }
    for share in &table.shares {
        galexy_galf::write_share(&mut flat[off..off + SHARE_ON_DISK], &export_share(share));
        off += SHARE_ON_DISK;
    }
    galexy_galf::write_free(
        &mut flat[off..off + galexy_galf::FREE_EXTENTS * 12],
        &table.geo.free,
    );
    off += galexy_galf::FREE_EXTENTS * 12;
    galexy_galf::write_map_ptrs(
        &mut flat[off..off + galexy_galf::MAP_POINTERS * 8],
        &table.geo.map_at,
        &table.geo.map_gen,
    );
    debug_assert_eq!(
        off + galexy_galf::MAP_POINTERS * 8,
        DISK_HEADER + PAYLOAD_LEN
    );
    debug_assert!(off <= flat.len());

    let aad = seal_aad(generation);

    // Re-wrap with the passphrase stored for this boot. An empty slot
    // is the test-harness passphrase; production stores one before sealing.
    let mut kdf_salt = [0u8; SALT_LEN];
    let mut wrap_nonce = [0u8; NONCE_LEN];
    let mut data_nonce = [0u8; NONCE_LEN];
    crate::arch::rand::fill_bytes(&mut kdf_salt);
    crate::arch::rand::fill_bytes(&mut wrap_nonce);
    crate::arch::rand::fill_bytes(&mut data_nonce);
    let mut kek = [0u8; KEY_LEN];
    let phrase: &[u8] = if pass_len == 0 {
        VOLUME_PASSPHRASE
    } else {
        &pass_buf[..pass_len]
    };
    derive_key(phrase, &kdf_salt, &mut kek);
    wipe_bytes(&mut pass_buf);
    let mut wrapped = vk;
    let mut wrap_tag = [0u8; TAG_LEN];
    seal(&kek, &wrap_nonce, &aad, &mut wrapped, &mut wrap_tag);
    wipe_bytes(&mut kek);

    flat[32..40].copy_from_slice(&kdf_salt);
    flat[40..52].copy_from_slice(&wrap_nonce);
    flat[52..84].copy_from_slice(&wrapped);
    flat[84..100].copy_from_slice(&wrap_tag);
    wipe_bytes(&mut wrapped);

    // Encrypt payload in place.
    let mut data_tag = [0u8; TAG_LEN];
    seal(
        &vk,
        &data_nonce,
        &aad,
        &mut flat[DISK_HEADER..DISK_HEADER + PAYLOAD_LEN],
        &mut data_tag,
    );
    flat[100..112].copy_from_slice(&data_nonce);
    flat[112..128].copy_from_slice(&data_tag);

    let sum = crc32(&flat[DISK_HEADER..DISK_HEADER + PAYLOAD_LEN]);
    flat[24..28].copy_from_slice(&sum.to_le_bytes());
    wipe_bytes(&mut vk);
    true
}

fn seal_aad(generation: u64) -> [u8; 14] {
    let mut aad = [0u8; 14];
    aad[0..4].copy_from_slice(&DISK_MAGIC);
    aad[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    aad[6..14].copy_from_slice(&generation.to_le_bytes());
    aad
}

/// Decodes a sealed slot. Returns the generation and volume key when unlock
/// and AEAD succeed. File bytes are loaded separately from the data region.
/// Decrypts the payload in place in `sectors`. The caller wipes the key.
fn decode_table(
    sectors: &mut [[u8; block::SECTOR]; DISK_SECTORS],
    table: &mut Table,
    passphrase: &[u8],
) -> Option<(u64, [u8; KEY_LEN])> {
    let flat = sectors_flat_mut(sectors);
    if flat[0..4] != DISK_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([flat[4], flat[5]]);
    let actors = u16::from_le_bytes([flat[6], flat[7]]) as usize;
    let shares_n = u16::from_le_bytes([flat[8], flat[9]]) as usize;
    if version != DISK_VERSION
        || actors != ACTOR_SLOTS
        || shares_n != SHARE_SLOTS
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

    let geo = galexy_galf::read_geometry_head(&flat[DISK_HEADER..])?;
    table.geo = geo;
    let mut off = DISK_HEADER + galexy_galf::GEOMETRY_ON_DISK;
    for actor in &mut table.actors {
        let got = galexy_galf::read_actor(&flat[off..])?;
        *actor = import_actor(&got);
        off += ACTOR_ON_DISK;
    }
    for share in &mut table.shares {
        let got = galexy_galf::read_share(&flat[off..])?;
        *share = Share {
            used: got.used,
            grantee: got.grantee,
            rights: got.rights,
            object: got.object,
        };
        off += SHARE_ON_DISK;
    }
    if !galexy_galf::read_free(&flat[off..], &mut table.geo.free) {
        let mut gone = vk;
        wipe_bytes(&mut gone);
        return None;
    }
    off += galexy_galf::FREE_EXTENTS * 12;
    if !galexy_galf::read_map_ptrs(&flat[off..], &mut table.geo.map_at, &mut table.geo.map_gen) {
        let mut gone = vk;
        wipe_bytes(&mut gone);
        return None;
    }
    table
        .objects
        .resize(table.geo.inode_slots as usize, Object::empty());
    if !load_inode_blocks(table, &vk) {
        let mut gone = vk;
        wipe_bytes(&mut gone);
        return None;
    }
    Some((generation, vk))
}

fn verify_slot_blocks(table: &Table, volume_key: &[u8; KEY_LEN]) -> bool {
    let _ = volume_key;
    for obj in &table.objects {
        if obj.kind != KIND_FILE {
            continue;
        }
        for ext in obj.extents.iter().take(obj.extent_count as usize) {
            let end = ext.start.saturating_add(ext.count);
            if ext.count == 0 || ext.gen == 0 || end > table.geo.pool_blocks {
                return false;
            }
        }
    }
    true
}

/// Marks committed blocks stable and reserves indexes the sibling slot
/// still names, so the next commit can replace that slot before reuse.
fn pin_meta_blocks(table: &Table) {
    let loc = META_LOC.lock();
    let inodes: alloc::vec::Vec<(u32, u32)> = loc
        .inode_at
        .iter()
        .copied()
        .zip(loc.inode_gen.iter().copied())
        .collect();
    drop(loc);
    let mut pool = POOL.lock();
    let pin = |pool: &mut Pool, at: u32, gen: u32| {
        if gen == 0 {
            return;
        }
        let i = at as usize;
        if i >= pool.blocks as usize || i / 8 >= pool.bitmap.len() {
            return;
        }
        pool.bitmap[i / 8] |= 1 << (i % 8);
        pool.gen[i] = gen;
        pool.stable[i] = true;
        pool.sealed[i] = true;
        pool.clock = pool.clock.max(gen.saturating_add(1));
    };
    for (at, gen) in inodes {
        pin(&mut pool, at, gen);
    }
    for m in 0..table.geo.map_len as usize {
        pin(&mut pool, table.geo.map_at[m], table.geo.map_gen[m]);
    }
}

fn install_pool(table: &Table, sibling: Option<&[u8]>, clock: u32) {
    let mut pool = POOL.lock();
    pool.disk = true;
    pool.blocks = table.geo.pool_blocks;
    let n = pool.blocks as usize;
    pool.bitmap = extent_bitmap(table);
    pool.dirty.clear();
    pool.dirty.resize(n, false);
    pool.sealed.clear();
    pool.sealed.resize(n, false);
    pool.stable.clear();
    pool.stable.resize(n, false);
    pool.hold.clear();
    pool.hold.resize(n, 0);
    pool.gen.clear();
    pool.gen.resize(n, 0);
    let mut next = clock.max(1);
    for obj in &table.objects {
        if obj.kind != KIND_FILE {
            continue;
        }
        for ext in obj.extents.iter().take(obj.extent_count as usize) {
            let mut b = ext.start;
            for _ in 0..ext.count {
                if (b as usize) < n && ext.gen != 0 {
                    pool.gen[b as usize] = ext.gen;
                    pool.stable[b as usize] = true;
                    pool.sealed[b as usize] = true;
                    next = next.max(ext.gen.saturating_add(1));
                }
                b = b.saturating_add(1);
            }
        }
    }
    if let Some(sib) = sibling {
        for i in 0..pool.blocks as usize {
            if bitmap_set(sib, i) && !bitmap_set(&pool.bitmap, i) {
                pool.hold[i] = 1;
            }
        }
    }
    pool.clock = next.max(1);
    drop(pool);
    CACHE.lock().index = [u32::MAX; CACHE_SLOTS];
}

/// Structural checks after CRC so a bit-flipped-but-checksum-ok image cannot
/// take the kernel into undefined object walks.
fn validate_table(table: &Table) -> bool {
    let mut saw_admin = false;
    for (ai, actor) in table.actors.iter().enumerate() {
        if !actor.used {
            continue;
        }
        if actor.name_len == 0 || actor.name_len as usize > ACTOR_NAME {
            return false;
        }
        let root = actor.root as usize;
        if root >= table.objects.len() {
            return false;
        }
        let obj = &table.objects[root];
        if obj.kind != KIND_DIR || obj.parent != NO_PARENT || obj.actor != ai as u8 {
            return false;
        }
        if actor.name_is(ADMIN_NAME) {
            saw_admin = true;
        }
    }
    if !saw_admin {
        return false;
    }
    for share in &table.shares {
        if !share.used {
            continue;
        }
        if share.rights == 0
            || share.grantee as usize >= ACTOR_SLOTS
            || !table.actors[share.grantee as usize].used
            || share.object as usize >= table.objects.len()
            || table.objects[share.object as usize].kind == KIND_EMPTY
        {
            return false;
        }
    }
    let mut seen = BLOCK_SEEN.lock();
    let words = (table.geo.pool_blocks as usize).div_ceil(64).max(1);
    if seen.len() < words {
        seen.resize(words, 0);
    }
    seen[..words].fill(0);
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.kind != KIND_FILE && obj.kind != KIND_DIR {
            return false;
        }
        if obj.actor as usize >= ACTOR_SLOTS || !table.actors[obj.actor as usize].used {
            return false;
        }
        if obj.parent == NO_PARENT {
            // Must be some actor's root.
            if !table.actors.iter().any(|a| a.used && a.root == i as u32) {
                return false;
            }
        } else {
            let parent = obj.parent as usize;
            if parent >= table.objects.len() || parent == i {
                return false;
            }
            if table.objects[parent].kind != KIND_DIR {
                return false;
            }
            // Walk to root; refuse cycles.
            let mut cur = obj.parent;
            for _ in 0..table.objects.len() {
                if cur == i as u32 {
                    return false;
                }
                let p = table.objects[cur as usize].parent;
                if p == NO_PARENT {
                    break;
                }
                if p as usize >= table.objects.len() {
                    return false;
                }
                cur = p;
            }
        }
        if !validate_object_blocks(table, obj, &mut seen[..]) {
            return false;
        }
    }
    directories_ok(table) && super::large::ok()
}

/// Sibling lists are the directory. Parent pointers are what cards walk.
/// The two have to name the same children, with no cycles and no orphans.
fn directories_ok(table: &Table) -> bool {
    let n = table.objects.len();
    let mut bits = DIR_SEEN.lock();
    let words = n.div_ceil(64);
    if bits.len() < words {
        bits.resize(words, 0);
    }
    bits[..words].fill(0);
    for i in 0..n {
        let obj = &table.objects[i];
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.kind != KIND_DIR && obj.child != NO_OBJECT {
            return false;
        }
        if obj.kind != KIND_DIR {
            continue;
        }
        let mut child = obj.child;
        let mut guard = 0usize;
        while child != NO_OBJECT {
            if child as usize >= n || bits[child as usize / 64] & (1u64 << (child % 64)) != 0 {
                return false;
            }
            bits[child as usize / 64] |= 1u64 << (child % 64);
            let entry = &table.objects[child as usize];
            if entry.kind == KIND_EMPTY || entry.parent != i as u32 {
                return false;
            }
            child = entry.next;
            guard += 1;
            if guard > table.objects.len() {
                return false;
            }
        }
    }
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY {
            continue;
        }
        if obj.parent == NO_PARENT {
            if obj.next != NO_OBJECT {
                return false;
            }
            continue;
        }
        if bits[i / 64] & (1u64 << (i % 64)) == 0 {
            return false;
        }
    }
    true
}

fn mark_block(seen: &mut [u64], block: u32, pool: u32) -> bool {
    if block >= pool || (block as usize) / 64 >= seen.len() {
        return false;
    }
    let i = block as usize;
    let bit = 1u64 << (i % 64);
    if seen[i / 64] & bit != 0 {
        return false;
    }
    seen[i / 64] |= bit;
    true
}

fn validate_object_blocks(table: &Table, obj: &Object, seen: &mut [u64]) -> bool {
    if obj.kind != KIND_FILE {
        return obj.len == 0 && obj.extent_count == 0;
    }
    if obj.len > table.geo.file_max.max(1) || obj.extent_count as usize > EXTENTS_PER_FILE {
        return false;
    }
    let need = (obj.len as usize).div_ceil(BLOCK_SIZE);
    let mut covered = 0u64;
    for ext in obj.extents.iter().take(obj.extent_count as usize) {
        if ext.count == 0 {
            return false;
        }
        covered += u64::from(ext.count);
        let mut b = ext.start;
        for _ in 0..ext.count {
            if !mark_block(seen, b, table.geo.pool_blocks) {
                return false;
            }
            b = b.saturating_add(1);
        }
    }
    covered >= need as u64
}

fn sectors_flat_mut(sectors: &mut [[u8; block::SECTOR]; DISK_SECTORS]) -> &mut [u8] {
    // SAFETY: `[[u8; SECTOR]; N]` is contiguous bytes with no padding.
    unsafe {
        core::slice::from_raw_parts_mut(
            sectors.as_mut_ptr().cast::<u8>(),
            DISK_SECTORS * block::SECTOR,
        )
    }
}

/// True when `index` is `root` or a descendant of it.
pub fn belongs_to_root(index: u32, root: u32) -> bool {
    let table = TABLE.lock();
    covers_object(&table, root, index)
}

/// Object indices that make up an actor's tree (root first). At most a
/// handful for the empty-delete path (root + optional Desktop).
pub fn collect_actor_objects(root: u32, out: &mut [u32]) -> usize {
    let table = TABLE.lock();
    if root as usize >= table.objects.len() || table.objects[root as usize].kind == KIND_EMPTY {
        return 0;
    }
    let mut n = 0usize;
    if n < out.len() {
        out[n] = root;
        n += 1;
    }
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY || obj.parent != root {
            continue;
        }
        if n < out.len() {
            out[n] = i as u32;
            n += 1;
        }
    }
    n
}

pub use galexy_galf::cards::drop_tokens_on;

/// Credentials for the default boot actor.
pub fn admin_cred() -> FsCred {
    let root = ADMIN_ROOT.load(Ordering::Relaxed);
    debug_assert!(root != NO_OBJECT, "galfs: init before admin_cred");
    FsCred::launcher(root)
}

/// Pre-login / logged-out seat: no actor root and no tokens.
pub fn unauth_cred() -> FsCred {
    FsCred::none()
}

/// Admin's actor root object index.
pub fn admin_root() -> u32 {
    ADMIN_ROOT.load(Ordering::Relaxed)
}

/// Creates `dir/name` under the admin root when it is missing.
///
/// A network name and a USB port both fit the existing object name
/// (`NAME_CAP` is 64). This does not change `DISK_VERSION`.
pub(crate) fn ensure_card(dir: &str, name: &str) -> Result<u32, SysError> {
    if !component_ok(dir) || !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let root = admin_root();
    if root == NO_OBJECT || (root as usize) >= table.objects.len() {
        return Err(SysError::NotFound);
    }
    let dir_index = match find_child(&table, root, dir) {
        Some(i) => {
            if table.objects[i].kind != KIND_DIR {
                return Err(SysError::Unsupported);
            }
            i as u32
        }
        None => make_child(&mut table, root, dir, KIND_DIR)?,
    };
    let file = match find_child(&table, dir_index, name) {
        Some(i) => {
            if table.objects[i].kind != KIND_FILE {
                return Err(SysError::Unsupported);
            }
            i as u32
        }
        None => make_child(&mut table, dir_index, name, KIND_FILE)?,
    };
    drop(table);
    mark_dirty();
    Ok(file)
}

fn make_child(table: &mut Table, parent: u32, name: &str, kind: u8) -> Result<u32, SysError> {
    let actor = table.objects[parent as usize].actor as usize;
    if !can_add_object(table, actor) {
        return Err(SysError::NoResource);
    }
    let Some(oi) = free_object(table) else {
        return Err(SysError::NoResource);
    };
    let obj = &mut table.objects[oi];
    *obj = Object::empty();
    obj.kind = kind;
    obj.parent = parent;
    obj.actor = actor as u8;
    place_name(obj, name);
    link_child(table, parent, oi as u32);
    Ok(oi as u32)
}

/// Whether `root` is the immortal admin actor root.
pub fn is_admin_root(root: u32) -> bool {
    root != NO_OBJECT && root == ADMIN_ROOT.load(Ordering::Relaxed)
}

/// Writes the actor name for `root` into `out`. Returns the byte count.
///
/// [`NO_OBJECT`] (logged out) is [`SysError::AccessDenied`].
pub fn name_of_root(root: u32, out: &mut [u8]) -> Result<usize, SysError> {
    if root == NO_OBJECT {
        return Err(SysError::AccessDenied);
    }
    let table = TABLE.lock();
    let actor = table
        .actors
        .iter()
        .find(|a| a.used && a.root == root)
        .ok_or(SysError::NotFound)?;
    let n = actor.name_len as usize;
    if n > out.len() {
        return Err(SysError::BadBuffer);
    }
    out[..n].copy_from_slice(&actor.name[..n]);
    Ok(n)
}

/// Calls `each` with every live actor name.
pub fn for_each_actor(mut each: impl FnMut(&[u8])) {
    let table = TABLE.lock();
    for actor in &table.actors {
        if !actor.used {
            continue;
        }
        let n = actor.name_len as usize;
        each(&actor.name[..n]);
    }
}

/// Looks up an actor's root by name.
pub fn root_named(name: &str) -> Result<u32, SysError> {
    let table = TABLE.lock();
    find_actor_root(&table, name)
}

/// Creates an actor and an empty Desktop. Returns the new root.
pub fn add_user(name: &str, password: &[u8]) -> Result<u32, SysError> {
    if name == ADMIN_NAME {
        return Err(SysError::Unsupported);
    }
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    // KDF before the lock (see `PasswordCred`).
    let mut cred = PasswordCred::derive(password);
    let mut table = TABLE.lock();
    let result = add_actor(&mut table, name, password, &cred)
        .and_then(|root| mkdir_locked(&mut table, root, "Desktop").map(|_| root));
    drop(table);
    cred.wipe();
    result
}

/// True when `password` verifies for actor `name`.
///
/// The table lock is held only to copy the stored salt/cost/hash; the
/// derivation runs unlocked. A concurrent `passwd` on the same actor
/// races benignly: the check runs against whichever credential was
/// stored when the copy was taken.
pub fn verify_password(name: &str, password: &[u8]) -> Result<bool, SysError> {
    let mut cred = {
        let table = TABLE.lock();
        let Some(actor) = table.actors.iter().find(|a| a.used && a.name_is(name)) else {
            return Err(SysError::NotFound);
        };
        actor.password_cred()
    };
    let ok = cred.check(password);
    cred.wipe();
    Ok(ok)
}

/// Sets the password for actor `name`.
///
/// Derives first (unlocked), then takes the table lock only to store the
/// new salt/cost/hash.
pub fn set_password(name: &str, password: &[u8]) -> Result<(), SysError> {
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    let exists = TABLE
        .lock()
        .actors
        .iter()
        .any(|a| a.used && a.name_is(name));
    if !exists {
        return Err(SysError::NotFound);
    }
    let mut cred = PasswordCred::derive(password);
    let result = {
        let mut table = TABLE.lock();
        match table.actors.iter_mut().find(|a| a.used && a.name_is(name)) {
            Some(actor) => {
                actor.install_password(&cred, password);
                Ok(())
            }
            // Deleted while we derived.
            None => Err(SysError::NotFound),
        }
    };
    cred.wipe();
    if result.is_ok() {
        mark_dirty();
    }
    result
}

fn password_ok(password: &[u8]) -> bool {
    (1..=64).contains(&password.len())
        && password.iter().all(|b| b.is_ascii_graphic() || *b == b' ')
}

/// Deletes an actor whose tree is only an empty root (and optional empty Desktop).
///
/// Refuses [`ADMIN_NAME`]. Caller must ensure no live task still uses this root.
pub fn remove_user(name: &str) -> Result<(), SysError> {
    if name == ADMIN_NAME {
        return Err(SysError::Unsupported);
    }
    let mut table = TABLE.lock();
    let Some(ai) = table.actors.iter().position(|a| a.used && a.name_is(name)) else {
        return Err(SysError::NotFound);
    };
    let root = table.actors[ai].root;
    let mut desktop: Option<usize> = None;
    for (i, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY || obj.parent != root {
            continue;
        }
        if desktop.is_some() {
            return Err(SysError::Unsupported);
        }
        if obj.kind != KIND_DIR || !obj.name_is("Desktop") || has_child(&table, i as u32) {
            return Err(SysError::Unsupported);
        }
        desktop = Some(i);
    }
    if let Some(di) = desktop {
        unlink_child(&mut table, root, di as u32);
        table.objects[di] = Object::empty();
    }
    table.objects[root as usize] = Object::empty();
    // Drop durable shares naming this actor or its (now-empty) objects.
    for share in &mut table.shares {
        if !share.used {
            continue;
        }
        if share.grantee as usize == ai
            || share.object == root
            || desktop == Some(share.object as usize)
        {
            *share = Share::empty();
        }
    }
    table.actors[ai] = Actor::empty();
    Ok(())
}

/// Records a durable home share for actor `grantee` on `path`.
///
/// Re-applied at login. Caller must already hold `rights` on the object
/// (same confused-deputy rule as live `grant`).
pub fn add_share(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    path: &str,
    rights: u8,
    grantee: &str,
) -> Result<(), SysError> {
    if rights == 0 {
        return Err(SysError::BadValue);
    }
    let object = resolve_and_check(root, tokens, path, rights)?;
    let mut table = TABLE.lock();
    let Some(gi) = table
        .actors
        .iter()
        .position(|a| a.used && a.name_is(grantee))
    else {
        return Err(SysError::NotFound);
    };
    // Merge into an existing share on the same object+grantee when present.
    for share in &mut table.shares {
        if share.used && share.grantee as usize == gi && share.object == object {
            share.rights |= rights;
            drop(table);
            mark_dirty();
            return Ok(());
        }
    }
    let Some(slot) = table.shares.iter().position(|s| !s.used) else {
        return Err(SysError::NoResource);
    };
    table.shares[slot] = Share {
        used: true,
        grantee: gi as u8,
        rights,
        object,
    };
    drop(table);
    mark_dirty();
    Ok(())
}

/// Clears durable share rights for actor `grantee` on `path`.
pub fn remove_share(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    path: &str,
    rights: u8,
    grantee: &str,
) -> Result<(), SysError> {
    if rights == 0 {
        return Err(SysError::BadValue);
    }
    let object = resolve_and_check(root, tokens, path, rights)?;
    let mut table = TABLE.lock();
    let Some(gi) = table
        .actors
        .iter()
        .position(|a| a.used && a.name_is(grantee))
    else {
        return Err(SysError::NotFound);
    };
    let mut found = false;
    for share in &mut table.shares {
        if share.used && share.grantee as usize == gi && share.object == object {
            found = true;
            share.rights &= !rights;
            if share.rights == 0 {
                *share = Share::empty();
            }
        }
    }
    if !found {
        return Err(SysError::NotFound);
    }
    drop(table);
    mark_dirty();
    Ok(())
}

/// Installs durable shares for the actor that owns `root` into `tokens`.
///
/// Best-effort: a full token table skips remaining shares (`Ok` still).
pub fn apply_shares(root: u32, tokens: &mut [Token; TOKEN_SLOTS]) -> Result<(), SysError> {
    let table = TABLE.lock();
    if root as usize >= table.objects.len() || table.objects[root as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    let ai = table.objects[root as usize].actor as usize;
    if ai >= ACTOR_SLOTS || !table.actors[ai].used {
        return Err(SysError::NotFound);
    }
    for share in &table.shares {
        if !share.used || share.grantee as usize != ai {
            continue;
        }
        if share.object as usize >= table.objects.len()
            || table.objects[share.object as usize].kind == KIND_EMPTY
        {
            continue;
        }
        // Ignore NoResource — login still succeeds with ALL on home.
        let _ = push_token(tokens, share.object, share.rights);
    }
    Ok(())
}

/// True when `cred` holds every right on `object` (exact or ancestor token).
pub fn holds_all(root: u32, tokens: &[Token; TOKEN_SLOTS], object: u32) -> bool {
    let table = TABLE.lock();
    let cred = cred_from_tokens(root, tokens);
    token_allows(&table, &cred, object, RIGHT_ALL)
}

/// True when actor `root` still has the default-admin must-change flag.
pub fn actor_must_change(root: u32) -> bool {
    if root == NO_OBJECT {
        return false;
    }
    let table = TABLE.lock();
    table
        .actors
        .iter()
        .any(|a| a.used && a.root == root && a.max_objects & MUST_CHANGE_BIT != 0)
}

pub use galexy_galf::cards::{attenuate_tokens, consume_once};

/// Adds an actor and an empty root. Test and boot only.
pub fn add_actor_named(name: &str, password: &[u8]) -> Result<u32, SysError> {
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    let mut cred = PasswordCred::derive(password);
    let mut table = TABLE.lock();
    let result = add_actor(&mut table, name, password, &cred);
    drop(table);
    cred.wipe();
    result
}

fn admin_quota_of(table: &Table) -> (u32, u64) {
    let slots = table.geo.inode_slots.max(DISKLESS_INODES as u32);
    let bytes = table
        .geo
        .file_max
        .max(super::large::LARGE_POOL_BYTES as u64);
    (slots, bytes)
}

fn admin_quota_limits() -> (u32, u64) {
    admin_quota_of(&TABLE.lock())
}

/// Raises admin's stored quota to [`admin_quota_limits`] when it is lower.
///
/// A sealed disk keeps the quota from the release that formatted it.
/// That cap can be 64 MiB, which is below `libcore`'s metadata, so the
/// sysroot import returns `NoResource` after the smaller libraries have
/// already been copied. A higher quota is left alone. The must-change
/// flag stays set. Nothing is dirtied when the stored limits already fit.
pub fn raise_admin_quota_for_sysroot() {
    let (need_objects, need_bytes) = admin_quota_limits();
    let mut table = TABLE.lock();
    let Some(actor) = table
        .actors
        .iter_mut()
        .find(|a| a.used && a.name_is(ADMIN_NAME))
    else {
        return;
    };
    let objects_low = actor.object_limit() < need_objects;
    let bytes_low = actor.max_bytes < need_bytes;
    if !objects_low && !bytes_low {
        return;
    }
    if objects_low {
        let keep = actor.max_objects & MUST_CHANGE_BIT;
        actor.max_objects = need_objects | keep;
    }
    if bytes_low {
        actor.max_bytes = need_bytes;
    }
    drop(table);
    mark_dirty();
}

fn actor_usage(table: &Table, ai: usize) -> (u32, u64) {
    let actor = &table.actors[ai];
    if actor.used_objects > 0 || actor.used_bytes > 0 {
        return (actor.used_objects, actor.used_bytes);
    }
    let mut objects = 0u32;
    let mut bytes = 0u64;
    for (index, obj) in table.objects.iter().enumerate() {
        if obj.kind == KIND_EMPTY || obj.actor as usize != ai {
            continue;
        }
        objects += 1;
        if obj.kind == KIND_FILE {
            let n = super::large::len(index).map(u64::from).unwrap_or(obj.len);
            bytes = bytes.saturating_add(n);
        }
    }
    (objects, bytes)
}

fn can_add_object(table: &Table, ai: usize) -> bool {
    if ai >= ACTOR_SLOTS || !table.actors[ai].used {
        return false;
    }
    let (used, _) = actor_usage(table, ai);
    used < table.actors[ai].object_limit()
}

fn can_add_bytes(table: &Table, ai: usize, extra: u64) -> bool {
    if ai >= ACTOR_SLOTS || !table.actors[ai].used {
        return false;
    }
    let (_, used) = actor_usage(table, ai);
    used.saturating_add(extra) <= table.actors[ai].max_bytes
}

fn subtree_usage(table: &Table, root: u32) -> (u32, u64) {
    let mut objects = 0u32;
    let mut bytes = 0u64;
    for i in 0..table.objects.len() {
        if table.objects[i].kind == KIND_EMPTY {
            continue;
        }
        if covers_object(table, root, i as u32) {
            objects += 1;
            if table.objects[i].kind == KIND_FILE {
                bytes = bytes.saturating_add(table.objects[i].len);
            }
        }
    }
    (objects, bytes)
}

/// Adds an actor whose credential was derived BEFORE `table` was locked
/// (see [`PasswordCred`]); `password` is only compared against the default
/// for the must-change flag.
fn add_actor(
    table: &mut Table,
    name: &str,
    password: &[u8],
    cred: &PasswordCred,
) -> Result<u32, SysError> {
    if !component_ok(name) || name.len() > ACTOR_NAME {
        return Err(SysError::BadValue);
    }
    if table.actors.iter().any(|a| a.name_is(name)) {
        return Err(SysError::Unsupported);
    }
    let Some(ai) = table.actors.iter().position(|a| !a.used) else {
        return Err(SysError::NoResource);
    };
    let Some(oi) = free_object(table) else {
        return Err(SysError::NoResource);
    };
    let (max_objects, max_bytes) = if name == ADMIN_NAME {
        admin_quota_of(table)
    } else {
        (DEFAULT_MAX_OBJECTS, DEFAULT_MAX_BYTES)
    };
    let actor = &mut table.actors[ai];
    actor.used = true;
    actor.name = [0; ACTOR_NAME];
    actor.name[..name.len()].copy_from_slice(name.as_bytes());
    actor.name_len = name.len() as u8;
    actor.root = oi as u32;
    actor.max_objects = max_objects;
    actor.max_bytes = max_bytes;
    actor.install_password(cred, password);
    let obj = &mut table.objects[oi];
    *obj = Object::empty();
    obj.kind = KIND_DIR;
    obj.parent = NO_PARENT;
    obj.actor = ai as u8;
    // Root has an empty name; paths start at its children.
    Ok(oi as u32)
}

/// Sets durable object/byte limits for actor `name`. Admin-only at the syscall layer.
pub fn set_actor_quota(name: &str, max_objects: u32, max_bytes: u64) -> Result<(), SysError> {
    if max_objects == 0 || max_objects & MUST_CHANGE_BIT != 0 {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let Some(actor) = table.actors.iter_mut().find(|a| a.used && a.name_is(name)) else {
        return Err(SysError::NotFound);
    };
    let keep = actor.max_objects & MUST_CHANGE_BIT;
    actor.max_objects = max_objects | keep;
    actor.max_bytes = max_bytes;
    drop(table);
    mark_dirty();
    Ok(())
}

/// Returns `(objects_used, objects_max, bytes_used, bytes_max)` for `name`.
pub fn actor_quota(name: &str) -> Result<(u32, u32, u32, u32), SysError> {
    let table = TABLE.lock();
    let Some(ai) = table.actors.iter().position(|a| a.used && a.name_is(name)) else {
        return Err(SysError::NotFound);
    };
    let (used_o, used_b) = actor_usage(&table, ai);
    let actor = &table.actors[ai];
    Ok((
        used_o,
        actor.object_limit(),
        used_b.min(u32::MAX as u64) as u32,
        actor.max_bytes.min(u32::MAX as u64) as u32,
    ))
}

/// Quota for the actor that owns `root`, or `NotFound` if unset.
pub fn root_quota(root: u32) -> Result<(u32, u32, u32, u32), SysError> {
    let table = TABLE.lock();
    if root as usize >= table.objects.len() || table.objects[root as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    let ai = table.objects[root as usize].actor as usize;
    if ai >= ACTOR_SLOTS || !table.actors[ai].used {
        return Err(SysError::NotFound);
    }
    let (used_o, used_b) = actor_usage(&table, ai);
    let actor = &table.actors[ai];
    Ok((
        used_o,
        actor.object_limit(),
        used_b.min(u32::MAX as u64) as u32,
        actor.max_bytes.min(u32::MAX as u64) as u32,
    ))
}

/// Packs a quota record into `out` (at least [`galexy_abi::QUOTA_LEN`] bytes).
pub fn format_quota_record(
    used_objects: u32,
    max_objects: u32,
    used_bytes: u32,
    max_bytes: u32,
    out: &mut [u8],
) -> Result<usize, SysError> {
    use galexy_abi::QUOTA_LEN;
    if out.len() < QUOTA_LEN {
        return Err(SysError::BadBuffer);
    }
    out[0..4].copy_from_slice(&used_objects.to_le_bytes());
    out[4..8].copy_from_slice(&max_objects.to_le_bytes());
    out[8..12].copy_from_slice(&used_bytes.to_le_bytes());
    out[12..16].copy_from_slice(&max_bytes.to_le_bytes());
    Ok(QUOTA_LEN)
}

/// Looks up a direct child by name. Test helper.
pub fn find_under(parent: u32, name: &str) -> Option<u32> {
    let table = TABLE.lock();
    find_child(&table, parent, name).map(|i| i as u32)
}

/// Creates a directory under an actor's root. Test helper.
pub fn mkdir_under_root(root: u32, name: &str) -> Result<u32, SysError> {
    if !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let oi = mkdir_locked(&mut table, root, name)?;
    drop(table);
    mark_dirty();
    Ok(oi)
}

fn mkdir_locked(table: &mut Table, root: u32, name: &str) -> Result<u32, SysError> {
    if root as usize >= table.objects.len() || table.objects[root as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    if find_child(table, root, name).is_some() {
        return Err(SysError::Unsupported);
    }
    let actor = table.objects[root as usize].actor as usize;
    if !can_add_object(table, actor) {
        return Err(SysError::NoResource);
    }
    let Some(oi) = free_object(table) else {
        return Err(SysError::NoResource);
    };
    let obj = &mut table.objects[oi];
    obj.kind = KIND_DIR;
    obj.parent = root;
    obj.actor = actor as u8;
    place_name(obj, name);
    link_child(table, root, oi as u32);
    Ok(oi as u32)
}

/// Creates an empty file under `parent`. Test helper.
pub fn create_file_under(parent: u32, name: &str) -> Result<u32, SysError> {
    if !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    if parent as usize >= table.objects.len() || table.objects[parent as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    if find_child(&table, parent, name).is_some() {
        return Err(SysError::Unsupported);
    }
    let actor = table.objects[parent as usize].actor as usize;
    if !can_add_object(&table, actor) {
        return Err(SysError::NoResource);
    }
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let obj = &mut table.objects[oi];
    obj.kind = KIND_FILE;
    obj.parent = parent;
    obj.actor = actor as u8;
    place_name(obj, name);
    obj.len = 0;
    link_child(&mut table, parent, oi as u32);
    drop(table);
    mark_dirty();
    Ok(oi as u32)
}

/// Creates `name` under `parent` with a copy of `bytes` in the RAM store.
///
/// Files that fit in 32 KiB normally stay in the v12 pool. A real write
/// that outgrows that pool promotes through [`super::large::create_from`].
/// The compiler sysroot uses [`import_shared`] instead, so its ramdisk
/// bytes are not copied into 4 KiB boxes.
pub fn import_promoted(parent: u32, name: &str, bytes: &[u8]) -> Result<u32, SysError> {
    if bytes.len() > super::large::LARGE_FILE_MAX as usize {
        return Err(SysError::BadValue);
    }
    let id = create_file_under(parent, name)?;
    {
        let table = TABLE.lock();
        let actor = table.objects[id as usize].actor as usize;
        if !can_add_bytes(&table, actor, bytes.len() as u64) {
            return Err(SysError::NoResource);
        }
    }
    if !super::large::create_from(id as usize, bytes) {
        return Err(SysError::NoResource);
    }
    mark_dirty();
    Ok(id)
}

/// Creates `name` under `parent` with `bytes` attached, not copied.
///
/// `bytes` must outlive the volume (a ramdisk slice). Quota still
/// counts the length. A later write copies the visible prefix into
/// pool blocks. The compiler sysroot uses this so `libcore`'s metadata
/// does not allocate one 4 KiB heap box per block.
pub fn import_shared(parent: u32, name: &str, bytes: &'static [u8]) -> Result<u32, SysError> {
    if bytes.len() > super::large::LARGE_FILE_MAX as usize {
        return Err(SysError::BadValue);
    }
    let id = create_file_under(parent, name)?;
    {
        let table = TABLE.lock();
        let actor = table.objects[id as usize].actor as usize;
        if !can_add_bytes(&table, actor, bytes.len() as u64) {
            return Err(SysError::NoResource);
        }
    }
    if !super::large::create_shared(id as usize, bytes) {
        return Err(SysError::NoResource);
    }
    mark_dirty();
    Ok(id)
}

fn free_object(table: &Table) -> Option<usize> {
    table.objects.iter().position(|o| o.kind == KIND_EMPTY)
}

/// Single-component rule; the grammar lives in `galexy_core::path`.
fn component_ok(name: &str) -> bool {
    galexy_core::component_ok(name)
}

fn place_name(obj: &mut Object, name: &str) {
    obj.name = [0; NAME_CAP];
    obj.name[..name.len()].copy_from_slice(name.as_bytes());
    obj.name_len = name.len() as u8;
}

/// A path after the optional leading `/` and `owner@` on the first component.
pub(crate) type ParsedPath<'a> = galexy_core::ParsedPath<'a>;

/// Splits `name` per the galfs path grammar (`galexy_core::parse_path`);
/// anything outside the grammar is `BadValue`.
pub(crate) fn parse_path(name: &str) -> Result<ParsedPath<'_>, SysError> {
    galexy_core::parse_path(name).ok_or(SysError::BadValue)
}

fn find_actor_root(table: &Table, name: &str) -> Result<u32, SysError> {
    table
        .actors
        .iter()
        .find(|a| a.name_is(name))
        .map(|a| a.root)
        .ok_or(SysError::NotFound)
}

fn find_child(table: &Table, parent: u32, name: &str) -> Option<usize> {
    let mut child = table.objects.get(parent as usize)?.child;
    for _ in 0..table.objects.len() {
        if child == NO_OBJECT {
            return None;
        }
        let index = child as usize;
        let obj = table.objects.get(index)?;
        if obj.name_is(name) {
            return Some(index);
        }
        child = obj.next;
    }
    None
}

fn has_child(table: &Table, parent: u32) -> bool {
    table
        .objects
        .get(parent as usize)
        .is_some_and(|obj| obj.kind == KIND_DIR && obj.child != NO_OBJECT)
}

/// Inserts `child` at the head of `parent`'s directory list.
fn link_child(table: &mut Table, parent: u32, child: u32) {
    let Some(dir) = table.objects.get_mut(parent as usize) else {
        return;
    };
    let head = dir.child;
    dir.child = child;
    if let Some(obj) = table.objects.get_mut(child as usize) {
        obj.next = head;
    }
}

/// Removes `child` from `parent`'s directory list. The object itself stays.
fn unlink_child(table: &mut Table, parent: u32, child: u32) {
    let Some(dir) = table.objects.get(parent as usize) else {
        return;
    };
    let mut prev = NO_OBJECT;
    let mut cur = dir.child;
    for _ in 0..table.objects.len() {
        if cur == NO_OBJECT {
            return;
        }
        let next = table
            .objects
            .get(cur as usize)
            .map(|obj| obj.next)
            .unwrap_or(NO_OBJECT);
        if cur == child {
            if prev == NO_OBJECT {
                table.objects[parent as usize].child = next;
            } else if let Some(obj) = table.objects.get_mut(prev as usize) {
                obj.next = next;
            }
            if let Some(obj) = table.objects.get_mut(child as usize) {
                obj.next = NO_OBJECT;
            }
            return;
        }
        prev = cur;
        cur = next;
    }
}

/// Walks every component except the last under `start`.
fn walk_parents(table: &Table, start: u32, parsed: &ParsedPath<'_>) -> Result<u32, SysError> {
    let mut parent = start;
    for comp in &parsed.comps[..parsed.n.saturating_sub(1)] {
        let Some(index) = find_child(table, parent, comp) else {
            return Err(SysError::NotFound);
        };
        if table.objects[index].kind != KIND_DIR {
            return Err(SysError::NotFound);
        }
        parent = index as u32;
    }
    Ok(parent)
}

fn start_root(table: &Table, cred: &FsCred, parsed: &ParsedPath<'_>) -> Result<u32, SysError> {
    match parsed.owner {
        Some(owner) => find_actor_root(table, owner),
        None => {
            if cred.root == NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            Ok(cred.root)
        }
    }
}

/// Parent lookup for the card algebra: `None` past the table or at a root.
fn parent_link(table: &Table) -> impl Fn(u32) -> Option<u32> + '_ {
    move |cur| {
        let parent = table.objects.get(cur as usize)?.parent;
        (parent != NO_PARENT).then_some(parent)
    }
}

fn covers_object(table: &Table, ancestor: u32, object: u32) -> bool {
    galexy_galf::cards::covers(parent_link(table), ancestor, object, OBJECT_SLOTS)
}

/// Whether `cred` holds `need` on `object` or an ancestor.
///
/// Admin is not a blanket bypass. `useradd` / `userdel` / `passwd` of
/// another account stay admin-only via [`is_admin_root`]. Foreign trees
/// need an explicit card (or `su`, which installs `ALL` on that root).
fn token_allows(table: &Table, cred: &FsCred, object: u32, need: u8) -> bool {
    galexy_galf::cards::allows(parent_link(table), &cred.tokens, object, need, OBJECT_SLOTS)
}

fn cred_from_tokens(root: u32, tokens: &[Token; TOKEN_SLOTS]) -> FsCred {
    FsCred {
        root,
        tokens: *tokens,
    }
}

/// Resolves `parsed` to an object index, checking existence only.
fn lookup(table: &Table, cred: &FsCred, parsed: &ParsedPath<'_>) -> Result<(u32, usize), SysError> {
    let start = start_root(table, cred, parsed)?;
    if parsed.n == 0 {
        // Actor root path (`eve@/`).
        return Ok((NO_PARENT, start as usize));
    }
    let parent = walk_parents(table, start, parsed)?;
    let last = parsed.comps[parsed.n - 1];
    let Some(found) = find_child(table, parent, last) else {
        return Err(SysError::NotFound);
    };
    Ok((parent, found))
}

/// Looks up a file for open. Directory → `Unsupported` after the token check.
pub(crate) fn open_file(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u32, SysError> {
    let parsed = parse_path(name)?;
    if parsed.dir {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u32;
    if !token_allows(&table, &cred, index, RIGHT_READ) {
        return Err(SysError::AccessDenied);
    }
    if table.objects[found].kind != KIND_FILE {
        return Err(SysError::Unsupported);
    }
    Ok(index)
}

/// Creates a file or directory. `replace` empties an existing file.
pub(crate) fn create(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    replace: bool,
) -> Result<Option<u32>, SysError> {
    let parsed = parse_path(name)?;
    if parsed.n == 0 {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let mut table = TABLE.lock();
    let start = start_root(&table, &cred, &parsed)?;
    let parent = walk_parents(&table, start, &parsed)?;
    if !token_allows(&table, &cred, parent, RIGHT_CREATE) {
        return Err(SysError::AccessDenied);
    }
    let last = parsed.comps[parsed.n - 1];
    if let Some(found) = find_child(&table, parent, last) {
        if replace && !parsed.dir && table.objects[found].kind == KIND_FILE {
            let index = found as u32;
            if !token_allows(&table, &cred, index, RIGHT_WRITE) {
                return Err(SysError::AccessDenied);
            }
            free_file_blocks(&mut table, found);
            return Ok(Some(index));
        }
        return Err(SysError::Unsupported);
    }
    let actor = table.objects[parent as usize].actor as usize;
    if !can_add_object(&table, actor) {
        return Err(SysError::NoResource);
    }
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let obj = &mut table.objects[oi];
    obj.kind = if parsed.dir { KIND_DIR } else { KIND_FILE };
    obj.parent = parent;
    obj.actor = actor as u8;
    place_name(obj, last);
    obj.len = 0;
    link_child(&mut table, parent, oi as u32);
    if parsed.dir {
        Ok(None)
    } else {
        Ok(Some(oi as u32))
    }
}

/// Removes a file or empty directory.
pub(crate) fn remove(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u32, SysError> {
    let parsed = parse_path(name)?;
    if parsed.n == 0 {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let mut table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u32;
    if !token_allows(&table, &cred, index, RIGHT_REMOVE) {
        return Err(SysError::AccessDenied);
    }
    let kind = table.objects[found].kind;
    if parsed.dir && kind != KIND_DIR {
        return Err(SysError::Unsupported);
    }
    if kind == KIND_DIR && has_child(&table, index) {
        return Err(SysError::Unsupported);
    }
    // Never remove an actor root.
    if table.objects[found].parent == NO_PARENT {
        return Err(SysError::Unsupported);
    }
    if kind == KIND_FILE {
        free_file_blocks(&mut table, found);
    }
    let parent = table.objects[found].parent;
    if parent != NO_PARENT {
        unlink_child(&mut table, parent, index);
    }
    table.objects[found] = Object::empty();
    Ok(index)
}

/// File length under the global lock.
pub(crate) fn with_file<R>(index: u32, f: impl FnOnce(&Object) -> R) -> Option<R> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(f(&table.objects[i]))
}

fn alloc_block() -> Option<u32> {
    let mut pool = POOL.lock();
    let n = pool.blocks as usize;
    if n == 0 || pool.bitmap.is_empty() {
        return None;
    }
    let start = (pool.cursor as usize).min(n);
    for pass in 0..2 {
        let range = if pass == 0 { start..n } else { 0..start };
        for i in range {
            let mask = 1u8 << (i % 8);
            if pool.bitmap[i / 8] & mask != 0 || pool.hold[i] > 0 {
                continue;
            }
            pool.bitmap[i / 8] |= mask;
            pool.gen[i] = pool.clock.max(1);
            pool.cursor = (i as u32).saturating_add(1);
            pool.stable[i] = false;
            pool.dirty[i] = true;
            pool.sealed[i] = false;
            pool.hold[i] = 0;
            let mut cache = CACHE.lock();
            let _ = cache_get(&mut cache, &mut pool, i as u32);
            return Some(i as u32);
        }
    }
    None
}

fn free_block(block: u32) {
    let i = block as usize;
    let mut pool = POOL.lock();
    if i >= pool.blocks as usize {
        return;
    }
    let was_stable = pool.stable[i];
    pool.bitmap[i / 8] &= !(1u8 << (i % 8));
    pool.dirty[i] = false;
    pool.sealed[i] = false;
    pool.stable[i] = false;
    if was_stable {
        pool.hold[i] = 2;
    } else {
        pool.hold[i] = 0;
        pool.gen[i] = 0;
    }
    drop(pool);
    let mut cache = CACHE.lock();
    for slot in &mut cache.index {
        if *slot == block {
            *slot = u32::MAX;
        }
    }
}

fn phys_at(obj: &Object, logical: u32) -> Option<u32> {
    let mut at = 0u32;
    for ext in obj.extents.iter().take(obj.extent_count as usize) {
        let end = at.saturating_add(ext.count);
        if logical < end {
            return Some(ext.start.saturating_add(logical - at));
        }
        at = end;
    }
    None
}

fn append_extent(obj: &mut Object, phys: u32) -> bool {
    if obj.extent_count > 0 {
        let last = &mut obj.extents[obj.extent_count as usize - 1];
        let gen = POOL.lock().gen.get(phys as usize).copied().unwrap_or(1);
        if last.start.saturating_add(last.count) == phys && last.gen == gen {
            last.count = last.count.saturating_add(1);
            return true;
        }
    }
    if obj.extent_count as usize >= EXTENTS_PER_FILE {
        return false;
    }
    let n = obj.extent_count as usize;
    let gen = POOL.lock().gen.get(phys as usize).copied().unwrap_or(1);
    obj.extents[n] = galexy_galf::Extent {
        start: phys,
        count: 1,
        gen,
    };
    obj.extent_count += 1;
    true
}

fn replace_logical(obj: &mut Object, logical: u32, phys: u32) -> bool {
    let mut at = 0u32;
    let mut found = None;
    for (i, ext) in obj
        .extents
        .iter()
        .take(obj.extent_count as usize)
        .enumerate()
    {
        let end = at.saturating_add(ext.count);
        if logical < end {
            found = Some((i, logical - at, *ext));
            break;
        }
        at = end;
    }
    let Some((i, off, ext)) = found else {
        return false;
    };
    let fresh_gen = POOL.lock().gen.get(phys as usize).copied().unwrap_or(1);
    let mut parts = [galexy_galf::Extent::empty(); 3];
    let mut n = 0usize;
    if off > 0 {
        parts[n] = galexy_galf::Extent {
            start: ext.start,
            count: off,
            gen: ext.gen,
        };
        n += 1;
    }
    parts[n] = galexy_galf::Extent {
        start: phys,
        count: 1,
        gen: fresh_gen,
    };
    n += 1;
    if off + 1 < ext.count {
        parts[n] = galexy_galf::Extent {
            start: ext.start.saturating_add(off + 1),
            count: ext.count - off - 1,
            gen: ext.gen,
        };
        n += 1;
    }
    let old_n = obj.extent_count as usize;
    if old_n - 1 + n > EXTENTS_PER_FILE {
        return false;
    }
    let mut next = [galexy_galf::Extent::empty(); EXTENTS_PER_FILE];
    next[..i].copy_from_slice(&obj.extents[..i]);
    next[i..i + n].copy_from_slice(&parts[..n]);
    let tail = old_n - (i + 1);
    if tail > 0 {
        next[i + n..i + n + tail].copy_from_slice(&obj.extents[i + 1..i + 1 + tail]);
    }
    obj.extents = next;
    obj.extent_count = (old_n - 1 + n) as u8;
    true
}

fn copy_block(from: u32, to: u32) -> bool {
    let mut pool = POOL.lock();
    let mut cache = CACHE.lock();
    let Some(src) = cache_get(&mut cache, &mut pool, from) else {
        return false;
    };
    let mut tmp = [0u8; BLOCK_SIZE];
    tmp.copy_from_slice(&cache.data[src]);
    let Some(dst) = cache_get(&mut cache, &mut pool, to) else {
        return false;
    };
    cache.data[dst] = tmp;
    pool.dirty[to as usize] = true;
    pool.sealed[to as usize] = false;
    true
}

fn make_mutable(obj: &mut Object, logical: usize) -> Option<u32> {
    let phys = phys_at(obj, logical as u32)?;
    let stable = POOL.lock().stable[phys as usize];
    if !stable {
        // Bring the block into the cache before clearing `sealed`. A miss
        // on a dirty unsealed line is a fresh zero page; an evicted block
        // that was already stored must be reloaded first.
        let mut pool = POOL.lock();
        let mut cache = CACHE.lock();
        cache_get(&mut cache, &mut pool, phys)?;
        pool.dirty[phys as usize] = true;
        pool.sealed[phys as usize] = false;
        return Some(phys);
    }
    let fresh = alloc_block()?;
    if !copy_block(phys, fresh) || !replace_logical(obj, logical as u32, fresh) {
        free_block(fresh);
        return None;
    }
    free_block(phys);
    Some(fresh)
}

fn ensure_data_block(obj: &mut Object, slot: usize) -> Option<u32> {
    if phys_at(obj, slot as u32).is_some() {
        return make_mutable(obj, slot);
    }
    let b = alloc_block()?;
    if !append_extent(obj, b) {
        free_block(b);
        return None;
    }
    Some(b)
}

fn free_file_blocks(table: &mut Table, index: usize) {
    super::large::clear(index);
    free_extent_blocks(&mut table.objects[index]);
}

fn free_extent_blocks(obj: &mut Object) {
    for ext in obj.extents.iter().take(obj.extent_count as usize) {
        let mut b = ext.start;
        for _ in 0..ext.count {
            free_block(b);
            b = b.saturating_add(1);
        }
    }
    obj.extent_count = 0;
    obj.extents = [galexy_galf::Extent::empty(); EXTENTS_PER_FILE];
    obj.len = 0;
}

fn write_block_bytes(phys: u32, off: usize, src: &[u8]) -> bool {
    let mut pool = POOL.lock();
    let mut cache = CACHE.lock();
    let Some(slot) = cache_get(&mut cache, &mut pool, phys) else {
        return false;
    };
    cache.data[slot][off..off + src.len()].copy_from_slice(src);
    pool.dirty[phys as usize] = true;
    pool.sealed[phys as usize] = false;
    true
}

fn read_block_bytes(phys: u32, off: usize, dst: &mut [u8]) -> bool {
    let mut pool = POOL.lock();
    let mut cache = CACHE.lock();
    let Some(slot) = cache_get(&mut cache, &mut pool, phys) else {
        return false;
    };
    dst.copy_from_slice(&cache.data[slot][off..off + dst.len()]);
    true
}

fn zero_block_tail(phys: u32, off: usize) -> bool {
    let mut pool = POOL.lock();
    let mut cache = CACHE.lock();
    let Some(slot) = cache_get(&mut cache, &mut pool, phys) else {
        return false;
    };
    cache.data[slot][off..].fill(0);
    pool.dirty[phys as usize] = true;
    pool.sealed[phys as usize] = false;
    true
}

fn file_len(table: &Table, index: usize) -> usize {
    super::large::len(index)
        .map(|n| n as usize)
        .unwrap_or(table.objects[index].len as usize)
}

fn headroom(table: &Table, index: usize) -> u32 {
    let ai = table.objects[index].actor as usize;
    let (_, used) = actor_usage(table, ai);
    let room = table.actors[ai].max_bytes.saturating_sub(used);
    room.min(u32::MAX as u64) as u32
}

fn copy_small(obj: &Object, buf: &mut [u8]) -> bool {
    let mut copied = 0usize;
    while copied < buf.len() {
        let slot = copied / BLOCK_SIZE;
        let off = copied % BLOCK_SIZE;
        let Some(b) = phys_at(obj, slot as u32) else {
            return false;
        };
        let n = (BLOCK_SIZE - off).min(buf.len() - copied);
        if !read_block_bytes(b, off, &mut buf[copied..copied + n]) {
            return false;
        }
        copied += n;
    }
    true
}

/// Moves a durable file into the RAM store. Extents are freed only after
/// the copy lands.
fn promote_large(table: &mut Table, index: usize) -> bool {
    let pos = table.objects[index].len as usize;
    let mut buf = alloc::vec![0u8; pos];
    if pos > 0 && !copy_small(&table.objects[index], &mut buf) {
        return false;
    }
    if !super::large::create_from(index, &buf) {
        return false;
    }
    free_extent_blocks(&mut table.objects[index]);
    true
}

/// Appends `src` to a file. Returns the byte count written (short on full
/// file, exhausted block pool, or actor byte quota).
pub(crate) fn append(index: u32, src: &[u8]) -> Option<usize> {
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(append_locked(&mut table, i, src))
}

/// Positional write: bytes inside the current length are overwritten in
/// place (no quota charge — the bytes are already accounted for), bytes at
/// or past the end are appended under the usual file/pool/quota limits. A
/// `start` beyond the end is zero-filled first, so a seek past EOF followed
/// by a write behaves like POSIX. Returns the byte count written.
pub(crate) fn write_at(index: u32, start: usize, src: &[u8]) -> Option<usize> {
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return None;
    }
    let len = file_len(&table, i);
    if super::large::len(i).is_some()
        || (start as u64).saturating_add(src.len() as u64) > table.geo.file_max
    {
        if super::large::len(i).is_none() && !promote_large(&mut table, i) {
            return Some(0);
        }
        return Some(super::large::write_at(i, start, src, headroom(&table, i)));
    }
    let mut done = 0usize;
    if start < len {
        let overwrite = src.len().min(len - start);
        while done < overwrite {
            let pos = start + done;
            let slot = pos / BLOCK_SIZE;
            let off = pos % BLOCK_SIZE;
            let Some(b) = make_mutable(&mut table.objects[i], slot) else {
                return Some(done);
            };
            let n = (BLOCK_SIZE - off).min(overwrite - done);
            if !write_block_bytes(b, off, &src[done..done + n]) {
                return Some(done);
            }
            done += n;
        }
    } else if start > len {
        let zeros = [0u8; BLOCK_SIZE];
        let mut gap = start - len;
        while gap > 0 {
            let n = append_locked(&mut table, i, &zeros[..gap.min(BLOCK_SIZE)]);
            if n == 0 {
                return Some(0);
            }
            gap -= n;
        }
    }
    if done < src.len() {
        done += append_locked(&mut table, i, &src[done..]);
    }
    Some(done)
}

/// Append loop shared by `append` and `write_at`; `i` must be a file slot.
fn append_locked(table: &mut Table, i: usize, src: &[u8]) -> usize {
    if super::large::len(i).is_some()
        || table.objects[i].len.saturating_add(src.len() as u64) > table.geo.file_max
    {
        if super::large::len(i).is_none() && !promote_large(table, i) {
            return 0;
        }
        return super::large::append(i, src, headroom(table, i));
    }
    let ai = table.objects[i].actor as usize;
    let mut written = 0usize;
    while written < src.len() {
        let pos = table.objects[i].len as usize;
        if pos as u64 >= table.geo.file_max {
            break;
        }
        if !can_add_bytes(table, ai, 1) {
            break;
        }
        let slot = pos / BLOCK_SIZE;
        let off = pos % BLOCK_SIZE;
        let Some(bi) = ensure_data_block(&mut table.objects[i], slot) else {
            break;
        };
        let mut room = (BLOCK_SIZE - off)
            .min(src.len() - written)
            .min((table.geo.file_max as usize).saturating_sub(pos));
        let (_, used) = actor_usage(table, ai);
        let headroom = table.actors[ai].max_bytes.saturating_sub(used) as usize;
        if headroom == 0 {
            break;
        }
        room = room.min(headroom);
        if !write_block_bytes(bi, off, &src[written..written + room]) {
            break;
        }
        table.objects[i].len = (pos + room) as u64;
        written += room;
    }
    written
}

/// Copies `dst.len()` bytes from file offset `start`.
pub(crate) fn read_at(index: u32, start: usize, dst: &mut [u8]) -> Option<usize> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return None;
    }
    if super::large::len(i).is_some() {
        return super::large::read(i, start, dst);
    }
    let len = table.objects[i].len as usize;
    if start >= len || dst.is_empty() {
        return Some(0);
    }
    let mut copied = 0usize;
    let want = dst.len().min(len - start);
    while copied < want {
        let pos = start + copied;
        let slot = pos / BLOCK_SIZE;
        let off = pos % BLOCK_SIZE;
        let b = phys_at(&table.objects[i], slot as u32)?;
        let n = (BLOCK_SIZE - off).min(want - copied);
        if !read_block_bytes(b, off, &mut dst[copied..copied + n]) {
            return None;
        }
        copied += n;
    }
    Some(copied)
}

/// Appends bytes to a file by object index. Test helper.
pub fn append_file(index: u32, src: &[u8]) -> Option<usize> {
    let n = append(index, src)?;
    if n > 0 {
        mark_dirty();
    }
    Some(n)
}

/// Removes a path as admin. Test helper.
pub fn remove_as_admin(name: &str) -> Result<(), SysError> {
    let cred = admin_cred();
    remove(cred.root, &cred.tokens, name)?;
    mark_dirty();
    Ok(())
}

/// Copies file bytes into `out`. Returns the length, or `None` if missing.
pub fn read_file_bytes(index: u32, out: &mut [u8]) -> Option<usize> {
    read_at(index, 0, out)
}

/// Byte length of a file object, including a promoted large file.
pub fn object_len(index: u32) -> Option<usize> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(file_len(&table, i))
}

/// How many blocks are currently allocated in the pool (test helper).
pub fn blocks_used() -> usize {
    let pool = POOL.lock();
    pool.bitmap.iter().map(|b| b.count_ones() as usize).sum()
}

/// Moves a dirent as admin. Test helper.
pub fn rename_as_admin(old: &str, new: &str) -> Result<(), SysError> {
    let cred = admin_cred();
    rename(cred.root, &cred.tokens, old, new)?;
    mark_dirty();
    Ok(())
}

/// Sets a file's length. Test helper (object index).
pub fn truncate_file(index: u32, new_len: usize) -> Result<(), SysError> {
    truncate(index, new_len)?;
    mark_dirty();
    Ok(())
}

/// Stats a path as admin into `out`. Test helper.
pub fn stat_as_admin(name: &str, out: &mut [u8]) -> Result<usize, SysError> {
    let cred = admin_cred();
    stat(cred.root, &cred.tokens, name, out)
}

/// Moves a dirent from `old` to `new` without copying file bytes.
pub(crate) fn rename(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    old: &str,
    new: &str,
) -> Result<(), SysError> {
    let old_parsed = parse_path(old)?;
    let new_parsed = parse_path(new)?;
    if old_parsed.n == 0 || new_parsed.n == 0 {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let mut table = TABLE.lock();
    let (old_parent, found) = lookup(&table, &cred, &old_parsed)?;
    let src = found as u32;
    if !token_allows(&table, &cred, src, RIGHT_REMOVE) {
        return Err(SysError::AccessDenied);
    }
    if table.objects[found].parent == NO_PARENT {
        return Err(SysError::Unsupported);
    }
    let kind = table.objects[found].kind;
    // Trailing slash is allowed only for directories; bare names work for both.
    if (old_parsed.dir || new_parsed.dir) && kind != KIND_DIR {
        return Err(SysError::BadValue);
    }

    let new_start = start_root(&table, &cred, &new_parsed)?;
    let new_parent = walk_parents(&table, new_start, &new_parsed)?;
    if !token_allows(&table, &cred, new_parent, RIGHT_CREATE) {
        return Err(SysError::AccessDenied);
    }
    let new_last = new_parsed.comps[new_parsed.n - 1];
    if find_child(&table, new_parent, new_last).is_some() {
        // Same path rename is a no-op.
        if new_parent == old_parent && table.objects[found].name_is(new_last) {
            return Ok(());
        }
        return Err(SysError::Unsupported);
    }
    if kind == KIND_DIR && (new_parent == src || is_descendant(&table, src, new_parent)) {
        return Err(SysError::BadValue);
    }

    let new_actor = table.objects[new_parent as usize].actor;
    if table.objects[found].actor != new_actor {
        let (need_o, need_b) = subtree_usage(&table, src);
        let dest = new_actor as usize;
        let (have_o, have_b) = actor_usage(&table, dest);
        if have_o.saturating_add(need_o) > table.actors[dest].object_limit()
            || have_b.saturating_add(need_b) > table.actors[dest].max_bytes
        {
            return Err(SysError::NoResource);
        }
    }
    if new_parent != old_parent {
        unlink_child(&mut table, old_parent, src);
    }
    place_name(&mut table.objects[found], new_last);
    table.objects[found].parent = new_parent;
    if new_parent != old_parent {
        link_child(&mut table, new_parent, src);
    }
    if table.objects[found].actor != new_actor {
        reassign_actor_subtree(&mut table, src, new_actor);
    }
    Ok(())
}

fn is_descendant(table: &Table, ancestor: u32, node: u32) -> bool {
    let mut cur = node;
    for _ in 0..table.objects.len() {
        if cur == ancestor {
            return true;
        }
        let p = table.objects[cur as usize].parent;
        if p == NO_PARENT {
            return false;
        }
        cur = p;
    }
    false
}

fn reassign_actor_subtree(table: &mut Table, root: u32, actor: u8) {
    for i in 0..table.objects.len() {
        if table.objects[i].kind == KIND_EMPTY {
            continue;
        }
        if covers_object(table, root, i as u32) {
            table.objects[i].actor = actor;
        }
    }
}

fn truncate_large(index: u32, new_len: usize) -> Result<(), SysError> {
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return Err(SysError::BadCap);
    }
    if super::large::len(i).is_none() && !promote_large(&mut table, i) {
        return Err(SysError::NoResource);
    }
    let old = super::large::len(i).unwrap_or(0) as usize;
    if new_len == old {
        return Ok(());
    }
    if new_len < old {
        super::large::truncate_to(i, new_len as u32);
        return Ok(());
    }
    let mut budget = headroom(&table, i);
    let mut left = new_len - old;
    let chunk = super::large::LARGE_BLOCK.min(left.max(1));
    let zeros = alloc::vec![0u8; chunk];
    while left > 0 {
        let n = left.min(zeros.len());
        let w = super::large::append(i, &zeros[..n], budget);
        if w == 0 {
            return Err(SysError::NoResource);
        }
        budget = budget.saturating_sub(w as u32);
        left -= w;
    }
    Ok(())
}

/// Sets a file's length, allocating or freeing data blocks as needed.
pub(crate) fn truncate(index: u32, new_len: usize) -> Result<(), SysError> {
    if new_len > super::large::LARGE_FILE_MAX as usize {
        return Err(SysError::BadValue);
    }
    if (new_len as u64) > TABLE.lock().geo.file_max || super::large::len(index as usize).is_some() {
        return truncate_large(index, new_len);
    }
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= table.objects.len() || table.objects[i].kind != KIND_FILE {
        return Err(SysError::BadCap);
    }
    let old = table.objects[i].len as usize;
    if new_len == old {
        return Ok(());
    }
    if new_len < old {
        let keep = if new_len == 0 {
            0
        } else {
            new_len.div_ceil(BLOCK_SIZE) as u32
        };
        shrink_extents(&mut table.objects[i], keep);
        if new_len > 0 {
            let off = new_len % BLOCK_SIZE;
            if off != 0 {
                let slot = (new_len - 1) / BLOCK_SIZE;
                let Some(b) = make_mutable(&mut table.objects[i], slot) else {
                    return Err(SysError::NoResource);
                };
                if !zero_block_tail(b, off) {
                    return Err(SysError::NoResource);
                }
            }
        }
        table.objects[i].len = new_len as u64;
        return Ok(());
    }

    let ai = table.objects[i].actor as usize;
    let grow = (new_len - old) as u64;
    if !can_add_bytes(&table, ai, grow) {
        return Err(SysError::NoResource);
    }
    let saved = table.objects[i].extents;
    let saved_count = table.objects[i].extent_count;
    let mut pos = old;
    while pos < new_len {
        let slot = pos / BLOCK_SIZE;
        let Some(bi) = ensure_data_block(&mut table.objects[i], slot) else {
            free_extents_after(&table.objects[i], saved_count, &saved);
            table.objects[i].extents = saved;
            table.objects[i].extent_count = saved_count;
            table.objects[i].len = old as u64;
            return Err(SysError::NoResource);
        };
        let off = pos % BLOCK_SIZE;
        let n = (BLOCK_SIZE - off).min(new_len - pos);
        if pos >= old {
            let zeros = [0u8; 256];
            let mut left = n;
            let mut at = off;
            while left > 0 {
                let chunk = left.min(zeros.len());
                if !write_block_bytes(bi, at, &zeros[..chunk]) {
                    return Err(SysError::NoResource);
                }
                at += chunk;
                left -= chunk;
            }
        }
        pos += n;
    }
    table.objects[i].len = new_len as u64;
    Ok(())
}

/// Drops extent blocks past `keep` logical blocks.
fn shrink_extents(obj: &mut Object, keep: u32) {
    let mut at = 0u32;
    let mut next = [galexy_galf::Extent::empty(); EXTENTS_PER_FILE];
    let mut n = 0u8;
    for ext in obj.extents.iter().take(obj.extent_count as usize) {
        if at >= keep {
            let mut b = ext.start;
            for _ in 0..ext.count {
                free_block(b);
                b = b.saturating_add(1);
            }
            continue;
        }
        if at + ext.count <= keep {
            next[n as usize] = *ext;
            n += 1;
            at += ext.count;
            continue;
        }
        let stay = keep - at;
        next[n as usize] = galexy_galf::Extent {
            start: ext.start,
            count: stay,
            gen: ext.gen,
        };
        n += 1;
        let mut b = ext.start + stay;
        for _ in stay..ext.count {
            free_block(b);
            b = b.saturating_add(1);
        }
        at = keep;
    }
    obj.extents = next;
    obj.extent_count = n;
}

/// Frees blocks present in `obj` that `saved` did not name.
fn free_extents_after(
    obj: &Object,
    saved_count: u8,
    saved: &[galexy_galf::Extent; EXTENTS_PER_FILE],
) {
    for ext in obj.extents.iter().take(obj.extent_count as usize) {
        let mut b = ext.start;
        for _ in 0..ext.count {
            let kept = saved
                .iter()
                .take(saved_count as usize)
                .any(|old| b >= old.start && b < old.start.saturating_add(old.count));
            if !kept {
                free_block(b);
            }
            b = b.saturating_add(1);
        }
    }
}

/// Fills `out` (must be [`galexy_abi::STAT_LEN`]) with path metadata.
pub(crate) fn stat(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    out: &mut [u8],
) -> Result<usize, SysError> {
    use galexy_abi::{STAT_DIR, STAT_FILE, STAT_LEN};
    if out.len() < STAT_LEN {
        return Err(SysError::BadBuffer);
    }
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u32;
    if !token_allows(&table, &cred, index, RIGHT_LIST) {
        return Err(SysError::AccessDenied);
    }
    let obj = &table.objects[found];
    let kind = match obj.kind {
        KIND_FILE => STAT_FILE,
        KIND_DIR => STAT_DIR,
        _ => return Err(SysError::NotFound),
    };
    let rights = held_rights(&table, &cred, index);
    let size = if obj.kind == KIND_FILE {
        super::large::len(found).map(u64::from).unwrap_or(obj.len)
    } else {
        0
    };
    let actor = &table.actors[obj.actor as usize];
    let owner_len = actor.name_len.min(32);
    out[..STAT_LEN].fill(0);
    out[0] = kind;
    out[1] = rights;
    out[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    out[44..48].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    out[8] = owner_len;
    out[9..9 + owner_len as usize].copy_from_slice(&actor.name[..owner_len as usize]);
    Ok(STAT_LEN)
}

fn held_rights(table: &Table, cred: &FsCred, object: u32) -> u8 {
    let mut rights = 0u8;
    for token in &cred.tokens {
        if token.is_live() && covers_object(table, token.object, object) {
            rights |= token.rights & RIGHT_ALL;
        }
    }
    rights
}

fn path_bytes(table: &Table, index: usize, viewer_root: u32, out: &mut [u8]) -> Option<usize> {
    let mut chain = [0usize; MAX_DEPTH];
    let mut depth = 0usize;
    let mut cur = index;
    loop {
        if depth >= MAX_DEPTH {
            return None;
        }
        let obj = &table.objects[cur];
        if obj.kind == KIND_EMPTY {
            return None;
        }
        if obj.parent == NO_PARENT {
            // Stop before the root; children form the path.
            break;
        }
        chain[depth] = cur;
        depth += 1;
        cur = obj.parent as usize;
        if cur >= table.objects.len() {
            return None;
        }
    }
    if depth == 0 {
        return None;
    }
    let first = chain[depth - 1];
    let mut walk = first;
    let object_root = loop {
        let p = table.objects[walk].parent;
        if p == NO_PARENT {
            break walk as u32;
        }
        walk = p as usize;
        if walk >= table.objects.len() {
            return None;
        }
    };
    let foreign = object_root != viewer_root;

    let mut len = 0usize;
    for (i, slot) in chain[..depth].iter().rev().enumerate() {
        let file = &table.objects[*slot];
        if i == 0 && foreign {
            let actor = &table.actors[file.actor as usize];
            let an = actor.name_len as usize;
            if len + an + 1 + file.name_len as usize > out.len() {
                return None;
            }
            out[len..len + an].copy_from_slice(&actor.name[..an]);
            len += an;
            out[len] = b'@';
            len += 1;
        } else if len > 0 {
            if len >= out.len() {
                return None;
            }
            out[len] = b'/';
            len += 1;
        }
        let name_len = file.name_len as usize;
        if len + name_len > out.len() {
            return None;
        }
        out[len..len + name_len].copy_from_slice(&file.name[..name_len]);
        len += name_len;
    }
    if table.objects[index].kind == KIND_DIR {
        if len >= out.len() {
            return None;
        }
        out[len] = b'/';
        len += 1;
    }
    Some(len)
}

/// Writes token lines `path rights\n` into `out`. Returns bytes written.
pub fn format_tokens(root: u32, tokens: &[Token; TOKEN_SLOTS], out: &mut [u8]) -> usize {
    let table = TABLE.lock();
    let mut len = 0usize;
    for token in tokens {
        if !token.is_live() || token.object as usize >= table.objects.len() {
            continue;
        }
        let mut path = [0u8; NAME_CAP];
        let Some(pn) = path_bytes(&table, token.object as usize, root, &mut path) else {
            continue;
        };
        // rights letters: r w l c x
        let mut rights = [0u8; 5];
        let mut rn = 0usize;
        if token.rights & RIGHT_READ != 0 {
            rights[rn] = b'r';
            rn += 1;
        }
        if token.rights & RIGHT_WRITE != 0 {
            rights[rn] = b'w';
            rn += 1;
        }
        if token.rights & RIGHT_LIST != 0 {
            rights[rn] = b'l';
            rn += 1;
        }
        if token.rights & RIGHT_CREATE != 0 {
            rights[rn] = b'c';
            rn += 1;
        }
        if token.rights & RIGHT_REMOVE != 0 {
            rights[rn] = b'x';
            rn += 1;
        }
        let need = pn + 1 + rn + 1;
        if len + need > out.len() {
            break;
        }
        out[len..len + pn].copy_from_slice(&path[..pn]);
        len += pn;
        out[len] = b' ';
        len += 1;
        out[len..len + rn].copy_from_slice(&rights[..rn]);
        len += rn;
        out[len] = b'\n';
        len += 1;
    }
    len
}

/// Calls `each` with every path the credentials may list.
pub fn for_each_visible(root: u32, tokens: &[Token; TOKEN_SLOTS], mut each: impl FnMut(&[u8])) {
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    for index in 0..table.objects.len() {
        let obj = &table.objects[index];
        if obj.kind == KIND_EMPTY || obj.parent == NO_PARENT {
            continue;
        }
        // LIST on this object or an ancestor (typically the parent).
        if !token_allows(&table, &cred, index as u32, RIGHT_LIST)
            && !token_allows(&table, &cred, obj.parent, RIGHT_LIST)
        {
            continue;
        }
        // Longer than one component: a sysroot rlib path is
        // `lib/rustlib/<triple>/lib/<crate>-<hash>.rlib` (~80 bytes).
        let mut buf = [0u8; 256];
        let Some(n) = path_bytes(&table, index, root, &mut buf) else {
            continue;
        };
        each(&buf[..n]);
    }
}

/// Object index of `name`, without a rights check. Missing is `NotFound`.
pub(crate) fn lookup_index(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u32, SysError> {
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    Ok(found as u32)
}

pub(crate) fn resolve_and_check(
    root: u32,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    rights: u8,
) -> Result<u32, SysError> {
    if rights == 0 {
        return Err(SysError::BadValue);
    }
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u32;
    if !token_allows(&table, &cred, index, rights) {
        return Err(SysError::AccessDenied);
    }
    Ok(index)
}

pub use galexy_galf::cards::{push_token, revoke_token};
