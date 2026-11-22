# GALFS — actors, objects, tokens, and the sealed disk

galexy.os stores user data in **galfs**: a small, capability-gated
filesystem. Paths are for lookup; **tokens** are for authority.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Namespace | Actor root + `owner@` paths | Which tree is this name in? |
| Authority | Token (`object` + `RIGHT_*`) | May this task list / read / mutate it? |
| Durability | Dual-slot sealed GALF image | Does the table survive reboot / crash? |

This document is the model (same role as `PROCESS.md` for Caps/init).
Checkboxes live in `TODO.md` (Milestones **44–46**). Style rules:
`docs/STYLE.md` → Capabilities and tokens, On-disk formats. Passwords
and login sessions: `docs/AUTH.md`. Process wait/kill Caps:
`docs/PROCESS.md` — orthogonal to galfs cards.

## Why not a POSIX VFS

Unix paths + uid/gid + mode bits authorize by ambient identity. That
fights Caps elsewhere in Galexy (devices, future process Caps).

We keep:

- **Tokens as access cards** — rights on an object id, not a path string
- **Actors as homes** — each account owns one root (+ `Desktop/`)
- **Spawn attenuation** — bare programs inherit empty tokens
- **Explicit share** — `grant` / `revoke` move cards between live tasks;
  `share` / `unshare` record durable home shares re-applied at login

We do **not** claim POSIX, POSIX ACLs, or a full VFS stack. galfs is the
Galexy ABI for user trees.

## Pieces

| Piece | Meaning |
| --- | --- |
| Actor | Named account (`admin`, `eve`, …) with one root directory |
| Object | File or directory slot in the fixed table (or empty) |
| Root | Actor’s top directory; path walks start here unless `owner@` |
| Token | `(object, RIGHT_*)` on a task — the only way to touch galfs |
| `fs_root` | Session default root for paths without an owner prefix |
| Path | Lookup string (`/Desktop/notes`, `/eve@Desktop/x`); not authority |
| GALF | On-disk dual-slot image of the actor/object tables |
| Volume key | Random key that encrypts a sealed slot’s payload (v6+) |

## Rights

Exact bits live in `sched/galfs.rs` / the abi. Intent:

| Right | Allows |
| --- | --- |
| `RIGHT_READ` | Read file bytes |
| `RIGHT_WRITE` | Append / overwrite file bytes |
| `RIGHT_LIST` | List directory children |
| `RIGHT_CREATE` | Create a child under a directory |
| `RIGHT_REMOVE` | Remove a child |
| `RIGHT_ALL` | Union of the above |

Holding a token on an **ancestor** covers descendants for checks that
walk up. `grant` may only install rights the caller already holds on
that object (or an ancestor). `revoke` clears an exact-object token
slot.

**Paths do not grant rights.** Parsing `/eve@/` finds eve’s root; you
still need a card. Admin `su <actor>` installs `ALL` on that root;
admin’s own root token does not cover foreign trees.

## Paths

```text
/Desktop/notes          under the task's fs_root
/eve@Desktop/notes      under actor eve's root
/eve@/                  eve's root itself (login-card shape)
```

| Rule | Behavior |
| --- | --- |
| No `owner@` | Walk from the task’s `fs_root` |
| `owner@…` | Resolve actor by name, then walk from that root |
| Depth | At most `MAX_DEPTH` (**8**) components after parse |
| Component charset | ASCII alphanumeric plus `.` `_` `-` only |
| `.` / `..` | Rejected (`BadValue`) — no walk-up |
| Length | Component ≤ 64 bytes; empty / `//` rejected |
| Trailing `/` | Marks a directory path (`Desktop/`, `eve@/`) |
| Archive names | Ramdisk hits first (READ); else galfs |

Names are **bytes**, not Unicode scalars: non-ASCII and embedded NUL
are `BadValue`. Shell cwd is userspace-only; the kernel always sees
absolute or owner-qualified components from the syscall args.
`bin/test-paths` covers the negative cases.

## Tokens on a task

Each user task holds a small fixed table of tokens (`TOKEN_SLOTS` = **8**).
A full table is `NoResource`; `revoke` / `revoke_token` frees a slot.
Same-object grants merge rights into one slot.

```text
grant lr /Desktop notes     # list+read on Desktop → task "notes"
grant a /eve@/ shell2       # ALL on eve's root (login card)
revoke a /eve@/ shell2
share lr /Desktop eve       # durable; re-applied at eve's login
unshare lr /Desktop eve
```

Live `grant`/`revoke` target a **task name**. Durable `share`/`unshare`
target an **actor name** (`SHARE_SLOTS` = **32**). Both require the
caller to already hold every right being granted/shared
(`resolve_and_check` — confused-deputy bar). LIST-only cannot mint
WRITE; a path without a covering card is `AccessDenied`.

| Spawn kind | galfs credentials (today) |
| --- | --- |
| Utility (`SPAWN_WAIT`) | Inherits parent’s session tokens |
| Bare program | Parent’s `fs_root`, **empty** tokens |
| Pre-login seat | No `fs_root`; `spawn` is `AccessDenied` by kernel rule, and the seat holds no loader Cap |

Password `login` replaces tokens with `ALL` on the actor’s root, then
applies durable home shares. `logout` clears tokens and `fs_root`.
Card-based `su` installs `ALL` on the target when the caller already
holds that card (see `AUTH.md`).

Holding a **process Cap** to a child never grants galfs rights on that
child’s files (`PROCESS.md`).

`bin/test-cards` covers token/share slot exhaustion and the
confused-deputy share rules.

## Table limits (today)

The on-disk version is GALF **v17**. The sealed slot holds actors,
shares, quotas, and geometry. Inodes live in the data pool, named by
up to 128 map blocks. A directory is the sibling list hung off its
inode (cards still walk the parent pointer). Empty files cost an
inode only. File bytes are 4 KiB extents. A small cache holds the
blocks a read or write is using; the rest stay on the device.

| Resource | Cap |
| --- | --- |
| Actors | 32 |
| Names (files + dirs + roots) | 1,048,576 format ceiling. One desk compile names 41 (`DESK_COMPILE_FILES`: 33 sysroot entries, 4 directories, admin root, Desktop, `hello.rs`, and `/hello`). A live disk uses at most a quarter of its pool for inode and map blocks, and never more than the ceiling. An 8 MiB image holds a few thousand names, which covers that 41 |
| Durable home shares | 32 |
| Block size | 4096 bytes |
| Extents per file | 8. One sequential write in a commit is one extent |
| Max durable file | the rest of the pool after inode and map blocks, capped at 64 GiB |
| Block pool | sized to the disk |
| Default user quota | 16 objects / 16 KiB |
| Tokens per task | 8 |
| Open files per task | 8 (IF=0: no heap on `open`) |
| Path depth | 8 components |
| Name length | 64 (object) / 32 (actor) |
| Metadata slot | 64 KiB (no inodes) |
| Data region | 4 KiB blocks after the two slots (tag sector + 8 data sectors each) |

An 8 MiB test image holds a few thousand names and about 5 MiB of
file data. A 32 GiB disk clears 1,000,000 names and a file over
16 GiB. 1,048,576 is the format ceiling. One desk compile names
**41** files and directories: the `rustc-sysroot: 33 entries` import,
the four directories that import creates, the admin root, `Desktop`,
`hello.rs`, and the `/hello` that `rustc hello.rs -o /hello` writes.
The sealed geometry stores that count. `DISK_VERSION` is 17, so a v16
image is refused and the operator deletes `galfs.img`.

The IF=0 syscall path must not heap-allocate over this table. A file
that needs a ninth extent is `NoResource`, which is what happens when
the same file grows across more than eight commits. A write that does
not fit the live file cap, and still fits the 80 MiB RAM store,
promotes into that store. On the 8 MiB image that includes `core`'s
metadata (about 66 MiB). The 16-byte quota record saturates byte
counts above 4 GiB; the stored quota is a `u64`.

## On-disk: GALF slots

When the primary IDE slave is present, the table is durable. Without a
slave it stays RAM-only.

```text
slot 0 @ LBA disk_lba_base()          (default 0; tests may use 2048)
slot 1 @ LBA disk_lba_base() + DISK_SECTORS
data   @ LBA disk_lba_base() + 2 × DISK_SECTORS
        each block is BLOCK_STRIDE sectors: one tag sector, then 8 ciphertext
```

Call `set_disk_lba_base` before `galfs::init` when the volume should not
own absolute LBA 0 (e.g. after a protective MBR / GPT). The device must
cover `base` plus `MIN_DISK_BYTES` (8 MiB). The pool is whatever 4 KiB
blocks fit after the two slots. Inode and map blocks take at most a
quarter of it. File bytes take the rest, up to 64 GiB.

| Property | Behavior |
| --- | --- |
| Dual slot | Commit writes changed data blocks, then the **inactive** metadata slot, then flush |
| Generation | Newer gen wins on load |
| Crash | A torn slot leaves the previous metadata, which still names its own blocks |
| Version | Layout bump **refuses** old images (no silent reinterpret). v17 refuses v12 through v16 |

### Commit ordering and flush

Mutates update the in-RAM table and mark it **dirty**. A coalesced
commit (write-back) seals changed pool blocks into the data region
(ChaCha20-HMAC, nonce = domain ‖ block index ‖ allocation generation),
including the inode and map blocks that hold names and directory
links, then encodes the sealed metadata slot (actors, shares,
geometry, and the map-block pointers — not the file bytes, the
bitmap, or per-block tags) into the inactive LBA range, then
`BlockDevice::flush` **before**
publishing the new generation in RAM. A block named by committed
metadata is copied to a fresh index before it is overwritten. The old
index stays reserved until both slots have dropped it, so falling back
to the previous slot still opens the ciphertext it names. Commits run on:

- the kernel main loop's 1 Hz tick (`sync_if_dirty`)
- `Syscall::Sync` / shell `sync`
- power-off / reboot
- explicit `galfs::sync()` in tests

Crash window: up to about one second of unflushed mutates (plus any
work after the last tick). Data blocks are written before the metadata
that names them. A torn metadata slot fails AEAD and the previous slot
remains the volume. New blocks the torn slot would have named are
orphans; blocks the previous slot names were not overwritten.

Flush matrix (runner): `boot_with_galfs` uses `cache=writethrough`;
`galfs_disk_persists_writeback_cache` / `_none_cache` repeat the
persistence e2e under `cache=writeback` and `cache=none` so the guest
barrier is not an accidental host-cache artifact.

### Sealed slots (v6…v8 blocks → v9 quotas → v10 shares → v11 indirect)

**Threat (v1):** stolen `galfs.img` must not yield file bytes or password
hashes offline. Cold-boot RAM and a live compromised kernel are out of
scope initially.

1. Format creates a random **volume key**.
2. KEK = PBKDF2(volume passphrase); wrap the volume key (ChaCha20-HMAC).
3. Encrypt the actor/object/share payload under the volume key
   (each file inode carries up to eight extents and its directory links).
   File bytes are sealed per 4 KiB block in the data region. The block's
   generation and AEAD tag sit in that block's private tag sector, not in
   the metadata payload.
4. AAD binds magic + version + generation (slot splice rejected).
5. CRC of ciphertext is a cheap reject before AEAD open.

Test kernels auto-unlock with the fixed passphrase `galfs`. Production
boot leaves the volume locked. An empty image asks the operator to
choose a passphrase and confirm it; there is no passphrase to type
until that choice. A sealed image prompts `Volume passphrase:` and
does not show `Login as:` until the volume opens. An empty line asks
again. Admin changes it later with `volpass`, which re-wraps the
volume key. A wrong passphrase refuses disk sync and keeps the RAM
table (not a corrupt image). Login is refused until unlock. The last
logout and power-off zero the volume key in RAM; cold-boot remanence
is accepted. The MAC stays
HMAC-SHA256 (Poly1305 needs a versioned cutover). Per-file keys,
secure erase, and TPM seal are non-goals. Details: `AUTH.md` → Sealed GALF.

v15 refuses older images (including v14, v13, and v12); delete `galfs.img` or let
format recreate. The actor record carries `kdf_iters` (4 bytes) after
the password hash. Directory membership is a sibling list on the inode;
a card still names an object id and covers descendants through `parent`.

## Boot and format

1. If a `BlockDevice` with capacity ≥ dual-slot image is present and
   auto-unlock is on → try load newest valid sealed slot with `galfs`.
   Production boot (auto-unlock off) builds a RAM admin table and waits
   for `USER_UNLOCK`. No GALF header means the seat chooses the
   passphrase (`volume_query` returns 2). A header means the existing
   passphrase (`volume_query` returns 1).
2. Empty zeros (no GALF magic) → format: immortal `admin` + `Desktop/`,
   new volume key, sync sealed image. Test kernels and a diskless
   boot still use password `admin`. An interactive fresh disk takes
   the admin password in the same `USER_UNLOCK` and seals that hash
   instead. `USER_VOLPASS`
   re-wraps an unlocked volume.
3. Both slots carry GALF magic but fail decode/validate → **refuse
   silent format**; galfs stays unavailable (`Unsupported` on sync /
   mutates that need a mount). Serial: `disk corrupt; refusing silent
   format`.
4. Mutates (`create` / `remove` / `append` / `rename` / `truncate` /
   `useradd` / `userdel` / `passwd`, …) mark the table dirty when
   disk-backed; the 1 Hz tick / `Syscall::Sync` / power-off commit the
   inactive slot + flush.
5. `userdel` refuses `admin`, non-empty trees, and roots still in use;
   clears tokens and durable shares that named that actor’s objects.

Boot serial names the winning slot and generation; a bad newer sibling
logs `(recovered from bad sibling)`.

`bin/test-galfs` exercises tokens in RAM. `bin/test-blocks` fills the
block pool and reuses after remove. `bin/test-galfs-disk` proves a
multi-block file and a 1 MiB file persist across reboot; the host
asserts plaintext markers are absent from the raw image.
`bin/test-galfs-corrupt` boots a both-bad image and refuses format.
`bin/test-fsck` checks the live table after write/truncate/remove,
including a ten-block extent file. `bin/test-shares` records a durable
share, proves login re-apply, unshare, and `userdel` cleanup.
`bin/test-indirect` writes a two-block extent file, truncates, grows
it back, and fills eight more blocks. Shell `tokens` lists the task’s
cards (`USER_TOKENS`); `share` / `unshare` manage durable home shares.

## Auth interaction

| Concern | Doc |
| --- | --- |
| Password login / actors / passwd | `AUTH.md` |
| Token grant / revoke / su cards | `AUTH.md` + **this doc** |
| Sealed volume / stolen disk | `AUTH.md` + **this doc** |
| Who may wait/kill a task | `PROCESS.md` |
| Syscall wiring / sizes | `DESIGN.md` |

## Evolution

### Today (through review readiness)

- GALF **v17**: 32 actors / up to 1,048,576 names / 32 durable shares.
  The geometry records that one desk compile names 41 objects. A v16
  image is refused. Per-actor `kdf_iters`; 4 KiB extents (eight per file) in a pool sized
  to the disk. Inodes are pool blocks, not slot bytes. File length is
  a `u64`, capped at 64 GiB and at whatever the disk has left after
  inode and map blocks. Directory sibling lists; per-actor object and
  byte quotas (defaults for new users; admin at least the 128 MiB RAM
  pool on a fresh table, or the stored quota on a loaded disk). The
  compiler sysroot import raises a stored admin quota up to the live
  inode count and that RAM pool, and does not lower a higher one. Its
  rlib and rmeta bodies stay the ramdisk mapping until a write copies
  them
- **A write past the live file cap** promotes into the RAM store when
  it fits in 80 MiB (`core`'s metadata is about 66 MiB, so the 8 MiB
  test image still promotes it). Promoted bytes are not sealed. Host
  **v13** (`galexy-galf::v13`) remains the 32-file extent-map checker.
  `check_image` dispatches on the version word. The kernel speaks v17
  and refuses every other version, including v13 through v16. v13
  names 32 files and does not carry actors, shares, or quotas, so it
  is not the login volume
- Sealed dual-slot image (64 KiB per slot) on virtio-blk, else the IDE slave
- Object + block fill stress; remove reuses blocks without leaks
- Shell: `ls` / `cat` / `echo` / `touch` / `mkdir` / `rm` / `cp` / `mv`
  / `stat` / `truncate` / `grant` / `revoke` / `share` / `unshare` /
  `quota` / `tokens` / `sync`; `echo text | cat` and `*` glob (M50)
- Admin operator bypass is narrow (Milestone 43): admin's root token
  does not cover foreign trees; `su` or a card is required

### Ops (landed)

- `rename` / `truncate` / `stat` syscalls; shell `mv` / `truncate` / `stat`
- Directory listing stays the `FILES` snapshot (`ls`); no dir `open`
- `write` is positional at the file cursor (overwrite, then append);
  `seek` moves that cursor for both `read` and `write` (Milestone 70)
- No hard links, symlinks, or sparse holes (truncate grow zero-fills)
- Names: ASCII alphanumeric plus `.` `_` `-`; `.` / `..` rejected

### Durability (landed)

- Write-back: mutates dirty the table; 1 Hz / `sync` / power commit the
  inactive dual slot + flush
- Boot logs slot/gen; recovery from a bad sibling is explicit
- Both-bad GALF magic → no silent format; volume stays unavailable
- Live `validate_table` smoke (`test-fsck`), including a promoted
  large file; host `galfs-fsck` / `galexy-galf` for v12 and v13 images

### Quotas (landed)

- Each actor stores `max_objects` / `max_bytes` (durable on GALF v9+)
- New users: 16 objects / 16 KiB; admin: table + pool maxima
- Enforced on create, append (short write), truncate grow, cross-actor rename
- `USER_QUOTA` / `USER_SETQUOTA`; shell `quota` and `quota set`

### Durable home shares (landed)

- GALF v10 share table (32 × 6-byte records): grantee actor + object + rights
- `Share` / `Unshare` syscalls; shell `share` / `unshare`
- Login / `su` re-applies shares into the session token table
- `userdel` drops shares naming the deleted actor or its removed objects
- Live `grant`/`revoke` remain task-scoped (logout clears those cards)

### Host fsck (landed)

- `crates/galexy-galf`: layout constants, sealed unlock, structural issues
- `crates/galfs-fsck`: CLI over `galfs.img` (default passphrase `galfs`; `--passphrase` for one the operator chose)
- Runner asserts host check on a guest-written image and both-corrupt

### Single-indirect (landed)

- Object records one `indirect` u16 after the eight direct pointers
- Append / read / truncate / free walk directs then the indirect block
- Max file 32 KiB (keeps on-disk `len` as u16); host fsck marks indirect
- `bin/test-indirect`

### Card limits & confused deputy (landed)

- `TOKEN_SLOTS` = 8; `SHARE_SLOTS` = 32; full table → `NoResource`
- `share` uses the same hold-every-right check as `grant`
- `bin/test-cards`

### Path policy (landed)

- Byte charset; reject `.` / `..`, overlong, depth > 8, NUL, non-ASCII
- `bin/test-paths`

### Durable share disk e2e (landed)

- `bin/test-share-disk` + runner `boot_with_galfs`: share survives reboot
- Host decode sees a used share slot; plaintext file marker absent

### Torn-write recovery (landed)

- Host tears the newest slot mid-payload-sector (`boot_with_galfs_torn`)
- Guest loads the older sibling and logs `(recovered from bad sibling)`
- Host fsck still accepts the intact sibling

### Open-file budget (landed)

- **64** opens per task (`MAX_OPEN_FILES`, Milestone 71). The table is
  a heap `Vec` allocated when the task is registered, before the IRQ
  gate, so `open` still only indexes a slot on the IF=0 path.
  `PROC_CAP_BASE` is `0x80` so the file band does not overlap process
  caps.

### Idempotent mutate after recover (landed)

- `bin/test-galfs-idempotent` + `boot_with_galfs_recover`
- Duplicate create → `Unsupported`; remove+recreate advances generation

### Crash injection (landed)

- `boot_with_galfs_crash` kills QEMU during the guest's in-flight
  `sync` (`bin/test-crash`)
- Next boot loads a consistent slot: the pre-crash file survives and
  the killed mutate does not

### ATA errors (landed)

- ERR/DF and command timeout log `[ata] I/O error` / `[ata] I/O timeout`
  and return `Unsupported` (no panic). Absent slave: same error.
  `bin/test-ata`

### Follow-ons

- Milestone **101** clears a bad share and seals that metadata into
  the inactive slot. A cycle, a missing block, or a missing admin
  is still left on disk. The repair does not rewrite inode blocks
- Object `len` is `u64` (64 GiB per file). The old note about
  lengths past a `u16` described the v12 record. A second indirect
  block is not scheduled

### Scheduled commits (Phase 15, Milestones 97–99)

Checkboxes: `TODO.md`. The bar is a commit that costs the changed
bytes, one metadata seal, and one flush. None of the three bars is
a gigabyte per second.

- **97** (landed) — one virtio data request for the 64 KiB metadata
  slot (128 sectors), then one flush. `DISK_VERSION` does not move
  for this bar. Still one command in flight. `PERF.md` records the
  TCG sync time. The KVM cell stays empty on a host whose
  `KVM_CREATE_VCPU` fails
- **98** (landed) — the kernel formats and loads a large volume:
  changed 4 KiB blocks, one sealed 64 KiB metadata slot, one flush.
  Actors, shares, and quotas stay in that metadata. v16 also keeps
  inodes in pool blocks so a large disk can hold more than 1,000,000
  names and a file over 16 GiB. Host v13 (`MAX_FILES` = 32, no actor
  table) is the extent-map shape, not the login image. Older images
  are refused. A 1 MiB file survives reboot. On the 8 MiB test image
  `core`'s metadata (~66 MiB) stays in the RAM store because the live
  file cap is a few megabytes
- **99** (landed) — one desk compile names 41 objects, written above
  and in the sealed geometry. 1,048,576 stays the format ceiling. The
  version moves to 17 with `galexy-galf`, and a v16 image is refused.
  Dual-slot order stays. A crash recovers the previous metadata slot.
  No journal

Left out of this phase: `async` / `await`, several virtio commands in
flight, ATA PIO as the fast path, POSIX, mmap, sparse files, hard links.

### Storage stack (Milestone 46 ✅)

Default attach is virtio 1.x on `-M q35` (PCI capabilities, MMIO BARs,
MSI-X). A missed completion is noticed on the next timer tick. ATA PIO
and the legacy virtio I/O BAR (`disable-modern=on`, serial line
`legacy IO BAR`) stay as named fallbacks. The IDE matrix is one
`-M pc` case (port PCI config, serial line `config via 0xCF8`).

- **Landed:** `BlockDevice` + `ata::PrimarySlave` + IDENTIFY capacity;
  galfs via `disk()`; dual-slot size gate
- **Landed:** flush matrix — persistence e2e under QEMU
  `cache=writethrough` / `writeback` / `none`; commit ordering documented
  above
- **Landed:** `virtio-blk` legacy PCI (`drivers/virtio_blk`); galfs
  prefers it when present; `cargo run` attaches virtio-blk-pci by
  default (`GALEXY_GALFS_IDE=1` for IDE slave); `galfs_disk_persists_virtio_blk`
- **Landed:** partition LBA offset (`set_disk_lba_base` /
  `DISK_PART_LBA`); `bin/test-galfs-part`

## Explicit non-goals

- POSIX VFS, mount table, device nodes, hard links, symlinks
- uid/gid / mode-bit authorization as the primary model
- Sparse files (holes); truncate grow always allocates zeros
- Recursive delete (`rm -r` / non-empty `userdel`) — refuse instead
- Per-file keys, secure erase, TPM seal (until explicitly scheduled)
- Network filesystems
- Silent migration across incompatible GALF versions
- Checking in `galfs.img` or other stateful images

## Milestone map

| Milestone | Delivers |
| --- | --- |
| **44** ✅ | Sealed GALF (volume key + AEAD); threat model; no plaintext in image |
| **45** ✅ | Capacity, blocks, ops, sync, quotas, host fsck, shares, single-indirect, crash injection, ATA I/O errors |
| **46** ✅ | `BlockDevice`, capacity gate, flush matrix, legacy virtio-blk, partition offset |
| **63** ✅ | Per-actor KDF cost in the actor record (GALF v12) |
| **64** ✅ | virtio-blk parks on `IO_BLOCK`; completion IRQ (INTx then, MSI-X in 65) |
| **65** ✅ | virtio 1.x + MSI-X on `q35`; IDE (`-M pc`) and legacy virtio as fallbacks |
| **97** ✅ | One virtio request for the 64 KiB slot, then one flush; sync time in `PERF.md` |
| **98** ✅ | Kernel large volume: changed 4 KiB blocks plus one 64 KiB metadata seal; login table kept; v12 refused |
| **99** ✅ | Object table covers one desk compile (41 names); v17 refuses v16; crash recovers the previous metadata slot |
| **101** ✅ | Bad-share repair seals a new metadata slot. Anything else is left unwritten |

Demo limits above are the shipped contract. A second indirect block
is not scheduled; object `len` is already `u64`. New code must not
invent a second ambient “path implies permission” API beside tokens.
