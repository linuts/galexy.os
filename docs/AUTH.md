# AUTH — passwords for identity, tokens for access

galexy.os separates **who you are** from **what you can touch**.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Authentication | Password (per actor, salted hash on disk) | Who is at this seat? |
| Authorization | galfs tokens (`RIGHT_*` on an object) | Which folders/files may this task use? |

Filesystem shape (actors, paths, tokens, sealed GALF slots):
`docs/GALFS.md`. This doc is the auth/session layer on top.

Tokens are **access cards**. They are not a substitute for login. A card
can be handed to another user or to an app without sharing a password.
A password never grants rights on someone else’s tree by itself — after
login you hold `RIGHT_ALL` on **your** root; everything else is granted.

Passwords use **PBKDF2-HMAC-SHA256** (`galexy-crypto`: 10 000
iterations) with an 8-byte CSPRNG salt and a 16-byte digest per actor.
Salts come from `arch::rand` (RDRAND, with a tick-mixed fallback). Empty
passwords are rejected. Syscall staging buffers are wiped after login /
useradd / passwd / volume unlock, including the error path.
`check_password` wipes its digest, and PBKDF2 wipes HMAC key blocks.
Iteration count is capped for debug-QEMU boot budget;
raise it (or switch to Argon2id on a dedicated KDF stack) once release
profiles or fatter kstacks make that practical.

### Sealed GALF (at-rest disk)

**Threat model (v1):** an attacker who steals `galfs.img` / the ATA
slave must not recover file bytes or password hashes offline. Cold-boot
RAM extraction and a compromised live kernel are out of scope for now.

GALF **v8** slots are sealed (same AEAD as v6; actor/object tables plus
block pool — see `GALFS.md`):

1. Format creates a random 32-byte volume key.
2. A KEK is derived from the volume passphrase (`galfs` for bring-up)
   via PBKDF2; the volume key is wrapped with ChaCha20-HMAC-SHA256.
3. Actor/object payload is encrypted under the volume key (same AEAD);
   AAD binds magic + version + generation so slots cannot be spliced.
4. Test kernels and the disk harness auto-unlock with the bring-up
   passphrase (`galfs`). Production `galexy-os` calls
   `set_auto_unlock(false)` before `galfs::init`, so a usable disk stays
   locked until the seat submits a passphrase (`USER_UNLOCK`, prompted
   on the login screen as `Volume passphrase:`). An empty line skips
   the prompt and keeps the RAM table. A wrong passphrase logs
   `[galfs] unlock failed; RAM-only`, does **not** mark the disk
   corrupt (a later passphrase can retry), and `sync` returns
   `Unsupported`. Login against the RAM admin still works; disk
   mutates are not committed until unlock. An empty image is formatted
   under the typed passphrase.
5. The volume key and stored passphrase are wiped (`[galfs] volume key
   wiped`) after the last session logs out and after a power sync.
   The next login screen prompts again. Wiping the key does not
   guarantee destruction of RAM remnants: cold-boot extraction is an
   **accepted** residual risk.
6. The v6 tag is 16 bytes, but the MAC is HMAC-SHA256, not RFC 8439
   Poly1305. Swapping the MAC without a version bump would fail open
   on every sealed image. Poly1305 waits for a versioned cutover.
7. Non-goals: per-file keys, secure erase, TPM seal.

Older images (including sealed v7) are refused (format recreates admin).

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

   Volume passphrase: ********
   Login as: _
   Password: ********
   ```

   `Volume passphrase:` appears only when a disk volume is locked.
   The bring-up passphrase is `galfs`. Wrong password prints
   `Login incorrect` and repeats the screen.
   A cool-down prints `Login locked` (see below).
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

### Login lockout

Five failed guesses against **one actor** or **one TTY** start a
**5 second** cool-down (`LOCKOUT_MAX_FAILS` / `LOCKOUT_COOLDOWN_MS` in
`galexy-core`). The clock is monotonic `timer_ticks` (~1 ms), not a wall
clock. While it runs, `login` returns `Locked` **before** the password
KDF, so a locked guess does not reveal whether the password was right
and does not extend the deadline.

- An actor lock refuses that name on every seat.
- A TTY lock refuses every name on that seat (including unknown names,
  which count only against the TTY so probes cannot fill the actor table).
- The guess that reaches five still returns `AccessDenied` (known actor,
  wrong password) or `NotFound` (unknown name). The **next** guess is
  `Locked`.
- A successful password login clears that actor and that TTY.
- `userdel` drops the actor slot. The TTY cool-down stays.
- State is RAM-only. A reboot clears it. It is not written into GALF.

The login screen prints `Login locked`. The `login` command prints
`login: locked`. COM1 records the count and the arming line, with no
password material:

```text
[auth] login fail user=eve tty=1 fails=5
[auth] lockout user=eve for 5000ms after 5 fails
[auth] lockout tty=1 for 5000ms after 5 fails
[auth] login refused user=eve tty=1 locked
```

`stats` (query cap) appends `lockouts: N` — actor slots plus TTY slots
still inside a cool-down. Pre-login seats have no query cap; the serial
line is the signal there.

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
  them `su eve` **without** eve’s password (card-based switch). The
  path is only a lookup: the card is `ALL` on eve’s root object.
- `grant` may set the one-shot flag (`TOKEN_ONCE`, rights bit 7, shell
  letter not required). The first card-based `su` that token authorizes
  logs `[auth] card once user=… revoked` and drops that token. `su` as
  admin, or `su admin` on a born-admin seat, does not consume a card.
  Durable shares are not one-shot; they are re-applied at the next login.
- Password login is always available as `login eve <pass>` when you do
  not hold a card.

```text
su <name>     # only if caller holds ALL on that root (or is admin session)
login …       # always password-checked identity switch
logout        # return to pre-login (not a switch to another user)
```

Admin may `su` to any actor without a password (operator seat). That
installs `ALL` on the named root and is the way an operator touches a
foreign tree. Admin `fs_root` does **not** pass token checks on other
actors’ objects: list, open, grant, and share of `/eve@/…` need a card
(or `su eve` first). `useradd`, `userdel`, and `passwd <other>` stay
admin-only.

Returning to admin after `su` elsewhere uses `su admin` when the seat
was born/logged-in as admin (`born_admin` survives `su` away — it is
only cleared by `logout` or a password `login` as a non-admin), or
`login admin <pass>`. Non-admin sessions have no Power grant: `shutdown`
/ `reboot` return access denied.

Login and logout bump a session generation (`[auth] session login
user=… gen=N tty=K`). It is an audit counter, not a capability. A
logged-in F-key shell with no keystrokes for 60 s (`IDLE_LOGOUT_MS` on
`timer_ticks`) is exited; init respawns a logged-out seat
(`[auth] idle logout`).

COM1 audit lines name the actor, path, rights mask, or target task.
They never include password bytes. Rights are the raw mask
(`0x1` read, `0x2` write, `0x4` list, `0x8` create, `0x10` remove,
`0x80` one-shot). `dmesg` (query cap `0x8007`) shows the same lines
after login. Repeated `login fail` lines collapse in that ring.

```text
[auth] session login user=admin gen=1 tty=1
[auth] session logout gen=2 tty=1
[auth] session su user=eve gen=3 tty=1
[auth] passwd user=admin
[auth] passwd fail user=admin err=2
[auth] useradd user=eve
[auth] useradd fail user=eve err=2
[auth] userdel user=eve
[auth] userdel fail user=eve err=4
[auth] grant actor=admin path=/Desktop rights=0x1 target=shell2
[auth] revoke actor=admin path=/Desktop rights=0x1 target=shell2
```

A secret prompt (`login`, volume unlock, `passwd`, confirm) echoes `*`
and the COM1 mirror of that write is stars, not the secret. Esc or
Ctrl-C cancels the prompt. Ctrl-D is ignored; it is not end-of-file.

While `admin`’s password is still `admin`, the actor carries a
must-change flag (bit 15 of the on-disk object quota; the quota value
itself masks that bit off). Password login copies it onto the task.
Create, write, remove, rename, truncate, grant, share, `useradd`,
`userdel`, and `su` return `AccessDenied` until `passwd`. Reboot keeps
the flag because it lives in the sealed actor record.

## Spawn policy (least privilege)

| Spawn kind | galfs credentials |
| --- | --- |
| Utility (`SPAWN_INHERIT`, or legacy `SPAWN_WAIT`) | Parent’s tokens, optionally ANDed with the `r10` rights mask (bits 8..15). Mask `0` keeps the full set |
| Bare program (`hello`, `linger`, …) | Parent’s `fs_root`, **empty tokens** (the mask does not apply) |

Every `SPAWN_WAIT` / `SPAWN_INHERIT` ramdisk binary is trusted code
running with the caller’s cards. Milestone 51 signs and measures those
ELFs; until then, a hostile utility is a hostile operator.

Pre-login seats cannot spawn (no loader grant). Empty tokens stop a
runaway bare program from writing the caller’s tree. Utilities need
create/open.

Process identity and wait/kill are a separate layer: spawn will return a
**process Cap** (see `docs/PROCESS.md`). Holding that Cap does not grant
galfs rights on the child’s files.

## Console flood budget

Each task may write a fixed number of console bytes per timer tick.
Further writes in that tick return success with `0` bytes copied until
the next tick. Interactive typing and `linger`’s paced `beat` stay
within budget; a tight write loop cannot pin COM1.

## Explicit non-goals (for now)

- Interactive volume unlock (replace bring-up passphrase) → Milestone 44
- Argon2id (PBKDF2 stays; a dedicated KDF stack is deferred)
- One-shot scratch password syscall (interactive prompts already hide secrets)
- Wall clock. Audit lines use monotonic `timer_ticks` only
- PAM-style modules, MFA, networked IdP
- `crash` on production images. The command exists only in the
  `crash-seam` shell packed into `galexy-os-crashseam` for the
  supervisor typing test. `help` never lists it

**Note:** GALF **v8** refuses older images (including sealed v7). Delete
`galfs.img` or let format recreate a sealed volume after upgrading.
