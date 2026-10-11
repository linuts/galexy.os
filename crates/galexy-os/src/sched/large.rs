//! Files that do not fit the v12 pool.
//!
//! v12 keeps 512-byte blocks and a `u16` length (32 KiB). A write past
//! that promotes the file here: 4 KiB blocks, one indirect block per
//! 4 MiB, length `u32`. The pool grows on the kernel heap up to
//! [`LARGE_POOL_BYTES`] of file data, plus indirect blocks. One file
//! is still capped at [`LARGE_FILE_MAX`]. Callers hold the galfs table
//! lock and then [`STORE`]; this module never locks the table.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crate::sync::Mutex;

/// Block size for a promoted file.
pub const LARGE_BLOCK: usize = 4096;
/// Largest file this store will hold.
///
/// Milestone 71 required 64 MiB. `libcore`'s split metadata is about
/// 66 MiB, so one file needs 80 MiB. The on-disk v13 cap stays 64 MiB;
/// this store is RAM only.
pub const LARGE_FILE_MAX: u32 = 80 * 1024 * 1024;
/// Bytes the pool will hold across every promoted file.
///
/// A `std` compile keeps `core`'s metadata (~66 MiB) plus `alloc`,
/// `std`, and the rlibs the linker reads. That set is about 90 MiB.
/// 128 MiB leaves room for the linked ELF and rustc's temps.
pub const LARGE_POOL_BYTES: usize = 128 * 1024 * 1024;
/// Object slots. Matches `galfs::OBJECT_SLOTS`.
pub const SLOTS: usize = 128;

const PTRS: usize = LARGE_BLOCK / 4;
const NONE: u32 = u32::MAX;
/// Data blocks for [`LARGE_POOL_BYTES`], plus indirect blocks.
const POOL_CAP: usize = (LARGE_POOL_BYTES / LARGE_BLOCK) + 128;

struct File {
    len: u32,
    indirect: Vec<u32>,
}

struct Store {
    blocks: Vec<Option<Box<[u8; LARGE_BLOCK]>>>,
    free: Vec<u32>,
    files: Vec<Option<File>>,
}

static STORE: Mutex<Store> = Mutex::new(Store {
    blocks: Vec::new(),
    free: Vec::new(),
    files: Vec::new(),
});

fn zero_block() -> Box<[u8; LARGE_BLOCK]> {
    vec![0u8; LARGE_BLOCK]
        .into_boxed_slice()
        .try_into()
        .unwrap_or_else(|_| unreachable!("4096-byte block"))
}

fn ensure_slots(store: &mut Store) {
    if store.files.len() < SLOTS {
        store.files.resize_with(SLOTS, || None);
    }
}

fn alloc_block(store: &mut Store) -> Option<u32> {
    if let Some(i) = store.free.pop() {
        store.blocks[i as usize] = Some(zero_block());
        return Some(i);
    }
    if store.blocks.len() >= POOL_CAP {
        return None;
    }
    store.blocks.push(Some(zero_block()));
    Some((store.blocks.len() - 1) as u32)
}

fn fill_none(store: &mut Store, id: u32) {
    let Some(block) = store.blocks[id as usize].as_mut() else {
        return;
    };
    let (chunks, _) = block.as_chunks_mut::<4>();
    for chunk in chunks {
        *chunk = NONE.to_le_bytes();
    }
}

fn read_ptr(block: &[u8], slot: usize) -> u32 {
    let off = slot * 4;
    u32::from_le_bytes(block[off..off + 4].try_into().unwrap_or([0; 4]))
}

fn ensure_indirect(store: &mut Store, index: usize, ind: usize) -> Option<u32> {
    ensure_slots(store);
    while store.files[index].as_ref()?.indirect.len() <= ind {
        let b = alloc_block(store)?;
        fill_none(store, b);
        store.files[index].as_mut()?.indirect.push(b);
    }
    Some(store.files[index].as_ref()?.indirect[ind])
}

fn ensure_data(store: &mut Store, index: usize, data_index: usize) -> Option<u32> {
    let ind = data_index / PTRS;
    let slot = data_index % PTRS;
    let ib = ensure_indirect(store, index, ind)?;
    let existing = store.blocks[ib as usize]
        .as_ref()
        .map(|b| read_ptr(&b[..], slot))
        .unwrap_or(NONE);
    if existing != NONE {
        return Some(existing);
    }
    let data = alloc_block(store)?;
    let block = store.blocks[ib as usize].as_mut()?;
    let off = slot * 4;
    block[off..off + 4].copy_from_slice(&data.to_le_bytes());
    Some(data)
}

/// `Some(len)` when `index` has been promoted.
pub fn len(index: usize) -> Option<u32> {
    let mut store = STORE.lock();
    ensure_slots(&mut store);
    store
        .files
        .get(index)
        .and_then(|f| f.as_ref())
        .map(|f| f.len)
}

/// Copies `bytes` into a fresh promoted file. Replaces any previous
/// large body for `index`.
pub fn create_from(index: usize, bytes: &[u8]) -> bool {
    if index >= SLOTS || bytes.len() > LARGE_FILE_MAX as usize {
        return false;
    }
    clear(index);
    {
        let mut store = STORE.lock();
        ensure_slots(&mut store);
        store.files[index] = Some(File {
            len: 0,
            indirect: Vec::new(),
        });
    }
    append(index, bytes, bytes.len() as u32) == bytes.len()
}

/// Appends `src`, stopping after `max_new` new bytes or at
/// [`LARGE_FILE_MAX`]. The file must already be promoted.
pub fn append(index: usize, src: &[u8], max_new: u32) -> usize {
    let mut store = STORE.lock();
    ensure_slots(&mut store);
    if index >= SLOTS || store.files[index].is_none() {
        return 0;
    }
    let len0 = store.files[index].as_ref().map(|f| f.len).unwrap_or(0);
    let mut written = 0usize;
    while written < src.len() {
        let pos = store.files[index].as_ref().map(|f| f.len).unwrap_or(0);
        if pos >= LARGE_FILE_MAX {
            break;
        }
        let added = pos.saturating_sub(len0);
        if added >= max_new {
            break;
        }
        let room_quota = (max_new - added) as usize;
        let room_file = (LARGE_FILE_MAX - pos) as usize;
        let data_index = pos as usize / LARGE_BLOCK;
        let off = pos as usize % LARGE_BLOCK;
        let Some(bi) = ensure_data(&mut store, index, data_index) else {
            break;
        };
        let room = (LARGE_BLOCK - off)
            .min(src.len() - written)
            .min(room_quota)
            .min(room_file);
        if room == 0 {
            break;
        }
        let block = store.blocks[bi as usize].as_mut().unwrap();
        block[off..off + room].copy_from_slice(&src[written..written + room]);
        store.files[index].as_mut().unwrap().len = pos + room as u32;
        written += room;
    }
    written
}

/// Overwrites `[start, start+src)` and zero-fills a gap past the end.
/// `max_new` is how many bytes past the current length the actor may
/// still own. Returns bytes of `src` copied (not counting the gap).
pub fn write_at(index: usize, start: usize, src: &[u8], max_new: u32) -> usize {
    if src.is_empty() {
        return 0;
    }
    let mut store = STORE.lock();
    ensure_slots(&mut store);
    if index >= SLOTS || store.files[index].is_none() {
        return 0;
    }
    let mut budget = max_new;
    let len = store.files[index].as_ref().unwrap().len as usize;
    if start > len {
        let gap = start - len;
        if (gap as u32) > budget || !extend_zeros(&mut store, index, gap, &mut budget) {
            return 0;
        }
    }
    let len = store.files[index].as_ref().unwrap().len as usize;
    let mut done = 0usize;
    if start < len {
        let overwrite = src.len().min(len - start);
        while done < overwrite {
            let pos = start + done;
            let data_index = pos / LARGE_BLOCK;
            let off = pos % LARGE_BLOCK;
            let Some(bi) = ensure_data(&mut store, index, data_index) else {
                return done;
            };
            let n = (LARGE_BLOCK - off).min(overwrite - done);
            let block = store.blocks[bi as usize].as_mut().unwrap();
            block[off..off + n].copy_from_slice(&src[done..done + n]);
            done += n;
        }
    }
    while done < src.len() {
        let pos = store.files[index].as_ref().unwrap().len;
        if pos >= LARGE_FILE_MAX || budget == 0 {
            break;
        }
        let data_index = pos as usize / LARGE_BLOCK;
        let off = pos as usize % LARGE_BLOCK;
        let Some(bi) = ensure_data(&mut store, index, data_index) else {
            break;
        };
        let room = (LARGE_BLOCK - off)
            .min(src.len() - done)
            .min(budget as usize)
            .min((LARGE_FILE_MAX - pos) as usize);
        if room == 0 {
            break;
        }
        let block = store.blocks[bi as usize].as_mut().unwrap();
        block[off..off + room].copy_from_slice(&src[done..done + room]);
        store.files[index].as_mut().unwrap().len = pos + room as u32;
        budget -= room as u32;
        done += room;
    }
    done
}

fn extend_zeros(store: &mut Store, index: usize, mut gap: usize, budget: &mut u32) -> bool {
    let zeros = [0u8; 256];
    while gap > 0 {
        if *budget == 0 {
            return false;
        }
        let pos = store.files[index].as_ref().unwrap().len;
        if pos >= LARGE_FILE_MAX {
            return false;
        }
        let data_index = pos as usize / LARGE_BLOCK;
        let off = pos as usize % LARGE_BLOCK;
        let Some(bi) = ensure_data(store, index, data_index) else {
            return false;
        };
        let room = (LARGE_BLOCK - off)
            .min(gap)
            .min(*budget as usize)
            .min(zeros.len())
            .min((LARGE_FILE_MAX - pos) as usize);
        if room == 0 {
            return false;
        }
        let block = store.blocks[bi as usize].as_mut().unwrap();
        block[off..off + room].copy_from_slice(&zeros[..room]);
        store.files[index].as_mut().unwrap().len = pos + room as u32;
        *budget -= room as u32;
        gap -= room;
    }
    true
}

/// Copies `dst.len()` bytes from `start`. `None` when `index` is not large.
pub fn read(index: usize, start: usize, dst: &mut [u8]) -> Option<usize> {
    let store = STORE.lock();
    let file = store.files.get(index)?.as_ref()?;
    let len = file.len as usize;
    if start >= len || dst.is_empty() {
        return Some(0);
    }
    let want = dst.len().min(len - start);
    let mut copied = 0usize;
    while copied < want {
        let pos = start + copied;
        let data_index = pos / LARGE_BLOCK;
        let off = pos % LARGE_BLOCK;
        let ind = data_index / PTRS;
        let slot = data_index % PTRS;
        let ib = *file.indirect.get(ind)?;
        let block = store.blocks.get(ib as usize)?.as_ref()?;
        let id = read_ptr(&block[..], slot);
        if id == NONE {
            return None;
        }
        let data = store.blocks.get(id as usize)?.as_ref()?;
        let n = (LARGE_BLOCK - off).min(want - copied);
        dst[copied..copied + n].copy_from_slice(&data[off..off + n]);
        copied += n;
    }
    Some(copied)
}

/// Shrinks a promoted file. Growing is [`append`] of zeros.
pub fn truncate_to(index: usize, new_len: u32) {
    let mut store = STORE.lock();
    ensure_slots(&mut store);
    let Some(file) = store.files[index].as_mut() else {
        return;
    };
    if new_len >= file.len {
        return;
    }
    file.len = new_len;
    let need = if new_len == 0 {
        0
    } else {
        (new_len as usize).div_ceil(LARGE_BLOCK)
    };
    let ind_need = need.div_ceil(PTRS);
    let indirect = core::mem::take(&mut store.files[index].as_mut().unwrap().indirect);
    let mut keep = Vec::new();
    for (di, ib) in indirect.into_iter().enumerate() {
        if di >= ind_need {
            free_tree(&mut store, ib, true);
            continue;
        }
        let first = di * PTRS;
        let ptrs: Vec<u32> = store.blocks[ib as usize]
            .as_ref()
            .map(|block| (0..PTRS).map(|slot| read_ptr(&block[..], slot)).collect())
            .unwrap_or_default();
        let mut drop_slots = Vec::new();
        for (slot, id) in ptrs.iter().copied().enumerate() {
            if first + slot >= need {
                if id != NONE {
                    free_id(&mut store, id);
                    drop_slots.push(slot);
                }
            } else if !(new_len as usize).is_multiple_of(LARGE_BLOCK)
                && first + slot + 1 == need
                && id != NONE
            {
                let off = (new_len as usize) % LARGE_BLOCK;
                if let Some(data) = store.blocks[id as usize].as_mut() {
                    data[off..].fill(0);
                }
            }
        }
        if let Some(block) = store.blocks[ib as usize].as_mut() {
            for slot in drop_slots {
                let off = slot * 4;
                block[off..off + 4].copy_from_slice(&NONE.to_le_bytes());
            }
        }
        keep.push(ib);
    }
    store.files[index].as_mut().unwrap().indirect = keep;
}

fn free_id(store: &mut Store, id: u32) {
    let i = id as usize;
    if i >= store.blocks.len() {
        return;
    }
    if store.blocks[i].take().is_some() {
        store.free.push(id);
    }
}

fn free_tree(store: &mut Store, ib: u32, indirect: bool) {
    if !indirect {
        free_id(store, ib);
        return;
    }
    let ptrs: Vec<u32> = store
        .blocks
        .get(ib as usize)
        .and_then(|b| b.as_ref())
        .map(|block| {
            (0..PTRS)
                .map(|slot| read_ptr(&block[..], slot))
                .filter(|id| *id != NONE)
                .collect()
        })
        .unwrap_or_default();
    for id in ptrs {
        free_id(store, id);
    }
    free_id(store, ib);
}

/// Drops a promoted file and returns its blocks to the pool.
pub fn clear(index: usize) {
    let mut store = STORE.lock();
    ensure_slots(&mut store);
    if index >= store.files.len() {
        return;
    }
    let Some(file) = store.files[index].take() else {
        return;
    };
    for ib in file.indirect {
        free_tree(&mut store, ib, true);
    }
}

/// Structural check of the large pool (called from galfs `validate_table`).
pub fn ok() -> bool {
    let store = STORE.lock();
    let mut seen = vec![false; store.blocks.len()];
    for file in &store.files {
        let Some(file) = file else {
            continue;
        };
        if file.len > LARGE_FILE_MAX {
            return false;
        }
        let need = if file.len == 0 {
            0
        } else {
            (file.len as usize).div_ceil(LARGE_BLOCK)
        };
        let ind_need = need.div_ceil(PTRS);
        if file.indirect.len() < ind_need {
            return false;
        }
        for (di, &ib) in file.indirect.iter().enumerate() {
            if ib as usize >= store.blocks.len() || seen[ib as usize] {
                return false;
            }
            seen[ib as usize] = true;
            let Some(block) = store.blocks[ib as usize].as_ref() else {
                return false;
            };
            for slot in 0..PTRS {
                let id = read_ptr(&block[..], slot);
                let data_index = di * PTRS + slot;
                if data_index < need {
                    if id == NONE || id as usize >= store.blocks.len() || seen[id as usize] {
                        return false;
                    }
                    if store.blocks[id as usize].is_none() {
                        return false;
                    }
                    seen[id as usize] = true;
                } else if id != NONE && di < ind_need {
                    return false;
                }
            }
        }
    }
    true
}
