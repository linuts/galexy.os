//! galfs: owner-qualified paths and tokens.
//!
//! Each actor has one root directory. `/Desktop` is that actor's child
//! named Desktop. `/dan@Desktop` is dan's. A token names an object and a
//! set of rights; the path is only a lookup. Boot creates one immortal
//! actor, [`ADMIN_NAME`]. Lock order: [`THREADS`] then this table.
//!
//! When the primary IDE slave is present, the table is loaded from a
//! dual-slot GALF image (checksum + generation) or formatted if both
//! slots are bad. Mutates sync to the inactive slot then flush. Without
//! a slave the table stays RAM-only.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use spin::Mutex;

use galexy_abi::SysError;
use galexy_core::{crc32, HASH_LEN, SALT_LEN};
use galexy_crypto::{
    derive_key, hash_eq, hash_password, open, seal, wipe_bytes, KEY_LEN, NONCE_LEN, TAG_LEN,
};

const _: () = assert!(SALT_LEN == galexy_crypto::SALT_LEN);
const _: () = assert!(HASH_LEN == galexy_crypto::HASH_LEN);

use crate::drivers::ata;

/// Objects the kernel will hold (files, directories, and actor roots).
pub const OBJECT_SLOTS: usize = 64;
/// Actors (users) the table can name.
const ACTOR_SLOTS: usize = 16;
/// Bytes one file can hold.
pub const FILE_BYTES: usize = 512;
/// Tokens one task may hold.
pub const TOKEN_SLOTS: usize = 8;
/// Path components after the optional owner.
pub const MAX_DEPTH: usize = 8;
/// One path-component name.
const NAME_CAP: usize = 64;
/// Actor name length.
const ACTOR_NAME: usize = 32;

const KIND_EMPTY: u8 = 0;
pub(crate) const KIND_FILE: u8 = 1;
pub(crate) const KIND_DIR: u8 = 2;

/// Parent of an actor root.
const NO_PARENT: u16 = 0xffff;
/// No object / empty token.
pub const NO_OBJECT: u16 = 0xffff;

pub const RIGHT_READ: u8 = 1;
pub const RIGHT_WRITE: u8 = 2;
pub const RIGHT_LIST: u8 = 4;
pub const RIGHT_CREATE: u8 = 8;
pub const RIGHT_REMOVE: u8 = 16;
pub const RIGHT_ALL: u8 = RIGHT_READ | RIGHT_WRITE | RIGHT_LIST | RIGHT_CREATE | RIGHT_REMOVE;

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
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }

    fn set_password(&mut self, password: &[u8]) {
        crate::arch::rand::fill_bytes(&mut self.salt);
        hash_password(password, &self.salt, &mut self.pass_hash);
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
    pub(crate) data: [u8; FILE_BYTES],
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
            data: [0; FILE_BYTES],
            len: 0,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.kind != KIND_EMPTY && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

struct Table {
    actors: [Actor; ACTOR_SLOTS],
    objects: [Object; OBJECT_SLOTS],
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    actors: [Actor::empty(); ACTOR_SLOTS],
    objects: [Object::empty(); OBJECT_SLOTS],
});

static BOOTED: AtomicBool = AtomicBool::new(false);
/// True when the ATA slave accepted a load or format write.
static DISK_LIVE: AtomicBool = AtomicBool::new(false);
/// Slot (0 or 1) that holds the newest valid image; next sync writes the other.
static ACTIVE_SLOT: AtomicU32 = AtomicU32::new(0);
/// Generation of the active slot (next sync writes gen + 1).
static ACTIVE_GEN: AtomicU64 = AtomicU64::new(0);

/// Name of the immortal boot actor.
pub const ADMIN_NAME: &str = "admin";

/// Admin's root object. Valid after [`init`].
static ADMIN_ROOT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(NO_OBJECT);

/// On-disk image: dual slots of header + actors + objects.
///
/// Each slot is [`DISK_SECTORS`] long. Slot 0 starts at LBA 0; slot 1 at
/// LBA [`DISK_SECTORS`]. A sync writes the inactive slot with gen+1 and a
/// CRC, then flushes — a crash mid-write leaves the previous slot intact.
pub const DISK_MAGIC: [u8; 4] = *b"GALF";
/// Bumped for sealed slots (ChaCha20-HMAC volume AEAD). v5 plaintext
/// images are refused; format recreates admin under a wrapped volume key.
pub const DISK_VERSION: u16 = 6;
pub const DISK_SECTORS: usize = 80;
pub const DISK_SLOT_COUNT: usize = 2;
/// Clear header + wrap fields + data tag (see `encode_table`).
const DISK_HEADER: usize = 128;
/// used(1) + name_len(1) + name(32) + root(2) + salt(8) + hash(16) = 60.
const ACTOR_ON_DISK: usize = 60;
const OBJECT_ON_DISK: usize = 8 + NAME_CAP + FILE_BYTES; // 584
const PAYLOAD_LEN: usize = ACTOR_ON_DISK * ACTOR_SLOTS + OBJECT_ON_DISK * OBJECT_SLOTS;
const _: () = assert!(DISK_HEADER + PAYLOAD_LEN <= DISK_SECTORS * ata::SECTOR);
/// Default password for the immortal admin account at format.
pub const ADMIN_DEFAULT_PASSWORD: &str = "admin";
/// Bring-up volume passphrase (wraps the disk key). Interactive unlock is
/// a follow-up; tests and `cargo run` use this constant for now.
pub const VOLUME_PASSPHRASE: &[u8] = b"galfs";

static DISK_BUF: Mutex<[[u8; ata::SECTOR]; DISK_SECTORS]> =
    Mutex::new([[0u8; ata::SECTOR]; DISK_SECTORS]);
/// Unwrapped volume key while the disk is mounted. `None` when locked / RAM-only.
static VOLUME_KEY: Mutex<Option<[u8; KEY_LEN]>> = Mutex::new(None);

/// Builds actor [`ADMIN_NAME`] with an empty Desktop, or loads the newest
/// valid GALF slot from the ATA slave. Call once.
pub fn init() {
    if BOOTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if ata::present() && load_from_disk() {
        crate::serial_println!(
            "[galfs] loaded slot {} gen {}",
            ACTIVE_SLOT.load(Ordering::Relaxed),
            ACTIVE_GEN.load(Ordering::Relaxed),
        );
        return;
    }
    let mut table = TABLE.lock();
    let admin = add_actor(&mut table, ADMIN_NAME, ADMIN_DEFAULT_PASSWORD.as_bytes())
        .expect("galfs: admin");
    ADMIN_ROOT.store(admin, Ordering::Relaxed);
    mkdir_locked(&mut table, admin, "Desktop").expect("galfs: admin Desktop");
    drop(table);
    ACTIVE_SLOT.store(0, Ordering::Relaxed);
    ACTIVE_GEN.store(0, Ordering::Relaxed);
    // Fresh volume key for a sealed format (bring-up passphrase).
    let mut vk = [0u8; KEY_LEN];
    crate::arch::rand::fill_bytes(&mut vk);
    *VOLUME_KEY.lock() = Some(vk);
    wipe_bytes(&mut vk);
    if sync_to_disk() {
        crate::serial_println!("[galfs] formatted sealed disk");
    }
}

/// True when this boot is using the ATA slave for the table.
pub fn disk_backed() -> bool {
    DISK_LIVE.load(Ordering::Acquire)
}

/// Writes the in-RAM table to the slave. No-op when no disk is attached.
pub fn sync() {
    let _ = sync_to_disk();
}

fn sync_to_disk() -> bool {
    if !ata::present() {
        return false;
    }
    if VOLUME_KEY.lock().is_none() {
        crate::serial_println!("[galfs] sync skipped: volume locked");
        return false;
    }
    let next_gen = ACTIVE_GEN.load(Ordering::Relaxed).wrapping_add(1);
    let next_slot = 1 - ACTIVE_SLOT.load(Ordering::Relaxed);
    let lba = (next_slot as usize * DISK_SECTORS) as u32;
    let mut buf = DISK_BUF.lock();
    {
        let table = TABLE.lock();
        encode_table(&table, next_gen, &mut buf);
    }
    if ata::write_sectors(lba, &*buf).is_err() {
        crate::serial_println!("[galfs] disk sync write failed");
        return false;
    }
    if ata::flush().is_err() {
        crate::serial_println!("[galfs] disk flush failed");
        return false;
    }
    ACTIVE_SLOT.store(next_slot, Ordering::Release);
    ACTIVE_GEN.store(next_gen, Ordering::Release);
    DISK_LIVE.store(true, Ordering::Release);
    true
}

fn load_from_disk() -> bool {
    let mut best_gen = 0u64;
    let mut best_slot: Option<u32> = None;
    let mut best_table = Table {
        actors: [Actor::empty(); ACTOR_SLOTS],
        objects: [Object::empty(); OBJECT_SLOTS],
    };
    let mut buf = DISK_BUF.lock();
    for slot in 0..DISK_SLOT_COUNT as u32 {
        let lba = (slot as usize * DISK_SECTORS) as u32;
        if ata::read_sectors(lba, &mut *buf).is_err() {
            continue;
        }
        let mut candidate = Table {
            actors: [Actor::empty(); ACTOR_SLOTS],
            objects: [Object::empty(); OBJECT_SLOTS],
        };
        let Some(gen) = decode_table(&mut buf, &mut candidate) else {
            continue;
        };
        if !validate_table(&candidate) {
            crate::serial_println!("[galfs] slot {} failed validation", slot);
            continue;
        }
        if best_slot.is_none() || gen >= best_gen {
            best_gen = gen;
            best_slot = Some(slot);
            best_table = candidate;
        }
    }
    let Some(slot) = best_slot else {
        return false;
    };
    let mut table = TABLE.lock();
    *table = best_table;
    refresh_roots(&table);
    ACTIVE_SLOT.store(slot, Ordering::Release);
    ACTIVE_GEN.store(best_gen, Ordering::Release);
    DISK_LIVE.store(true, Ordering::Release);
    true
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

fn encode_table(table: &Table, generation: u64, sectors: &mut [[u8; ata::SECTOR]; DISK_SECTORS]) {
    let vk = VOLUME_KEY
        .lock()
        .expect("galfs: encode requires unlocked volume");
    let flat = sectors_flat_mut(sectors);
    flat.fill(0);
    flat[0..4].copy_from_slice(&DISK_MAGIC);
    flat[4..6].copy_from_slice(&DISK_VERSION.to_le_bytes());
    flat[6..8].copy_from_slice(&(ACTOR_SLOTS as u16).to_le_bytes());
    flat[8..10].copy_from_slice(&(OBJECT_SLOTS as u16).to_le_bytes());
    flat[10..12].copy_from_slice(&(FILE_BYTES as u16).to_le_bytes());
    flat[12] = 1; // sealed
    flat[16..24].copy_from_slice(&generation.to_le_bytes());

    let mut off = DISK_HEADER;
    for actor in &table.actors {
        flat[off] = u8::from(actor.used);
        flat[off + 1] = actor.name_len;
        flat[off + 2..off + 2 + ACTOR_NAME].copy_from_slice(&actor.name);
        flat[off + 2 + ACTOR_NAME..off + 4 + ACTOR_NAME]
            .copy_from_slice(&actor.root.to_le_bytes());
        let salt_off = off + 4 + ACTOR_NAME;
        flat[salt_off..salt_off + SALT_LEN].copy_from_slice(&actor.salt);
        flat[salt_off + SALT_LEN..salt_off + SALT_LEN + HASH_LEN]
            .copy_from_slice(&actor.pass_hash);
        off += ACTOR_ON_DISK;
    }
    for obj in &table.objects {
        flat[off] = obj.kind;
        flat[off + 1] = obj.actor;
        flat[off + 2] = obj.name_len;
        flat[off + 4..off + 6].copy_from_slice(&obj.parent.to_le_bytes());
        flat[off + 6..off + 8].copy_from_slice(&obj.len.to_le_bytes());
        flat[off + 8..off + 8 + NAME_CAP].copy_from_slice(&obj.name);
        flat[off + 8 + NAME_CAP..off + 8 + NAME_CAP + FILE_BYTES].copy_from_slice(&obj.data);
        off += OBJECT_ON_DISK;
    }
    debug_assert_eq!(off, DISK_HEADER + PAYLOAD_LEN);
    debug_assert!(off <= flat.len());

    let aad = seal_aad(generation);

    // Wrap volume key with KEK from the bring-up passphrase.
    let mut kdf_salt = [0u8; SALT_LEN];
    let mut wrap_nonce = [0u8; NONCE_LEN];
    let mut data_nonce = [0u8; NONCE_LEN];
    crate::arch::rand::fill_bytes(&mut kdf_salt);
    crate::arch::rand::fill_bytes(&mut wrap_nonce);
    crate::arch::rand::fill_bytes(&mut data_nonce);
    let mut kek = [0u8; KEY_LEN];
    derive_key(VOLUME_PASSPHRASE, &kdf_salt, &mut kek);
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
    sectors: &mut [[u8; ata::SECTOR]; DISK_SECTORS],
    table: &mut Table,
) -> Option<u64> {
    let flat = sectors_flat_mut(sectors);
    if flat[0..4] != DISK_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([flat[4], flat[5]]);
    let actors = u16::from_le_bytes([flat[6], flat[7]]) as usize;
    let objects = u16::from_le_bytes([flat[8], flat[9]]) as usize;
    let file_bytes = u16::from_le_bytes([flat[10], flat[11]]) as usize;
    if version != DISK_VERSION
        || actors != ACTOR_SLOTS
        || objects != OBJECT_SLOTS
        || file_bytes != FILE_BYTES
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
    derive_key(VOLUME_PASSPHRASE, &kdf_salt, &mut kek);
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
        obj.data
            .copy_from_slice(&flat[off + 8 + NAME_CAP..off + 8 + NAME_CAP + FILE_BYTES]);
        if obj.len as usize > FILE_BYTES {
            let mut gone = vk;
            wipe_bytes(&mut gone);
            return None;
        }
        off += OBJECT_ON_DISK;
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
            continue;
        }
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
    true
}

fn sectors_flat_mut(sectors: &mut [[u8; ata::SECTOR]; DISK_SECTORS]) -> &mut [u8] {
    // SAFETY: `[[u8; SECTOR]; N]` is contiguous bytes with no padding.
    unsafe {
        core::slice::from_raw_parts_mut(
            sectors.as_mut_ptr().cast::<u8>(),
            DISK_SECTORS * ata::SECTOR,
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
    sync();
    Ok(())
}

fn password_ok(password: &[u8]) -> bool {
    (1..=64).contains(&password.len())
        && password
            .iter()
            .all(|b| b.is_ascii_graphic() || *b == b' ')
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
    table.actors[ai] = Actor::empty();
    Ok(())
}

/// True when `cred` holds every right on `object` (exact or ancestor token).
pub fn holds_all(root: u16, tokens: &[Token; TOKEN_SLOTS], object: u16) -> bool {
    let table = TABLE.lock();
    let cred = cred_from_tokens(root, tokens);
    token_allows(&table, &cred, object, RIGHT_ALL)
}

/// Adds an actor and an empty root. Test and boot only.
pub fn add_actor_named(name: &str, password: &[u8]) -> Result<u16, SysError> {
    if !password_ok(password) {
        return Err(SysError::BadValue);
    }
    let mut table = TABLE.lock();
    add_actor(&mut table, name, password)
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
    let actor = &mut table.actors[ai];
    actor.used = true;
    actor.name = [0; ACTOR_NAME];
    actor.name[..name.len()].copy_from_slice(name.as_bytes());
    actor.name_len = name.len() as u8;
    actor.root = oi as u16;
    actor.set_password(password);
    let obj = &mut table.objects[oi];
    *obj = Object::empty();
    obj.kind = KIND_DIR;
    obj.parent = NO_PARENT;
    obj.actor = ai as u8;
    // Root has an empty name; paths start at its children.
    Ok(oi as u16)
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
    sync();
    Ok(oi)
}

fn mkdir_locked(table: &mut Table, root: u16, name: &str) -> Result<u16, SysError> {
    if root as usize >= OBJECT_SLOTS || table.objects[root as usize].kind != KIND_DIR {
        return Err(SysError::NotFound);
    }
    if find_child(table, root, name).is_some() {
        return Err(SysError::Unsupported);
    }
    let Some(oi) = free_object(table) else {
        return Err(SysError::NoResource);
    };
    let actor = table.objects[root as usize].actor;
    let obj = &mut table.objects[oi];
    obj.kind = KIND_DIR;
    obj.parent = root;
    obj.actor = actor;
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
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let actor = table.objects[parent as usize].actor;
    let obj = &mut table.objects[oi];
    obj.kind = KIND_FILE;
    obj.parent = parent;
    obj.actor = actor;
    place_name(obj, name);
    obj.data = [0; FILE_BYTES];
    obj.len = 0;
    drop(table);
    sync();
    Ok(oi as u16)
}

fn free_object(table: &Table) -> Option<usize> {
    table.objects.iter().position(|o| o.kind == KIND_EMPTY)
}

fn component_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= NAME_CAP
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

fn place_name(obj: &mut Object, name: &str) {
    obj.name = [0; NAME_CAP];
    obj.name[..name.len()].copy_from_slice(name.as_bytes());
    obj.name_len = name.len() as u8;
}

/// A path after the optional leading `/` and `owner@` on the first component.
pub(crate) struct ParsedPath<'a> {
    pub(crate) owner: Option<&'a str>,
    pub(crate) comps: [&'a str; MAX_DEPTH],
    pub(crate) n: usize,
    pub(crate) dir: bool,
}

/// Splits `name`. A trailing slash marks a directory. The first component
/// may be `owner@leaf`. `owner@/` (empty leaf, directory) names that
/// actor's root object — the login/grant path. `.` and `..` are rejected.
pub(crate) fn parse_path(name: &str) -> Result<ParsedPath<'_>, SysError> {
    let name = name.strip_prefix('/').unwrap_or(name);
    let (body, dir) = if let Some(stripped) = name.strip_suffix('/') {
        if stripped.is_empty() || stripped.ends_with('/') {
            return Err(SysError::BadValue);
        }
        (stripped, true)
    } else {
        (name, false)
    };
    if body.is_empty() {
        return Err(SysError::BadValue);
    }
    let mut comps = [""; MAX_DEPTH];
    let mut n = 0usize;
    let mut owner = None;
    for (i, comp) in body.split('/').enumerate() {
        if comp.is_empty() || n >= MAX_DEPTH {
            return Err(SysError::BadValue);
        }
        if i == 0 {
            if let Some((own, leaf)) = comp.split_once('@') {
                if own.is_empty() || leaf.contains('@') || !component_ok(own) {
                    return Err(SysError::BadValue);
                }
                // `eve@/` → actor root (no components under the root).
                if leaf.is_empty() {
                    if !dir || body.contains('/') {
                        return Err(SysError::BadValue);
                    }
                    owner = Some(own);
                    break;
                }
                if !component_ok(leaf) {
                    return Err(SysError::BadValue);
                }
                owner = Some(own);
                comps[n] = leaf;
                n += 1;
                continue;
            }
        } else if comp.contains('@') {
            return Err(SysError::BadValue);
        }
        if !component_ok(comp) {
            return Err(SysError::BadValue);
        }
        comps[n] = comp;
        n += 1;
    }
    if n == 0 && owner.is_none() {
        return Err(SysError::BadValue);
    }
    Ok(ParsedPath {
        owner,
        comps,
        n,
        dir,
    })
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
fn token_allows(table: &Table, cred: &FsCred, object: u16, need: u8) -> bool {
    // Logged-in admin may mint and use any card (operator seat).
    if need != 0 && is_admin_root(cred.root) {
        return true;
    }
    for token in &cred.tokens {
        if !token.is_live() || token.rights & need != need {
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
            table.objects[found].data = [0; FILE_BYTES];
            table.objects[found].len = 0;
            return Ok(Some(index));
        }
        return Err(SysError::Unsupported);
    }
    let Some(oi) = free_object(&table) else {
        return Err(SysError::NoResource);
    };
    let actor = table.objects[parent as usize].actor;
    let obj = &mut table.objects[oi];
    obj.kind = if parsed.dir { KIND_DIR } else { KIND_FILE };
    obj.parent = parent;
    obj.actor = actor;
    place_name(obj, last);
    obj.data = [0; FILE_BYTES];
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
    table.objects[found] = Object::empty();
    Ok(index)
}

/// Reads file bytes under the global lock.
pub(crate) fn with_file<R>(index: u16, f: impl FnOnce(&Object) -> R) -> Option<R> {
    let table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(f(&table.objects[i]))
}

/// Writes file bytes under the global lock.
pub(crate) fn with_file_mut<R>(index: u16, f: impl FnOnce(&mut Object) -> R) -> Option<R> {
    let mut table = TABLE.lock();
    let i = index as usize;
    if i >= OBJECT_SLOTS || table.objects[i].kind != KIND_FILE {
        return None;
    }
    Some(f(&mut table.objects[i]))
}

/// Appends `src` to a file. Returns the byte count written.
pub(crate) fn append(index: u16, src: &[u8]) -> Option<usize> {
    with_file_mut(index, |stored| {
        let start = stored.len as usize;
        let n = src.len().min(FILE_BYTES.saturating_sub(start));
        stored.data[start..start + n].copy_from_slice(&src[..n]);
        stored.len = (start + n) as u16;
        n
    })
}

/// Appends bytes to a file by object index. Test helper.
pub fn append_file(index: u16, src: &[u8]) -> Option<usize> {
    let n = append(index, src)?;
    if n > 0 {
        sync();
    }
    Some(n)
}

/// Copies file bytes into `out`. Returns the length, or `None` if missing.
pub fn read_file_bytes(index: u16, out: &mut [u8]) -> Option<usize> {
    with_file(index, |stored| {
        let n = (stored.len as usize).min(out.len());
        out[..n].copy_from_slice(&stored.data[..n]);
        n
    })
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
