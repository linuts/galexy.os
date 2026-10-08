//! galfs: owner-qualified paths and tokens.
//!
//! Each actor has one root directory. `/Desktop` is that actor's child
//! named Desktop. `/dan@Desktop` is dan's. A token names an object and a
//! set of rights; the path is only a lookup. Boot creates one immortal
//! actor, [`ADMIN_NAME`]. Lock order: [`THREADS`] then this table.
//!
//! When a [`BlockDevice`] large enough for both dual slots is present,
//! the table is loaded from a GALF image (checksum + generation) or
//! formatted if both slots are bad. Mutates mark the table dirty; a
//! coalesced sync (inactive slot + flush) runs on the 1 Hz main-loop
//! tick, on `Syscall::Sync`, and before power-off. Without a usable disk
//! the table stays RAM-only. Backends: virtio-blk (preferred when
//! present), else ATA primary slave.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use spin::Mutex;

use galexy_abi::SysError;
use galexy_core::{crc32, HASH_LEN, SALT_LEN};
use galexy_crypto::{
    derive_key, hash_eq, hash_password, open, seal, wipe_bytes, KEY_LEN, NONCE_LEN, TAG_LEN,
};

const _: () = assert!(SALT_LEN == galexy_crypto::SALT_LEN);
const _: () = assert!(HASH_LEN == galexy_crypto::HASH_LEN);

use crate::drivers::ata::PrimarySlave;
use crate::drivers::block::{self, BlockDevice};
use crate::drivers::virtio_blk::VirtioBlk;

/// Objects the kernel will hold (files, directories, and actor roots).
pub const OBJECT_SLOTS: usize = 128;
/// Actors (users) the table can name.
pub const ACTOR_SLOTS: usize = 32;
/// Fixed data block size (one ATA sector).
pub const BLOCK_SIZE: usize = 512;
/// Direct block pointers per file.
pub const DIRECT_BLOCKS: usize = 8;
/// u16 pointers that fit in one single-indirect block.
pub const INDIRECT_PTRS: usize = BLOCK_SIZE / 2;
/// Bytes one file can hold (8 directs + single indirect; `len` stays u16).
pub const FILE_BYTES: usize = 32 * 1024;
/// Logical data blocks covering [`FILE_BYTES`].
const MAX_DATA_BLOCKS: usize = FILE_BYTES / BLOCK_SIZE;
/// Shared block pool capacity (Milestone 45 / GALF v8+).
pub const BLOCK_SLOTS: usize = 256;
// Host fsck (`galexy-galf`) must stay byte-identical — STYLE: no forked magic.
const _: () = assert!(OBJECT_SLOTS == galexy_galf::OBJECT_SLOTS);
const _: () = assert!(ACTOR_SLOTS == galexy_galf::ACTOR_SLOTS);
const _: () = assert!(BLOCK_SIZE == galexy_galf::BLOCK_SIZE);
const _: () = assert!(DIRECT_BLOCKS == galexy_galf::DIRECT_BLOCKS);
const _: () = assert!(INDIRECT_PTRS == galexy_galf::INDIRECT_PTRS);
const _: () = assert!(FILE_BYTES == galexy_galf::FILE_BYTES);
const _: () = assert!(BLOCK_SLOTS == galexy_galf::BLOCK_SLOTS);
const _: () = assert!(MAX_DATA_BLOCKS <= DIRECT_BLOCKS + INDIRECT_PTRS);
const _: () = assert!(FILE_BYTES <= u16::MAX as usize);
/// Tokens one task may hold.
pub const TOKEN_SLOTS: usize = 8;
/// Durable home shares recorded in the sealed image (re-applied at login).
pub const SHARE_SLOTS: usize = 32;
/// Default object quota for a new non-admin actor (root + Desktop count).
pub const DEFAULT_MAX_OBJECTS: u16 = 16;
/// Default byte quota for a new non-admin actor (sum of file lengths).
pub const DEFAULT_MAX_BYTES: u32 = 16 * 1024;
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
const NO_PARENT: u16 = 0xffff;
/// No object / empty token.
pub const NO_OBJECT: u16 = 0xffff;
/// Unused direct-block pointer.
const NO_BLOCK: u16 = 0xffff;
const BITMAP_BYTES: usize = BLOCK_SLOTS / 8;

pub const RIGHT_READ: u8 = 1;
pub const RIGHT_WRITE: u8 = 2;
pub const RIGHT_LIST: u8 = 4;
pub const RIGHT_CREATE: u8 = 8;
pub const RIGHT_REMOVE: u8 = 16;
pub const RIGHT_ALL: u8 = RIGHT_READ | RIGHT_WRITE | RIGHT_LIST | RIGHT_CREATE | RIGHT_REMOVE;
/// Task-local flag: revoke this card on the first card-based `su` it authorizes.
pub const RIGHT_ONCE: u8 = 128;
/// Stored in bit 15 of `Actor::max_objects` (quotas use the low 15 bits).
/// Set while `admin`'s password is still the default. Not a GALF version bump.
const MUST_CHANGE_BIT: u16 = 0x8000;

#[derive(Clone, Copy)]
pub struct Token {
    pub object: u16,
    pub rights: u8,
}

impl Token {
    pub const fn empty() -> Self {
        Self {
            object: NO_OBJECT,
            rights: 0,
        }
    }

    pub const fn is_live(self) -> bool {
        self.object != NO_OBJECT
    }
}

/// Credentials copied onto a task at spawn.
#[derive(Clone, Copy)]
pub struct FsCred {
    pub root: u16,
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
    pub fn launcher(root: u16) -> Self {
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
    root: u16,
    salt: [u8; SALT_LEN],
    pass_hash: [u8; HASH_LEN],
    /// Max objects (files + dirs, including root) this actor may own.
    max_objects: u16,
    /// Max sum of file lengths (bytes) this actor may own.
    max_bytes: u32,
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
            max_objects: 0,
            max_bytes: 0,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }

    fn object_limit(self) -> u32 {
        (self.max_objects & !MUST_CHANGE_BIT) as u32
    }

    fn set_password(&mut self, password: &[u8]) {
        crate::arch::rand::fill_bytes(&mut self.salt);
        hash_password(password, &self.salt, &mut self.pass_hash);
        // Default admin password keeps the must-change flag across reboot.
        if self.name_is(ADMIN_NAME) && password == ADMIN_DEFAULT_PASSWORD.as_bytes() {
            self.max_objects |= MUST_CHANGE_BIT;
        } else {
            self.max_objects &= !MUST_CHANGE_BIT;
        }
    }

    fn check_password(&self, password: &[u8]) -> bool {
        let mut got = [0u8; HASH_LEN];
        hash_password(password, &self.salt, &mut got);
        let ok = hash_eq(&got, &self.pass_hash);
        wipe_bytes(&mut got);
        ok
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Object {
    pub(crate) kind: u8,
    parent: u16,
    /// Actor that owns this object (its root's actor).
    actor: u8,
    name: [u8; NAME_CAP],
    name_len: u8,
    /// Direct block indexes into [`Table::blocks`] (`NO_BLOCK` if unused).
    blocks: [u16; DIRECT_BLOCKS],
    /// Single-indirect block holding further data-block indexes (`NO_BLOCK` none).
    indirect: u16,
    pub(crate) len: u16,
}

impl Object {
    const fn empty() -> Self {
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
    object: u16,
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

struct Table {
    actors: [Actor; ACTOR_SLOTS],
    objects: [Object; OBJECT_SLOTS],
    shares: [Share; SHARE_SLOTS],
    blocks: [[u8; BLOCK_SIZE]; BLOCK_SLOTS],
    bitmap: [u8; BITMAP_BYTES],
}

const fn empty_table() -> Table {
    Table {
        actors: [Actor::empty(); ACTOR_SLOTS],
        objects: [Object::empty(); OBJECT_SLOTS],
        shares: [Share::empty(); SHARE_SLOTS],
        blocks: [[0u8; BLOCK_SIZE]; BLOCK_SLOTS],
        bitmap: [0u8; BITMAP_BYTES],
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
static ADMIN_ROOT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(NO_OBJECT);

/// On-disk image: dual slots of header + actors + objects.
///
/// Each slot is [`DISK_SECTORS`] long. Slot 0 starts at [`disk_lba_base`];
/// slot 1 at base + [`DISK_SECTORS`]. A sync writes the inactive slot with
/// gen+1 and a CRC, then flushes — a crash mid-write leaves the previous
/// slot intact.
pub const DISK_MAGIC: [u8; 4] = *b"GALF";
/// Bumped for single-indirect file blocks (Milestone 45). Older images are
/// refused; format recreates admin under a wrapped volume key.
pub const DISK_VERSION: u16 = 11;
/// Sectors per dual-slot image (must cover header + sealed payload).
pub const DISK_SECTORS: usize = 288;
pub const DISK_SLOT_COUNT: usize = 2;
/// Common first-partition LBA (1 MiB). Tests call [`set_disk_lba_base`]
/// with this so GALF need not sit at absolute LBA 0.
pub const DISK_PART_LBA: u32 = 2048;
/// Clear header + wrap fields + data tag (see `encode_table`).
const DISK_HEADER: usize = 128;
/// used(1) + name_len(1) + name(32) + root(2) + salt(8) + hash(16)
/// + max_objects(2) + max_bytes(4) = 66.
const ACTOR_ON_DISK: usize = 66;
/// used(1) + rights(1) + grantee(1) + pad(1) + object(2) = 6.
const SHARE_ON_DISK: usize = 6;
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
/// kind+actor+name_len+pad + parent+len + name + directs + indirect.
const OBJECT_ON_DISK: usize = 8 + NAME_CAP + DIRECT_BLOCKS * 2 + 2; // 90
const _: () = assert!(OBJECT_ON_DISK == galexy_galf::OBJECT_ON_DISK);
const PAYLOAD_LEN: usize = ACTOR_ON_DISK * ACTOR_SLOTS
    + OBJECT_ON_DISK * OBJECT_SLOTS
    + SHARE_ON_DISK * SHARE_SLOTS
    + BITMAP_BYTES
    + BLOCK_SLOTS * BLOCK_SIZE;
const _: () = assert!(DISK_HEADER + PAYLOAD_LEN <= DISK_SECTORS * block::SECTOR);
const _: () = assert!(BLOCK_SLOTS.is_multiple_of(8));
const _: () = assert!(FILE_BYTES <= u16::MAX as usize);
/// Sectors both dual slots need past the LBA base (capacity gate).
const DISK_MIN_SECTORS: u64 = (DISK_SECTORS * DISK_SLOT_COUNT) as u64;
/// Default password for the immortal admin account at format.
pub const ADMIN_DEFAULT_PASSWORD: &str = "admin";
/// Bring-up volume passphrase (wraps the disk key). Test kernels and the
/// disk harness auto-unlock with this. Production `galexy-os` prompts.
pub const VOLUME_PASSPHRASE: &[u8] = b"galfs";
/// Max bytes accepted by [`unlock_volume`] (matches the syscall staging cap).
const PASSPHRASE_MAX: usize = 64;

static DISK_BUF: Mutex<[[u8; block::SECTOR]; DISK_SECTORS]> =
    Mutex::new([[0u8; block::SECTOR]; DISK_SECTORS]);
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
        crate::serial_println!("[galfs] volume locked; passphrase required");
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

/// In-RAM admin tree. Does not install a volume key or touch the disk.
fn format_ram_table() {
    let mut table = TABLE.lock();
    let admin =
        add_actor(&mut table, ADMIN_NAME, ADMIN_DEFAULT_PASSWORD.as_bytes()).expect("galfs: admin");
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

/// Mount the sealed disk with `passphrase`, or format an empty image with it.
///
/// A wrong passphrase on a current-version GALF image does **not** set
/// [`disk_corrupt`]: the RAM table stays, disk sync fails, and a later
/// passphrase can retry. Already-unlocked and diskless boots succeed.
pub fn unlock_volume(passphrase: &[u8]) -> Result<(), SysError> {
    if passphrase.is_empty() || passphrase.len() > PASSPHRASE_MAX {
        return Err(SysError::BadValue);
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
            Ok(())
        }
        DiskLoad::Empty => {
            store_passphrase(passphrase);
            let mut vk = [0u8; KEY_LEN];
            crate::arch::rand::fill_bytes(&mut vk);
            *VOLUME_KEY.lock() = Some(vk);
            wipe_bytes(&mut vk);
            if sync_to_disk() {
                crate::serial_println!(
                    "[galfs] formatted sealed disk (LBA base {})",
                    disk_lba_base()
                );
                Ok(())
            } else {
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

/// Flush a dirty unlocked volume, then zero the key and passphrase.
///
/// No-op without a usable disk. Called when the last session logs out.
pub fn seal_and_lock() {
    if !disk_usable() {
        return;
    }
    if VOLUME_KEY.lock().is_some() {
        sync_if_dirty();
    }
    wipe_volume_key();
}

/// Zero the volume key and stored passphrase. Disk image is left as last synced.
///
/// Residual RAM remanence after this wipe is an accepted cold-boot risk.
pub fn wipe_volume_key() {
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

fn sync_to_disk() -> bool {
    if !disk_usable() || DISK_CORRUPT.load(Ordering::Acquire) {
        return false;
    }
    let next_gen = ACTIVE_GEN.load(Ordering::Relaxed).wrapping_add(1);
    let next_slot = 1 - ACTIVE_SLOT.load(Ordering::Relaxed);
    let lba = slot_lba(next_slot);
    let mut buf = DISK_BUF.lock();
    {
        let table = TABLE.lock();
        if !encode_table(&table, next_gen, &mut buf) {
            crate::serial_println!("[galfs] sync skipped: volume locked");
            return false;
        }
    }
    crate::serial_println!("[galfs] committing slot {}", next_slot);
    let d = disk();
    if d.write_sectors(lba, &*buf).is_err() {
        crate::serial_println!("[galfs] disk sync write failed");
        return false;
    }
    if d.flush().is_err() {
        crate::serial_println!("[galfs] disk flush failed");
        return false;
    }
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

fn load_from_disk(passphrase: &[u8]) -> DiskLoad {
    let mut best_gen = 0u64;
    let mut best_slot: Option<u32> = None;
    // True when a slot had GALF magic at the current DISK_VERSION but
    // failed decode/validate — refuse silent format. Obsolete versions
    // (layout bumps) count as empty so format can recreate.
    let mut saw_corrupt_current = false;
    let mut bad_with_magic = 0u32;
    let mut buf = DISK_BUF.lock();
    let mut best = LOAD_BEST.lock();
    let mut cand = LOAD_CAND.lock();
    for slot in 0..DISK_SLOT_COUNT as u32 {
        let lba = slot_lba(slot);
        if disk().read_sectors(lba, &mut *buf).is_err() {
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
                "[galfs] slot {} version {} (need {}); will reformat if no valid sibling",
                slot,
                version,
                DISK_VERSION,
            );
            continue;
        }
        let Some(gen) = decode_table(&mut buf, &mut cand, passphrase) else {
            if magic && version == DISK_VERSION {
                saw_corrupt_current = true;
                bad_with_magic += 1;
                crate::serial_println!("[galfs] slot {} rejected (decode)", slot);
            }
            continue;
        };
        if !validate_table(&cand) {
            saw_corrupt_current = true;
            bad_with_magic += 1;
            crate::serial_println!("[galfs] slot {} failed validation", slot);
            continue;
        }
        if best_slot.is_none() || gen >= best_gen {
            best_gen = gen;
            best_slot = Some(slot);
            // Avoid `*best = *cand` — that materializes a Table on the stack.
            copy_table(&cand, &mut best);
        }
    }
    let Some(slot) = best_slot else {
        return if saw_corrupt_current {
            DiskLoad::Corrupt
        } else {
            DiskLoad::Empty
        };
    };
    let mut table = TABLE.lock();
    copy_table(&best, &mut table);
    refresh_roots(&table);
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
    // SAFETY: distinct Mutex-owned tables; Table is plain data.
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const Table, dst as *mut Table, 1);
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
    flat[8..10].copy_from_slice(&(OBJECT_SLOTS as u16).to_le_bytes());
    flat[10..12].copy_from_slice(&(FILE_BYTES as u16).to_le_bytes());
    flat[12] = 1; // sealed
    flat[14..16].copy_from_slice(&(BLOCK_SLOTS as u16).to_le_bytes());
    flat[16..24].copy_from_slice(&generation.to_le_bytes());

    let mut off = DISK_HEADER;
    for actor in &table.actors {
        flat[off] = u8::from(actor.used);
        flat[off + 1] = actor.name_len;
        flat[off + 2..off + 2 + ACTOR_NAME].copy_from_slice(&actor.name);
        flat[off + 2 + ACTOR_NAME..off + 4 + ACTOR_NAME].copy_from_slice(&actor.root.to_le_bytes());
        let salt_off = off + 4 + ACTOR_NAME;
        flat[salt_off..salt_off + SALT_LEN].copy_from_slice(&actor.salt);
        flat[salt_off + SALT_LEN..salt_off + SALT_LEN + HASH_LEN].copy_from_slice(&actor.pass_hash);
        let qoff = salt_off + SALT_LEN + HASH_LEN;
        flat[qoff..qoff + 2].copy_from_slice(&actor.max_objects.to_le_bytes());
        flat[qoff + 2..qoff + 6].copy_from_slice(&actor.max_bytes.to_le_bytes());
        off += ACTOR_ON_DISK;
    }
    for obj in &table.objects {
        flat[off] = obj.kind;
        flat[off + 1] = obj.actor;
        flat[off + 2] = obj.name_len;
        flat[off + 4..off + 6].copy_from_slice(&obj.parent.to_le_bytes());
        flat[off + 6..off + 8].copy_from_slice(&obj.len.to_le_bytes());
        flat[off + 8..off + 8 + NAME_CAP].copy_from_slice(&obj.name);
        let boff = off + 8 + NAME_CAP;
        for (i, blk) in obj.blocks.iter().enumerate() {
            flat[boff + i * 2..boff + i * 2 + 2].copy_from_slice(&blk.to_le_bytes());
        }
        let ioff = boff + DIRECT_BLOCKS * 2;
        flat[ioff..ioff + 2].copy_from_slice(&obj.indirect.to_le_bytes());
        off += OBJECT_ON_DISK;
    }
    for share in &table.shares {
        flat[off] = u8::from(share.used);
        flat[off + 1] = share.rights;
        flat[off + 2] = share.grantee;
        flat[off + 3] = 0;
        flat[off + 4..off + 6].copy_from_slice(&share.object.to_le_bytes());
        off += SHARE_ON_DISK;
    }
    flat[off..off + BITMAP_BYTES].copy_from_slice(&table.bitmap);
    off += BITMAP_BYTES;
    for block in &table.blocks {
        flat[off..off + BLOCK_SIZE].copy_from_slice(block);
        off += BLOCK_SIZE;
    }
    debug_assert_eq!(off, DISK_HEADER + PAYLOAD_LEN);
    debug_assert!(off <= flat.len());

    let aad = seal_aad(generation);

    // Re-wrap with the passphrase that unlocked this boot (bring-up constant
    // when nothing was stored).
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

/// Decodes a sealed slot. Returns the generation when unlock + AEAD succeed.
/// Decrypts the payload in place in `sectors`.
fn decode_table(
    sectors: &mut [[u8; block::SECTOR]; DISK_SECTORS],
    table: &mut Table,
    passphrase: &[u8],
) -> Option<u64> {
    let flat = sectors_flat_mut(sectors);
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
        actor
            .name
            .copy_from_slice(&flat[off + 2..off + 2 + ACTOR_NAME]);
        actor.root = u16::from_le_bytes([flat[off + 2 + ACTOR_NAME], flat[off + 3 + ACTOR_NAME]]);
        let salt_off = off + 4 + ACTOR_NAME;
        actor
            .salt
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
        obj.name.copy_from_slice(&flat[off + 8..off + 8 + NAME_CAP]);
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
    *VOLUME_KEY.lock() = Some(vk);
    Some(generation)
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
        if root >= OBJECT_SLOTS {
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
            || share.object as usize >= OBJECT_SLOTS
            || table.objects[share.object as usize].kind == KIND_EMPTY
        {
            return false;
        }
    }
    let mut seen = [false; BLOCK_SLOTS];
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
            if !table.actors.iter().any(|a| a.used && a.root == i as u16) {
                return false;
            }
        } else {
            let parent = obj.parent as usize;
            if parent >= OBJECT_SLOTS || parent == i {
                return false;
            }
            if table.objects[parent].kind != KIND_DIR {
                return false;
            }
            // Walk to root; refuse cycles.
            let mut cur = obj.parent;
            for _ in 0..OBJECT_SLOTS {
                if cur == i as u16 {
                    return false;
                }
                let p = table.objects[cur as usize].parent;
                if p == NO_PARENT {
                    break;
                }
                if p as usize >= OBJECT_SLOTS {
                    return false;
                }
                cur = p;
            }
        }
        if !validate_object_blocks(table, obj, &mut seen) {
            return false;
        }
    }
    for (i, used) in seen.iter().enumerate() {
        let bit = (table.bitmap[i / 8] >> (i % 8)) & 1 != 0;
        if bit != *used {
            return false;
        }
    }
    true
}

fn mark_block(seen: &mut [bool; BLOCK_SLOTS], bitmap: &[u8], b: u16) -> bool {
    if b as usize >= BLOCK_SLOTS || seen[b as usize] {
        return false;
    }
    if (bitmap[b as usize / 8] >> (b as usize % 8)) & 1 == 0 {
        return false;
    }
    seen[b as usize] = true;
    true
}

fn validate_object_blocks(table: &Table, obj: &Object, seen: &mut [bool; BLOCK_SLOTS]) -> bool {
    let bitmap = &table.bitmap;
    let need = if obj.kind == KIND_FILE {
        if obj.len as usize > FILE_BYTES {
            return false;
        }
        (obj.len as usize).div_ceil(BLOCK_SIZE)
    } else if obj.len != 0 || obj.indirect != NO_BLOCK {
        return false;
    } else {
        0
    };
    for (i, &b) in obj.blocks.iter().enumerate() {
        if i < need {
            if !mark_block(seen, bitmap, b) {
                return false;
            }
        } else if b != NO_BLOCK {
            return false;
        }
    }
    if need > DIRECT_BLOCKS {
        if obj.indirect == NO_BLOCK || !mark_block(seen, bitmap, obj.indirect) {
            return false;
        }
        let ib = obj.indirect as usize;
        for i in 0..INDIRECT_PTRS {
            let off = i * 2;
            let b = u16::from_le_bytes([table.blocks[ib][off], table.blocks[ib][off + 1]]);
            let slot = DIRECT_BLOCKS + i;
            if slot < need {
                if !mark_block(seen, bitmap, b) {
                    return false;
                }
            } else if b != NO_BLOCK {
                return false;
            }
        }
    } else if obj.indirect != NO_BLOCK {
        return false;
    }
    true
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
pub fn belongs_to_root(index: u16, root: u16) -> bool {
    let table = TABLE.lock();
    covers_object(&table, root, index)
}

/// Object indices that make up an actor's tree (root first). At most a
/// handful for the empty-delete path (root + optional Desktop).
pub fn collect_actor_objects(root: u16, out: &mut [u16]) -> usize {
    let table = TABLE.lock();
    if root as usize >= OBJECT_SLOTS || table.objects[root as usize].kind == KIND_EMPTY {
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
            out[n] = i as u16;
            n += 1;
        }
    }
    n
}

/// Drops every token whose object is in `objects`.
pub fn drop_tokens_on(tokens: &mut [Token; TOKEN_SLOTS], objects: &[u16]) {
    for token in tokens.iter_mut() {
        if objects.contains(&token.object) {
            *token = Token::empty();
        }
    }
}

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
pub fn admin_root() -> u16 {
    ADMIN_ROOT.load(Ordering::Relaxed)
}

/// Whether `root` is the immortal admin actor root.
pub fn is_admin_root(root: u16) -> bool {
    root != NO_OBJECT && root == ADMIN_ROOT.load(Ordering::Relaxed)
}

/// Writes the actor name for `root` into `out`. Returns the byte count.
///
/// [`NO_OBJECT`] (logged out) is [`SysError::AccessDenied`].
pub fn name_of_root(root: u16, out: &mut [u8]) -> Result<usize, SysError> {
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
pub fn root_named(name: &str) -> Result<u16, SysError> {
    let table = TABLE.lock();
    find_actor_root(&table, name)
}

/// Creates an actor and an empty Desktop. Returns the new root.
pub fn add_user(name: &str, password: &[u8]) -> Result<u16, SysError> {
    if name == ADMIN_NAME {
        return Err(SysError::Unsupported);
    }
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let root = add_actor(&mut table, name, password)?;
    mkdir_locked(&mut table, root, "Desktop")?;
    Ok(root)
}

/// True when `password` verifies for actor `name`.
pub fn verify_password(name: &str, password: &[u8]) -> Result<bool, SysError> {
    let table = TABLE.lock();
    let Some(actor) = table.actors.iter().find(|a| a.used && a.name_is(name)) else {
        return Err(SysError::NotFound);
    };
    Ok(actor.check_password(password))
}

/// Sets the password for actor `name`.
pub fn set_password(name: &str, password: &[u8]) -> Result<(), SysError> {
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let Some(actor) = table.actors.iter_mut().find(|a| a.used && a.name_is(name)) else {
        return Err(SysError::NotFound);
    };
    actor.set_password(password);
    drop(table);
    mark_dirty();
    Ok(())
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
        if obj.kind != KIND_DIR || !obj.name_is("Desktop") || has_child(&table, i as u16) {
            return Err(SysError::Unsupported);
        }
        desktop = Some(i);
    }
    if let Some(di) = desktop {
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
    root: u16,
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
    root: u16,
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
pub fn apply_shares(root: u16, tokens: &mut [Token; TOKEN_SLOTS]) -> Result<(), SysError> {
    let table = TABLE.lock();
    if root as usize >= OBJECT_SLOTS || table.objects[root as usize].kind != KIND_DIR {
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
        if share.object as usize >= OBJECT_SLOTS
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
pub fn holds_all(root: u16, tokens: &[Token; TOKEN_SLOTS], object: u16) -> bool {
    let table = TABLE.lock();
    let cred = cred_from_tokens(root, tokens);
    token_allows(&table, &cred, object, RIGHT_ALL)
}

/// True when actor `root` still has the default-admin must-change flag.
pub fn actor_must_change(root: u16) -> bool {
    if root == NO_OBJECT {
        return false;
    }
    let table = TABLE.lock();
    table
        .actors
        .iter()
        .any(|a| a.used && a.root == root && a.max_objects & MUST_CHANGE_BIT != 0)
}

/// AND inherited token rights with `mask`. `mask == 0` keeps every right.
/// A token whose rights fall to zero is dropped.
pub fn attenuate_tokens(tokens: &mut [Token; TOKEN_SLOTS], mask: u8) {
    if mask == 0 {
        return;
    }
    let mask = mask & RIGHT_ALL;
    for token in tokens.iter_mut() {
        if !token.is_live() {
            continue;
        }
        let once = token.rights & RIGHT_ONCE;
        token.rights = (token.rights & RIGHT_ALL & mask) | once;
        if token.rights & RIGHT_ALL == 0 {
            *token = Token::empty();
        }
    }
}

/// Drops a one-shot card that names `object` exactly. Returns whether one was removed.
pub fn consume_once(tokens: &mut [Token; TOKEN_SLOTS], object: u16) -> bool {
    let mut hit = false;
    for token in tokens.iter_mut() {
        if token.is_live() && token.object == object && token.rights & RIGHT_ONCE != 0 {
            *token = Token::empty();
            hit = true;
        }
    }
    hit
}

/// Adds an actor and an empty root. Test and boot only.
pub fn add_actor_named(name: &str, password: &[u8]) -> Result<u16, SysError> {
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    add_actor(&mut table, name, password)
}

fn admin_quota_limits() -> (u16, u32) {
    (OBJECT_SLOTS as u16, (BLOCK_SLOTS * BLOCK_SIZE) as u32)
}

fn actor_usage(table: &Table, ai: usize) -> (u32, u32) {
    let mut objects = 0u32;
    let mut bytes = 0u32;
    for obj in &table.objects {
        if obj.kind == KIND_EMPTY || obj.actor as usize != ai {
            continue;
        }
        objects += 1;
        if obj.kind == KIND_FILE {
            bytes = bytes.saturating_add(obj.len as u32);
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

fn can_add_bytes(table: &Table, ai: usize, extra: u32) -> bool {
    if ai >= ACTOR_SLOTS || !table.actors[ai].used {
        return false;
    }
    let (_, used) = actor_usage(table, ai);
    used.saturating_add(extra) <= table.actors[ai].max_bytes
}

fn subtree_usage(table: &Table, root: u16) -> (u32, u32) {
    let mut objects = 0u32;
    let mut bytes = 0u32;
    for i in 0..OBJECT_SLOTS {
        if table.objects[i].kind == KIND_EMPTY {
            continue;
        }
        if covers_object(table, root, i as u16) {
            objects += 1;
            if table.objects[i].kind == KIND_FILE {
                bytes = bytes.saturating_add(table.objects[i].len as u32);
            }
        }
    }
    (objects, bytes)
}

fn add_actor(table: &mut Table, name: &str, password: &[u8]) -> Result<u16, SysError> {
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
        admin_quota_limits()
    } else {
        (DEFAULT_MAX_OBJECTS, DEFAULT_MAX_BYTES)
    };
    let actor = &mut table.actors[ai];
    actor.used = true;
    actor.name = [0; ACTOR_NAME];
    actor.name[..name.len()].copy_from_slice(name.as_bytes());
    actor.name_len = name.len() as u8;
    actor.root = oi as u16;
    actor.max_objects = max_objects;
    actor.max_bytes = max_bytes;
    actor.set_password(password);
    let obj = &mut table.objects[oi];
    *obj = Object::empty();
    obj.kind = KIND_DIR;
    obj.parent = NO_PARENT;
    obj.actor = ai as u8;
    // Root has an empty name; paths start at its children.
    Ok(oi as u16)
}

/// Sets durable object/byte limits for actor `name`. Admin-only at the syscall layer.
pub fn set_actor_quota(name: &str, max_objects: u16, max_bytes: u32) -> Result<(), SysError> {
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
    Ok((used_o, actor.object_limit(), used_b, actor.max_bytes))
}

/// Quota for the actor that owns `root`, or `NotFound` if unset.
pub fn root_quota(root: u16) -> Result<(u32, u32, u32, u32), SysError> {
    let table = TABLE.lock();
    if root as usize >= OBJECT_SLOTS || table.objects[root as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    let ai = table.objects[root as usize].actor as usize;
    if ai >= ACTOR_SLOTS || !table.actors[ai].used {
        return Err(SysError::NotFound);
    }
    let (used_o, used_b) = actor_usage(&table, ai);
    let actor = &table.actors[ai];
    Ok((used_o, actor.object_limit(), used_b, actor.max_bytes))
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
pub fn find_under(parent: u16, name: &str) -> Option<u16> {
    let table = TABLE.lock();
    find_child(&table, parent, name).map(|i| i as u16)
}

/// Creates a directory under an actor's root. Test helper.
pub fn mkdir_under_root(root: u16, name: &str) -> Result<u16, SysError> {
    if !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let oi = mkdir_locked(&mut table, root, name)?;
    drop(table);
    mark_dirty();
    Ok(oi)
}

fn mkdir_locked(table: &mut Table, root: u16, name: &str) -> Result<u16, SysError> {
    if root as usize >= OBJECT_SLOTS || table.objects[root as usize].kind != KIND_DIR {
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
    Ok(oi as u16)
}

/// Creates an empty file under `parent`. Test helper.
pub fn create_file_under(parent: u16, name: &str) -> Result<u16, SysError> {
    if !component_ok(name) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    if parent as usize >= OBJECT_SLOTS || table.objects[parent as usize].kind != KIND_DIR {
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
    obj.blocks = [NO_BLOCK; DIRECT_BLOCKS];
    obj.indirect = NO_BLOCK;
    obj.len = 0;
    drop(table);
    mark_dirty();
    Ok(oi as u16)
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

fn find_actor_root(table: &Table, name: &str) -> Result<u16, SysError> {
    table
        .actors
        .iter()
        .find(|a| a.name_is(name))
        .map(|a| a.root)
        .ok_or(SysError::NotFound)
}

fn find_child(table: &Table, parent: u16, name: &str) -> Option<usize> {
    table
        .objects
        .iter()
        .position(|o| o.kind != KIND_EMPTY && o.parent == parent && o.name_is(name))
}

fn has_child(table: &Table, parent: u16) -> bool {
    table
        .objects
        .iter()
        .any(|o| o.kind != KIND_EMPTY && o.parent == parent)
}

/// Walks every component except the last under `start`.
fn walk_parents(table: &Table, start: u16, parsed: &ParsedPath<'_>) -> Result<u16, SysError> {
    let mut parent = start;
    for comp in &parsed.comps[..parsed.n.saturating_sub(1)] {
        let Some(index) = find_child(table, parent, comp) else {
            return Err(SysError::NotFound);
        };
        if table.objects[index].kind != KIND_DIR {
            return Err(SysError::NotFound);
        }
        parent = index as u16;
    }
    Ok(parent)
}

fn start_root(table: &Table, cred: &FsCred, parsed: &ParsedPath<'_>) -> Result<u16, SysError> {
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

/// True when `ancestor` is `object` or a parent of it.
fn covers_object(table: &Table, ancestor: u16, object: u16) -> bool {
    let mut cur = object;
    for _ in 0..OBJECT_SLOTS {
        if cur == ancestor {
            return true;
        }
        if cur as usize >= OBJECT_SLOTS {
            return false;
        }
        let parent = table.objects[cur as usize].parent;
        if parent == NO_PARENT {
            return cur == ancestor;
        }
        cur = parent;
    }
    false
}

/// Whether `cred` holds `need` on `object` or an ancestor.
///
/// Admin is not a blanket bypass. `useradd` / `userdel` / `passwd` of
/// another account stay admin-only via [`is_admin_root`]. Foreign trees
/// need an explicit card (or `su`, which installs `ALL` on that root).
fn token_allows(table: &Table, cred: &FsCred, object: u16, need: u8) -> bool {
    let need = need & RIGHT_ALL;
    if need == 0 {
        return false;
    }
    for token in &cred.tokens {
        if !token.is_live() || token.rights & RIGHT_ALL & need != need {
            continue;
        }
        if covers_object(table, token.object, object) {
            return true;
        }
    }
    false
}

fn cred_from_tokens(root: u16, tokens: &[Token; TOKEN_SLOTS]) -> FsCred {
    FsCred {
        root,
        tokens: *tokens,
    }
}

/// Resolves `parsed` to an object index, checking existence only.
fn lookup(table: &Table, cred: &FsCred, parsed: &ParsedPath<'_>) -> Result<(u16, usize), SysError> {
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
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u16, SysError> {
    let parsed = parse_path(name)?;
    if parsed.dir {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u16;
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
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    replace: bool,
) -> Result<Option<u16>, SysError> {
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
            let index = found as u16;
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
    obj.blocks = [NO_BLOCK; DIRECT_BLOCKS];
    obj.indirect = NO_BLOCK;
    obj.len = 0;
    if parsed.dir {
        Ok(None)
    } else {
        Ok(Some(oi as u16))
    }
}

/// Removes a file or empty directory.
pub(crate) fn remove(
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
) -> Result<u16, SysError> {
    let parsed = parse_path(name)?;
    if parsed.n == 0 {
        return Err(SysError::Unsupported);
    }
    let cred = cred_from_tokens(root, tokens);
    let mut table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u16;
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
    table.objects[found] = Object::empty();
    Ok(index)
}

/// File length under the global lock.
pub(crate) fn with_file<R>(index: u16, f: impl FnOnce(&Object) -> R) -> Option<R> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(f(&table.objects[i]))
}

fn alloc_block(table: &mut Table) -> Option<u16> {
    for i in 0..BLOCK_SLOTS {
        let mask = 1u8 << (i % 8);
        if table.bitmap[i / 8] & mask == 0 {
            table.bitmap[i / 8] |= mask;
            table.blocks[i] = [0; BLOCK_SIZE];
            return Some(i as u16);
        }
    }
    None
}

fn free_block(table: &mut Table, block: u16) {
    let i = block as usize;
    if block == NO_BLOCK || i >= BLOCK_SLOTS {
        return;
    }
    table.bitmap[i / 8] &= !(1u8 << (i % 8));
    table.blocks[i] = [0; BLOCK_SIZE];
}

fn free_file_blocks(table: &mut Table, index: usize) {
    for slot in 0..MAX_DATA_BLOCKS {
        let b = data_block(table, index, slot);
        if b != NO_BLOCK {
            free_block(table, b);
            let _ = set_data_block(table, index, slot, NO_BLOCK);
        }
    }
    let indirect = table.objects[index].indirect;
    if indirect != NO_BLOCK {
        free_block(table, indirect);
        table.objects[index].indirect = NO_BLOCK;
    }
    table.objects[index].blocks = [NO_BLOCK; DIRECT_BLOCKS];
    table.objects[index].len = 0;
}

/// Logical data-block index → physical pool index (`NO_BLOCK` if unset).
fn data_block(table: &Table, oi: usize, slot: usize) -> u16 {
    if slot < DIRECT_BLOCKS {
        return table.objects[oi].blocks[slot];
    }
    let ii = slot - DIRECT_BLOCKS;
    let indirect = table.objects[oi].indirect;
    if indirect == NO_BLOCK || ii >= INDIRECT_PTRS {
        return NO_BLOCK;
    }
    let off = ii * 2;
    let ib = indirect as usize;
    u16::from_le_bytes([table.blocks[ib][off], table.blocks[ib][off + 1]])
}

/// Sets the physical block for logical data-block `slot`. Allocates the
/// indirect block when first needed. Returns false if allocation fails.
fn set_data_block(table: &mut Table, oi: usize, slot: usize, block: u16) -> bool {
    if slot < DIRECT_BLOCKS {
        table.objects[oi].blocks[slot] = block;
        return true;
    }
    let ii = slot - DIRECT_BLOCKS;
    if ii >= INDIRECT_PTRS || slot >= MAX_DATA_BLOCKS {
        return false;
    }
    if table.objects[oi].indirect == NO_BLOCK {
        if block == NO_BLOCK {
            return true;
        }
        let Some(ib) = alloc_block(table) else {
            return false;
        };
        // Pointer slots must be NO_BLOCK (0xffff), not zero (block 0).
        table.blocks[ib as usize].fill(0xff);
        table.objects[oi].indirect = ib;
    }
    let ib = table.objects[oi].indirect as usize;
    let off = ii * 2;
    table.blocks[ib][off..off + 2].copy_from_slice(&block.to_le_bytes());
    true
}

/// Ensures logical data-block `slot` has a pool block; allocates if needed.
fn ensure_data_block(table: &mut Table, oi: usize, slot: usize) -> Option<u16> {
    let existing = data_block(table, oi, slot);
    if existing != NO_BLOCK {
        return Some(existing);
    }
    let b = alloc_block(table)?;
    if !set_data_block(table, oi, slot, b) {
        free_block(table, b);
        return None;
    }
    Some(b)
}

/// Appends `src` to a file. Returns the byte count written (short on full
/// file, exhausted block pool, or actor byte quota).
pub(crate) fn append(index: u16, src: &[u8]) -> Option<usize> {
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
    }
    let ai = table.objects[i].actor as usize;
    let mut written = 0usize;
    while written < src.len() {
        let pos = table.objects[i].len as usize;
        if pos >= FILE_BYTES {
            break;
        }
        if !can_add_bytes(&table, ai, 1) {
            break;
        }
        let slot = pos / BLOCK_SIZE;
        let off = pos % BLOCK_SIZE;
        let Some(bi) = ensure_data_block(&mut table, i, slot) else {
            break;
        };
        let bi = bi as usize;
        let mut room = (BLOCK_SIZE - off)
            .min(src.len() - written)
            .min(FILE_BYTES - pos);
        let (_, used) = actor_usage(&table, ai);
        let headroom = table.actors[ai].max_bytes.saturating_sub(used) as usize;
        if headroom == 0 {
            break;
        }
        room = room.min(headroom);
        table.blocks[bi][off..off + room].copy_from_slice(&src[written..written + room]);
        table.objects[i].len = (pos + room) as u16;
        written += room;
    }
    Some(written)
}

/// Copies `dst.len()` bytes from file offset `start`.
pub(crate) fn read_at(index: u16, start: usize, dst: &mut [u8]) -> Option<usize> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
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
        let b = data_block(&table, i, slot);
        if b == NO_BLOCK || b as usize >= BLOCK_SLOTS {
            return None;
        }
        let n = (BLOCK_SIZE - off).min(want - copied);
        dst[copied..copied + n].copy_from_slice(&table.blocks[b as usize][off..off + n]);
        copied += n;
    }
    Some(copied)
}

/// Appends bytes to a file by object index. Test helper.
pub fn append_file(index: u16, src: &[u8]) -> Option<usize> {
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
pub fn read_file_bytes(index: u16, out: &mut [u8]) -> Option<usize> {
    read_at(index, 0, out)
}

/// How many blocks are currently allocated in the pool (test helper).
pub fn blocks_used() -> usize {
    let table = TABLE.lock();
    table.bitmap.iter().map(|b| b.count_ones() as usize).sum()
}

/// Moves a dirent as admin. Test helper.
pub fn rename_as_admin(old: &str, new: &str) -> Result<(), SysError> {
    let cred = admin_cred();
    rename(cred.root, &cred.tokens, old, new)?;
    mark_dirty();
    Ok(())
}

/// Sets a file's length. Test helper (object index).
pub fn truncate_file(index: u16, new_len: usize) -> Result<(), SysError> {
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
    root: u16,
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
    let src = found as u16;
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
    place_name(&mut table.objects[found], new_last);
    table.objects[found].parent = new_parent;
    if table.objects[found].actor != new_actor {
        reassign_actor_subtree(&mut table, src, new_actor);
    }
    Ok(())
}

fn is_descendant(table: &Table, ancestor: u16, node: u16) -> bool {
    let mut cur = node;
    for _ in 0..OBJECT_SLOTS {
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

fn reassign_actor_subtree(table: &mut Table, root: u16, actor: u8) {
    for i in 0..OBJECT_SLOTS {
        if table.objects[i].kind == KIND_EMPTY {
            continue;
        }
        if covers_object(table, root, i as u16) {
            table.objects[i].actor = actor;
        }
    }
}

/// Sets a file's length, allocating or freeing data blocks as needed.
pub(crate) fn truncate(index: u16, new_len: usize) -> Result<(), SysError> {
    if new_len > FILE_BYTES {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
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
            new_len.div_ceil(BLOCK_SIZE)
        };
        for slot in keep..MAX_DATA_BLOCKS {
            let b = data_block(&table, i, slot);
            if b != NO_BLOCK {
                free_block(&mut table, b);
                let _ = set_data_block(&mut table, i, slot, NO_BLOCK);
            }
        }
        if keep <= DIRECT_BLOCKS {
            let indirect = table.objects[i].indirect;
            if indirect != NO_BLOCK {
                free_block(&mut table, indirect);
                table.objects[i].indirect = NO_BLOCK;
            }
        }
        if new_len > 0 {
            let slot = (new_len - 1) / BLOCK_SIZE;
            let off = new_len % BLOCK_SIZE;
            if off != 0 {
                let b = data_block(&table, i, slot) as usize;
                table.blocks[b][off..].fill(0);
            }
        }
        table.objects[i].len = new_len as u16;
        return Ok(());
    }

    // Grow with zero-fill; refuse when the actor byte quota would break.
    let ai = table.objects[i].actor as usize;
    let grow = (new_len - old) as u32;
    if !can_add_bytes(&table, ai, grow) {
        return Err(SysError::NoResource);
    }

    // Grow with zero-fill; roll back on pool exhaustion.
    let saved_directs = table.objects[i].blocks;
    let saved_indirect = table.objects[i].indirect;
    let saved_indirect_bytes = if saved_indirect != NO_BLOCK {
        Some(table.blocks[saved_indirect as usize])
    } else {
        None
    };
    let mut pos = old;
    while pos < new_len {
        let slot = pos / BLOCK_SIZE;
        let Some(bi) = ensure_data_block(&mut table, i, slot) else {
            // Roll back newly allocated data / indirect blocks.
            for s in 0..MAX_DATA_BLOCKS {
                let now = data_block(&table, i, s);
                let was = match saved_directs.get(s) {
                    Some(&direct) => direct,
                    None => match saved_indirect_bytes {
                        Some(bytes) => {
                            let ii = s - DIRECT_BLOCKS;
                            u16::from_le_bytes([bytes[ii * 2], bytes[ii * 2 + 1]])
                        }
                        None => NO_BLOCK,
                    },
                };
                if now != was && now != NO_BLOCK {
                    free_block(&mut table, now);
                }
            }
            let now_indirect = table.objects[i].indirect;
            if now_indirect != saved_indirect && now_indirect != NO_BLOCK {
                free_block(&mut table, now_indirect);
            }
            table.objects[i].blocks = saved_directs;
            table.objects[i].indirect = saved_indirect;
            if let (Some(bytes), true) = (saved_indirect_bytes, saved_indirect != NO_BLOCK) {
                table.blocks[saved_indirect as usize] = bytes;
            }
            table.objects[i].len = old as u16;
            return Err(SysError::NoResource);
        };
        let bi = bi as usize;
        let off = pos % BLOCK_SIZE;
        let n = (BLOCK_SIZE - off).min(new_len - pos);
        if pos >= old {
            table.blocks[bi][off..off + n].fill(0);
        }
        pos += n;
    }
    table.objects[i].len = new_len as u16;
    Ok(())
}

/// Fills `out` (must be [`galexy_abi::STAT_LEN`]) with path metadata.
pub(crate) fn stat(
    root: u16,
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
    let index = found as u16;
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
        obj.len as u32
    } else {
        0
    };
    let actor = &table.actors[obj.actor as usize];
    let owner_len = actor.name_len.min(32);
    out[..STAT_LEN].fill(0);
    out[0] = kind;
    out[1] = rights;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    out[8] = owner_len;
    out[9..9 + owner_len as usize].copy_from_slice(&actor.name[..owner_len as usize]);
    Ok(STAT_LEN)
}

fn held_rights(table: &Table, cred: &FsCred, object: u16) -> u8 {
    let mut rights = 0u8;
    for token in &cred.tokens {
        if token.is_live() && covers_object(table, token.object, object) {
            rights |= token.rights & RIGHT_ALL;
        }
    }
    rights
}

fn path_bytes(table: &Table, index: usize, viewer_root: u16, out: &mut [u8]) -> Option<usize> {
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
        if cur >= OBJECT_SLOTS {
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
            break walk as u16;
        }
        walk = p as usize;
        if walk >= OBJECT_SLOTS {
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
pub fn format_tokens(root: u16, tokens: &[Token; TOKEN_SLOTS], out: &mut [u8]) -> usize {
    let table = TABLE.lock();
    let mut len = 0usize;
    for token in tokens {
        if !token.is_live() || token.object as usize >= OBJECT_SLOTS {
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
pub fn for_each_visible(root: u16, tokens: &[Token; TOKEN_SLOTS], mut each: impl FnMut(&[u8])) {
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    for index in 0..OBJECT_SLOTS {
        let obj = &table.objects[index];
        if obj.kind == KIND_EMPTY || obj.parent == NO_PARENT {
            continue;
        }
        // LIST on this object or an ancestor (typically the parent).
        if !token_allows(&table, &cred, index as u16, RIGHT_LIST)
            && !token_allows(&table, &cred, obj.parent, RIGHT_LIST)
        {
            continue;
        }
        let mut buf = [0u8; NAME_CAP];
        let Some(n) = path_bytes(&table, index, root, &mut buf) else {
            continue;
        };
        each(&buf[..n]);
    }
}

/// Resolves `name` and checks that `cred` holds every bit in `rights`.
/// Returns the object index. Used by the `grant` syscall.
pub(crate) fn resolve_and_check(
    root: u16,
    tokens: &[Token; TOKEN_SLOTS],
    name: &str,
    rights: u8,
) -> Result<u16, SysError> {
    if rights == 0 {
        return Err(SysError::BadValue);
    }
    let parsed = parse_path(name)?;
    let cred = cred_from_tokens(root, tokens);
    let table = TABLE.lock();
    let (_parent, found) = lookup(&table, &cred, &parsed)?;
    let index = found as u16;
    if !token_allows(&table, &cred, index, rights) {
        return Err(SysError::AccessDenied);
    }
    Ok(index)
}

/// Installs a token into a fixed slot array. Same object merges rights.
pub fn push_token(
    tokens: &mut [Token; TOKEN_SLOTS],
    object: u16,
    rights: u8,
) -> Result<(), SysError> {
    if object == NO_OBJECT || rights == 0 {
        return Err(SysError::BadValue);
    }
    if let Some(slot) = tokens.iter_mut().find(|t| t.object == object) {
        slot.rights |= rights;
        return Ok(());
    }
    if let Some(slot) = tokens.iter_mut().find(|t| !t.is_live()) {
        *slot = Token { object, rights };
        Ok(())
    } else {
        Err(SysError::NoResource)
    }
}

/// Clears `rights` from the token that names `object` exactly.
pub fn revoke_token(
    tokens: &mut [Token; TOKEN_SLOTS],
    object: u16,
    rights: u8,
) -> Result<(), SysError> {
    if object == NO_OBJECT || rights == 0 {
        return Err(SysError::BadValue);
    }
    let Some(slot) = tokens.iter_mut().find(|t| t.object == object) else {
        return Err(SysError::NotFound);
    };
    slot.rights &= !rights;
    if slot.rights == 0 {
        *slot = Token::empty();
    }
    Ok(())
}
