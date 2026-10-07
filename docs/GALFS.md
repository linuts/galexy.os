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
- **Explicit share** — `grant` / `revoke` move cards between live tasks

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
| Volume key | Random key that encrypts a sealed slot’s payload (v6) |

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
| Depth | Bounded (`MAX_DEPTH`); demo sizes in DESIGN |
| Archive names | Ramdisk hits first (READ); else galfs |

Shell cwd is userspace-only; the kernel always sees absolute or
owner-qualified components from the syscall args.

## Tokens on a task

Each user task holds a small fixed table of tokens (`TOKEN_SLOTS`).

```text
grant lr /Desktop notes     # list+read on Desktop → task "notes"
grant a /eve@/ shell2       # ALL on eve's root (login card)
revoke a /eve@/ shell2
```

| Spawn kind | galfs credentials (today) |
| --- | --- |
| Utility (`SPAWN_WAIT`) | Inherits parent’s session tokens |
| Bare program | Parent’s `fs_root`, **empty** tokens |
| Pre-login seat | No loader; cannot spawn |

Password `login` replaces tokens with `ALL` on the actor’s root.
`logout` clears tokens and `fs_root`. Card-based `su` installs `ALL` on
the target when the caller already holds that card (see `AUTH.md`).

Holding a **process Cap** to a child never grants galfs rights on that
child’s files (`PROCESS.md`).

## Table limits (today)

Demo sizes — Milestone **45** grows them for real usage:

| Resource | Cap |
| --- | --- |
| Actors | 16 |
| Objects (files + dirs + roots) | 64 |
| File payload | 512 bytes |
| Tokens per task | 8 |
| Path depth | 8 components |
| Name length | 64 (object) / 32 (actor) |

The IF=0 syscall path must not heap-allocate over this table. Capacity
growth either keeps that invariant or documents a deferred-work path.

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

### Sealed v6 (Milestone 44)

**Threat (v1):** stolen `galfs.img` must not yield file bytes or password
hashes offline. Cold-boot RAM and a live compromised kernel are out of
scope initially.

1. Format creates a random **volume key**.
2. KEK = PBKDF2(volume passphrase); wrap the volume key (ChaCha20-HMAC).
3. Encrypt the actor/object payload under the volume key.
4. AAD binds magic + version + generation (slot splice rejected).
5. CRC of ciphertext is a cheap reject before AEAD open.

Bring-up unlock uses a fixed volume passphrase (`galfs` today).
Interactive unlock is a follow-up. Details: `AUTH.md` → Sealed GALF.

v5 plaintext images are refused; delete `galfs.img` or let format
recreate.

## Boot and format

1. If ATA slave present → try load newest valid sealed slot.
2. Else / both bad → format: immortal `admin` + `Desktop/`, default
   password `admin`, new volume key, sync sealed image.
3. Mutates (`create` / `remove` / `append` / `useradd` / `userdel` /
   `passwd`, …) sync when disk-backed.
4. `userdel` refuses `admin`, non-empty trees, and roots still in use;
   clears tokens that named that actor’s objects.

`bin/test-galfs` exercises tokens in RAM. `bin/test-galfs-disk` proves
persist + dual-slot recover; the host asserts plaintext markers are
absent from the raw image.

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

- Fixed table sizes; 512-byte files
- Sealed dual-slot GALF v6 on the IDE slave
- Shell: `ls` / `cat` / `echo` / `touch` / `mkdir` / `rm` / `cp` / `mv`
  / `grant` / `revoke` via utilities + syscalls
- Admin operator bypass still broad (narrow in Milestone 43 leftovers)

### Target capacity (Milestone 45)

- Larger actors/objects/files via block/extent store
- Endian-safe on-disk structs; shared defs with host fsck
- Stress to `NoResource` without leaking blocks

### Target storage stack (Milestone 46)

- Deeper ATA / virtio story as needed for demos
- Keep dual-slot (or journal) commit discipline from STYLE

## Explicit non-goals

- POSIX VFS, mount table, device nodes, symlinks (unless later planned)
- uid/gid / mode-bit authorization as the primary model
- Per-file keys, secure erase, TPM seal (until explicitly scheduled)
- Network filesystems
- Silent migration across incompatible GALF versions
- Checking in `galfs.img` or other stateful images

## Milestone map

| Milestone | Delivers |
| --- | --- |
| **44** | Sealed GALF (volume key + AEAD); threat model; no plaintext in image |
| **45** | Capacity & layout for real usage; block store; fsck-friendly defs |
| **46** | Storage stack polish for demos / review |

Until 45 lands, demo limits above are the shipped contract. New code
must not invent a second ambient “path implies permission” API beside
tokens.
