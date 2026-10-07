# AUTH — passwords for identity, tokens for access

galexy.os separates **who you are** from **what you can touch**.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Authentication | Password (per actor, salted hash on disk) | Who is at this seat? |
| Authorization | galfs tokens (`RIGHT_*` on an object) | Which folders/files may this task use? |

Tokens are **access cards**. They are not a substitute for login. A card
can be handed to another user or to an app without sharing a password.
A password never grants rights on someone else’s tree by itself — after
login you hold `RIGHT_ALL` on **your** root; everything else is granted.

Disk encryption is out of scope for now (Milestone 44). Passwords use
**PBKDF2-HMAC-SHA256** (`galexy-crypto`: 10 000 iterations) with an
8-byte CSPRNG salt and a 16-byte digest per actor (GALF **v5**). Salts
come from `arch::rand` (RDRAND, with a tick-mixed fallback). Empty
passwords are rejected. Syscall staging buffers are wiped after login /
useradd / passwd. Iteration count is capped for debug-QEMU boot budget;
raise it (or switch to Argon2id on a dedicated KDF stack) once release
profiles or fatter kstacks make that practical.

## Pieces

| Piece | Meaning |
| --- | --- |
| Actor | Named account (`admin`, `eve`, …) with one root + Desktop |
| Password | Proves control of that actor; stored as salt + hash on the actor |
| Token | `(object, RIGHT_*)` on a task — the only way to touch galfs |
| Session | Current `fs_root` + token table + grants on the running task |
| Pre-login | Seat with no actor: `fs_root = none`, empty tokens, console+keyboard only |

There is **no guest account**. A seat is either logged in as a real actor
or logged out on the login screen.

`RIGHT_ALL` on an actor’s **root** is both that actor’s home session and
a shareable **login card**. Paths of the form `/eve@/` name that root for
`grant` / `revoke`.

## Boot

1. Format creates immortal `admin` with `Desktop/` and password `admin`.
2. **Every F-key shell (F1–F12)** starts **logged out**: no root, no
   tokens, grants = console + keyboard only (no loader, no queries, no
   power). The seat shows a login screen:

   ```text
   Galexy.OS v0.1.0 (tty1)

   Login as: _
   Password: ********
   ```

   Wrong password prints `Login incorrect` and repeats the screen.
3. After a successful password login the prompt is `user@galexy>`.
4. `whoami` while logged out fails (`AccessDenied`).
5. `logout` returns to the login screen (not a shell prompt).

## Password login and logout

```text
login [user] [password]
logout
```

`login` verifies the password and replaces the caller’s session with
`ALL` on that user’s root (previous tokens dropped). Admin sessions also
receive the power grant; other users get loader + queries without power.

When the password (or, for bare `login`, the user name) is omitted, the
shell prompts interactively. The password line echoes `*` only — never
cleartext — so the COM1 console mirror cannot leak it. Esc or Ctrl-C
cancels a prompt. Overlong input is rejected (no silent truncate).
Inline `login <user> <password>` remains for scripts and older tests.

`logout` clears tokens, sets `fs_root = none`, restores pre-login grants,
and returns the shell to the login screen.

After format, `admin` / `admin` is the default. A seat that logs in with
that pair must run `passwd` before other shell commands (`help`,
`whoami`, and `logout` remain available). Kernel-wide enforcement of the
same gate is a follow-up.

```text
useradd <name> [password]     # admin only; prompts if password omitted
passwd [name]                 # always masked Password: + Confirm:
```

`passwd` never takes an inline secret. Mismatched confirmations print
`passwd: passwords do not match` and leave the hash unchanged.

## Access cards (tokens)

From a logged-in seat that holds the rights:

```text
grant lr /Desktop notes       # give another live task list+read on Desktop
grant a /eve@/ shell2         # give shell2 a login card for eve
revoke a /eve@/ shell2
```

- **Apps** receive cards via `grant`, or inherit nothing (see spawn).
- **Users** receive cards the same way; possession of `/eve@/` ALL lets
  them `su eve` **without** eve’s password (card-based switch).
- Password login is always available as `login eve <pass>` when you do
  not hold a card.

```text
su <name>     # only if caller holds ALL on that root (or is admin session)
login …       # always password-checked identity switch
logout        # return to pre-login (not a switch to another user)
```

Admin may `su` to any actor without a password (operator seat). A session
whose `fs_root` is admin also passes every token check (list/open/grant),
so an admin console can manage any tree without collecting cards.
Returning to admin after `su` elsewhere uses `su admin` (if born-admin /
card) or `login admin <pass>`.

## Spawn policy (least privilege)

| Spawn kind | galfs credentials |
| --- | --- |
| Utility (`SPAWN_WAIT`) | Inherits the parent’s full session (short trusted tools) |
| Bare program (`hello`, `linger`, …) | Parent’s `fs_root`, **empty tokens** |

Pre-login seats cannot spawn (no loader grant). Ramdisk code is still
trusted enough to run once logged in; empty tokens stop a runaway bare
program from writing the caller’s tree. Utilities need create/open.

Process identity and wait/kill are a separate layer: spawn will return a
**process Cap** (see `docs/PROCESS.md`). Holding that Cap does not grant
galfs rights on the child’s files.

## Console flood budget

Each task may write a fixed number of console bytes per timer tick.
Further writes in that tick return success with `0` bytes copied until
the next tick. Interactive typing and `linger`’s paced `beat` stay
within budget; a tight write loop cannot pin COM1.

## Explicit non-goals (for now)

Tracked for review readiness in `TODO.md` Milestones 43–44 (auth +
sealed disk). Until those land:

- Disk encryption / sealed password store → Milestone 44
- No-echo CLI prompts, lockout, idle logout, must-change admin →
  remaining Milestone 43 items (KDF + CSPRNG salts shipped)
- PAM-style modules, MFA, networked IdP (still out of scope for review)
- Removing the `crash` test seam from production images → Milestone 43
  (kept for supervisor e2e; omitted from `help`)

**Note:** GALF **v5** refuses v4 images (CRC password hashes). Delete
`galfs.img` or let format recreate admin after upgrading.
