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

Milestone **45** shaped the tables; the on-disk version is GALF **v12**
(Milestone 63 added `kdf_iters` on each actor). Actor/object tables
plus a shared block pool, per-actor quotas, durable home shares, and
single-indirect files. Empty files cost an inode only; bytes live in
direct/indirect blocks.

| Resource | Cap |
| --- | --- |
| Actors | 32 |
| Objects (files + dirs + roots) | 128 |
| Durable home shares | 32 |
| Block size | 512 bytes |
| Direct blocks per file | 8 |
| Single-indirect | 1 block of 256 u16 pointers |
| Max file size | 32 KiB (`len` is u16) |
| Block pool | 256 blocks |
| Default user quota | 16 objects / 16 KiB |
| Tokens per task | 8 |
| Open files per task | 8 (IF=0: no heap on `open`) |
| Path depth | 8 components |
| Name length | 64 (object) / 32 (actor) |
| Sectors per dual-slot image | 288 |

The IF=0 syscall path must not heap-allocate over this table. Double
indirect / larger-than-u16 lengths remain open.

## On-disk: GALF slots

When the primary IDE slave is present, the table is durable. Without a
slave it stays RAM-only.

```text
slot 0 @ LBA disk_lba_base()          (default 0; tests may use 2048)
slot 1 @ LBA disk_lba_base() + DISK_SECTORS
```

Call `set_disk_lba_base` before `galfs::init` when the volume should not
own absolute LBA 0 (e.g. after a protective MBR / GPT). Capacity must
cover `base + 2 × DISK_SECTORS`.

| Property | Behavior |
| --- | --- |
| Dual slot | Commit writes the **inactive** slot, then flush |
| Generation | Newer gen wins on load |
| Crash | Mid-write leaves the previous slot intact |
| Version | Layout bump **refuses** old images (no silent reinterpret) |

### Commit ordering and flush

Mutates update the in-RAM table and mark it **dirty**. A coalesced
commit (write-back) encodes the **whole** sealed slot (actors, objects,
shares, bitmap, and data blocks) into the inactive LBA range, issues
`BlockDevice::write_sectors`, then `BlockDevice::flush` **before**
publishing the new generation in RAM. Commits run on:

- the kernel main loop's 1 Hz tick (`sync_if_dirty`)
- `Syscall::Sync` / shell `sync`
- power-off / reboot
- explicit `galfs::sync()` in tests

Crash window: up to about one second of unflushed mutates (plus any
work after the last tick). There is no separate “data then metadata”
path: file bytes and directory metadata share one AEAD payload, so a
torn write cannot leave a newer directory pointing at uncommitted
blocks.

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
3. Encrypt the actor/object/**share/bitmap/block** payload under the volume key
   (each object carries 8 directs + one single-indirect pointer).
4. AAD binds magic + version + generation (slot splice rejected).
5. CRC of ciphertext is a cheap reject before AEAD open.

Test kernels auto-unlock with the fixed passphrase `galfs`. Production
boot leaves the volume locked and the F1 login screen prompts before
`Login as:`. A wrong passphrase refuses disk sync and keeps the RAM
table (not a corrupt image). The last logout and power-off zero the
volume key in RAM; cold-boot remanence is accepted. The MAC stays
HMAC-SHA256 (Poly1305 needs a versioned cutover). Per-file keys,
secure erase, and TPM seal are non-goals. Details: `AUTH.md` → Sealed GALF.

v12 refuses older images (including v11); delete `galfs.img` or let
format recreate. The actor record carries `kdf_iters` (4 bytes) after
the password hash.

## Boot and format

1. If a `BlockDevice` with capacity ≥ dual-slot image is present and
   auto-unlock is on → try load newest valid sealed slot with `galfs`.
   Production boot (auto-unlock off) builds a RAM admin table and waits
   for `USER_UNLOCK` instead of reading the image.
2. Empty zeros (no GALF magic) → format: immortal `admin` + `Desktop/`,
   default password `admin`, new volume key, sync sealed image.
   Interactive unlock of an empty image formats with the typed passphrase.
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
block pool and reuses after remove. `bin/test-galfs-disk` proves
multi-block persist + dual-slot recover; the host asserts plaintext
markers are absent from the raw image. `bin/test-galfs-corrupt` boots
a both-bad image and refuses format. `bin/test-fsck` checks the live
table after write/truncate/remove. `bin/test-shares` records a durable
share, proves login re-apply, unshare, and `userdel` cleanup.
`bin/test-indirect` writes past the eight directs, truncates through
indirect, and fills a 32 KiB file. Shell `tokens` lists the task’s
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

- GALF **v12**: 32 actors / 128 objects / 32 durable shares; per-actor
  `kdf_iters`; 256×512 block
  pool; 8 directs + single indirect/file (32 KiB max); per-actor object +
  byte quotas (defaults for new users; admin at table max)
- Sealed dual-slot image (288 sectors/slot) on the IDE slave
- Object + block fill stress; remove reuses blocks without leaks
- Shell: `ls` / `cat` / `echo` / `touch` / `mkdir` / `rm` / `cp` / `mv`
  / `stat` / `truncate` / `grant` / `revoke` / `share` / `unshare` /
  `quota` / `tokens` / `sync`; `echo text | cat` and `*` glob (M50)
- Admin operator bypass is narrow (Milestone 43): admin's root token
  does not cover foreign trees; `su` or a card is required

### Ops (landed)

- `rename` / `truncate` / `stat` syscalls; shell `mv` / `truncate` / `stat`
- Directory listing stays the `FILES` snapshot (`ls`); no dir `open`
- `write` is append-only; `seek` adjusts the read cursor only
- No hard links, symlinks, or sparse holes (truncate grow zero-fills)
- Names: ASCII alphanumeric plus `.` `_` `-`; `.` / `..` rejected

### Durability (landed)

- Write-back: mutates dirty the table; 1 Hz / `sync` / power commit the
  inactive dual slot + flush
- Boot logs slot/gen; recovery from a bad sibling is explicit
- Both-bad GALF magic → no silent format; volume stays unavailable
- Live `validate_table` smoke (`test-fsck`); host `galfs-fsck` / `galexy-galf`

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
- `crates/galfs-fsck`: CLI over `galfs.img` (bring-up passphrase `galfs`)
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

- **8** opens per task by design (`MAX_OPEN_FILES`); IF=0 syscall path
  must not allocate. Documented; raise needs a growth plan.

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

### Follow-ons (not this milestone)

- Double-indirect / lengths beyond u16
- Optional fsck repair into a new slot

### Storage stack (Milestone 46 ✅)

Legacy today, by the ROADMAP review: virtio-blk speaks the **legacy**
PCI IO-BAR transport and completes on INTx (a missed line is noticed
on the next timer tick). ATA is PIO and stays the legacy fallback.
Milestone 65 adds virtio 1.x (PCI capabilities, MMIO BARs, MSI-X) on
`-M q35` and keeps the paths below as fallbacks.

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
| **64 / 65** | IRQ completion; virtio 1.x + MSI-X on `q35`; IDE and legacy virtio as fallbacks |

Demo limits above are the shipped contract (double-indirect and fsck
repair-into-new-slot are follow-ons). New code must not invent a second
ambient “path implies permission” API beside tokens.
