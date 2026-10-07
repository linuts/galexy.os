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
still need a card (or a documented admin operator path — Milestone 43
narrows the blanket bypass).

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
| Pre-login seat | No loader; cannot spawn |

Password `login` replaces tokens with `ALL` on the actor’s root, then
applies durable home shares. `logout` clears tokens and `fs_root`.
Card-based `su` installs `ALL` on the target when the caller already
holds that card (see `AUTH.md`).

Holding a **process Cap** to a child never grants galfs rights on that
child’s files (`PROCESS.md`).

`bin/test-cards` covers token/share slot exhaustion and the
confused-deputy share rules.

## Table limits (today)

Milestone **45** / GALF **v11**: actor/object tables plus a shared block
pool, per-actor quotas, durable home shares, and single-indirect files.
Empty files cost an inode only; bytes live in direct/indirect blocks.

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
slot 0 @ LBA 0
slot 1 @ LBA DISK_SECTORS
```

| Property | Behavior |
| --- | --- |
| Dual slot | Mutate writes the **inactive** slot, then flush |
| Generation | Newer gen wins on load |
| Crash | Mid-write leaves the previous slot intact |
| Version | Layout bump **refuses** old images (no silent reinterpret) |

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

Bring-up unlock uses a fixed volume passphrase (`galfs` today).
Interactive unlock is a follow-up. Details: `AUTH.md` → Sealed GALF.

v11 refuses older images; delete `galfs.img` or let format recreate.

## Boot and format

1. If ATA slave present → try load newest valid sealed slot.
2. Empty zeros (no GALF magic) → format: immortal `admin` + `Desktop/`,
   default password `admin`, new volume key, sync sealed image.
3. Both slots carry GALF magic but fail decode/validate → **refuse
   silent format**; galfs stays unavailable (`Unsupported` on sync /
   mutates that need a mount). Serial: `disk corrupt; refusing silent
   format`.
4. Mutates (`create` / `remove` / `append` / `rename` / `truncate` /
   `useradd` / `userdel` / `passwd`, …) sync the inactive slot + flush
   when disk-backed. `Syscall::Sync` / shell `sync` is an extra barrier.
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

- GALF **v11**: 32 actors / 128 objects / 32 durable shares; 256×512 block
  pool; 8 directs + single indirect/file (32 KiB max); per-actor object +
  byte quotas (defaults for new users; admin at table max)
- Sealed dual-slot image (288 sectors/slot) on the IDE slave
- Object + block fill stress; remove reuses blocks without leaks
- Shell: `ls` / `cat` / `echo` / `touch` / `mkdir` / `rm` / `cp` / `mv`
  / `grant` / `revoke` / `share` / `unshare` / `quota` / `tokens` / `sync`
- Admin operator bypass still broad (narrow in Milestone 43 leftovers)

### Ops (landed)

- `rename` / `truncate` / `stat` syscalls; shell `mv` / `truncate` / `stat`
- Directory listing stays the `FILES` snapshot (`ls`); no dir `open`
- `write` is append-only; `seek` adjusts the read cursor only
- No hard links, symlinks, or sparse holes (truncate grow zero-fills)
- Names: ASCII alphanumeric plus `.` `_` `-`; `.` / `..` rejected

### Durability (landed)

- Every mutate syncs the inactive dual slot + flush; `sync` syscall barrier
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

### Remaining (Milestone 45)

- Double-indirect / lengths beyond u16
- Optional fsck repair into a new slot
- Crash injection (kill QEMU mid-mutate)
- ATA error surfacing (no panic on I/O failure where possible)

### Target storage stack (Milestone 46)

- Deeper ATA / virtio story as needed for demos
- Keep dual-slot (or journal) commit discipline from STYLE

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
| **44** | Sealed GALF (volume key + AEAD); threat model; no plaintext in image |
| **45** | Capacity, blocks, ops, sync, quotas, host fsck, shares, single-indirect; polish remain |
| **46** | Storage stack polish for demos / review |

Until 45 lands, demo limits above are the shipped contract. New code
must not invent a second ambient “path implies permission” API beside
tokens.
