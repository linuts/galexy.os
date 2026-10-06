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

Disk encryption is out of scope for now. The password hash is an interim
iterated mix (see `galexy_core::password`); it will be replaced with a
real KDF later without changing the syscall shape.

## Pieces

| Piece | Meaning |
| --- | --- |
| Actor | Named account (`admin`, `eve`, …) with one root + Desktop |
| Password | Proves control of that actor; stored as salt + hash on the actor |
| Token | `(object, RIGHT_*)` on a task — the only way to touch galfs |
| Session | Current `fs_root` + token table on the running task |
| Guest | Unauthenticated seat: `fs_root = none`, empty tokens |

`RIGHT_ALL` on an actor’s **root** is both that actor’s home session and
a shareable **login card**. Paths of the form `/eve@/` name that root for
`grant` / `revoke`.

## Boot

1. Format creates immortal `admin` with `Desktop/` and password `admin`.
2. **F1 (`shell`)** starts already logged in as `admin` (physical console
   trust at boot), with launcher grants including power, and `born_admin`.
3. **F2–F12** start as **guest**: no root, no tokens, session grants
   (console, keyboard, loader, queries — **no power**).
4. `whoami` as guest prints `guest`.

## Password login

```text
login <user> <password>
```

Kernel verifies the password and replaces the caller’s session with
`ALL` on that user’s root (previous tokens dropped). Works from guest
or from another session.

```text
useradd <name> <password>     # admin only
passwd <name> <password>     # admin, or self with current session
passwd <password>            # change own password
```

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
```

Admin may `su` to any actor without a password (operator seat). A session
whose `fs_root` is admin also passes every token check (list/open/grant),
so the F1 console can manage any tree without collecting cards. Returning
to admin from a born-admin F1 seat still works via `su admin` or
`login admin <pass>`.

## Spawn policy (least privilege)

| Spawn kind | galfs credentials |
| --- | --- |
| Utility (`SPAWN_WAIT`) | Inherits the parent’s full session (short trusted tools) |
| Bare program (`hello`, `linger`, …) | Parent’s `fs_root`, **empty tokens** |

Ramdisk code is still trusted enough to run; empty tokens stop a runaway
bare program from writing the caller’s tree. Utilities need create/open.

## Console flood budget

Each task may write a fixed number of console bytes per timer tick.
Further writes in that tick return success with `0` bytes copied until
the next tick. Interactive typing and `linger`’s paced `beat` stay
within budget; a tight write loop cannot pin COM1.

## Explicit non-goals (for now)

- Disk encryption / sealed password store
- PAM-style modules, MFA, networked IdP
- Removing the `crash` test seam (kept for supervisor e2e; omitted from `help`)
