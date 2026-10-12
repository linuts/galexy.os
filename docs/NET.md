# NET — lanes, not sockets

Phase 10. Milestones **78–85**. Required before the `v1.0` tag.
Checkboxes live in `TODO.md`. This page is the design those boxes
implement. Nothing in this phase is built yet.

Galexy has no network today, and that is why nothing in the tree is
reachable remotely (`THREAT.md`). The tag should not ship that way.
A machine you can log into and compile on, and cannot deliberately
reach, is missing a fundamental object. The object is not a socket.

## What a program holds

A program holds a **lane**, or it does not. A lane is one encrypted
session to one peer, and a capability with `READ` and `WRITE`. Drop
the Cap and the session ends. `give` moves an end. There is no
integer you can guess, no table indexed by a port, and no call that
means "open a network endpoint because I asked."

The words this phase uses:

| Word | What it is |
| --- | --- |
| **Wire** | The one virtio-net device. A reserved Cap. The `net` service is the only task that can read or write it. |
| **Lane** | The application object. TLS 1.3 to one peer. Bytes in, bytes out. |
| **Gate** | The right to receive new lanes for one **name**. Held by one listener. |
| **Name** | A label in the net service's directory (`echo`, `docs`). The same charset as a galfs path component, at most 64 bytes. |
| **Locator** | An IPv4 or IPv6 address and a transport port, stored inside the directory. A locator is how the service finds a peer. It is not an argument a normal program can pass. |
| **Dial** | Ask the service for a lane to a name this seat was granted. |
| **Fetch** | The shell command that dials, reads the lane to EOF, and prints the bytes. The phase gate. |

`shell`, `shell2`, … `shell12` stay the seats. `net` is a supervised
service beside them (`PROCESS.md`).

## What this refuses

These are design decisions, not a backlog.

| Refused | Why |
| --- | --- |
| BSD sockets and Winsock (`socket`, `bind`, `listen`, `accept`, `connect`, `sendto`, `recvfrom`, `setsockopt`) | That API is an ambient port namespace with fifty years of options. Galexy will not grow one. |
| `std::net::TcpStream` and a POSIX shim in the PAL | `std`'s `net` stays `unsupported`. A lane is not a TCP socket with a different name. |
| In-kernel TCP/IP | A parse bug on a packet would be ring 0. The kernel moves frames. The `net` service speaks the protocols. |
| IP addresses and port numbers in the syscall ABI | Ports are an internal demux. Source addresses are chosen by the service. An app cannot bind `0.0.0.0`, spoof a source, or race a well-known port. |
| Cleartext as an application right | The bytes an app reads are the plaintext of a finished TLS 1.3 session. There is no "raw TCP" Cap and no datagram Cap for ordinary tasks. |
| The public WebPKI as the default trust | A name is pinned to a key the admin installed. Trust-on-first-use is not silent. A system CA bundle is not in the image. |
| TLS before 1.3, renegotiation, compression, 0-RTT | Replay and oracle bugs we do not have to accept. 0-RTT can be a later right. It is off in this phase. |
| A hand-rolled handshake | The service links a maintained Rust TLS 1.3 implementation (rustls-class) with a pure-Rust crypto provider. No C. If that crate will not link on `x86_64-unknown-galexy`, Milestone 82 fixes the link. It does not replace the handshake. |
| QUIC as a second API | QUIC is the intended next carrier. It shows up behind the same lane Cap. This phase's carrier is TLS 1.3 over TCP, inside the service. |
| iptables, eBPF, NAT, raw frames, promiscuous capture | One device, one service, a grant list. Packet capture is not a side effect of holding a lane. |
| USB networking, Wi-Fi, a second NIC | virtio-net 1.x on the q35 path from Milestone 65. A legacy virtio I/O BAR is not part of these milestones. |
| Overloaded pipes | The pipe table is 8 slots of 256 bytes (`sched/pipe.rs`). A lane is its own object. |

## Shape

```text
virtio-net
    │  frames, DMA bounded like virtio-blk
    ▼
kernel wire Cap 0x8009          only the task named net
    │
    ▼
net  (ring 3, init child, restart)
    │  IPv6 and IPv4, neighbor discovery, UDP, TCP, TLS 1.3
    │  directory: name → locator + pinned key + who may dial
    │
    ├── lane Cap ──read/write── the program that dialed
    └── gate ──new lane Caps── the one listener for that name
```

Seats reach the service the way they reach init. Reserved index
`0x800A` is the net control Cap. `send` parks for a reply. The kernel
stamps the same 20-byte header it stamps for init: admin, tty, debug
id, session generation. A logged-out task is `AccessDenied`. The
sender cannot set the admin byte.

The reply to a dial carries one lane Cap (the application's end) plus
a short text line. Channel `send` already moves up to two Caps. The
service keeps the other end and writes plaintext into it after it
decrypts. The application never sees records, headers, or keys.

Creating that pair is the one new syscall. `Syscall::Lane` is number
**34** (the table today is `0..=33`). Only the task named `net` may
call it. Any other caller is `AccessDenied`. `read` and `write` on a
lane Cap are the existing syscalls, dispatched on the Cap type. There
is no `connect` to forget to check.

Opcodes on `0x800A`, appended and then stable, same rule as init:

| Op | Who | Effect |
| --- | --- | --- |
| 1 `link` | any logged-in seat | `down`, `up`, or `up` plus our locator once Milestone 80 has one |
| 2 `dial` | a seat granted that name | reply carries the app end of a new lane |
| 3 `allow` | admin | install a name: locator, pinned key, no grants yet |
| 4 `grant` | admin | let an actor dial a name |
| 5 `revoke` | admin | drop that dial right |
| 6 `serve` | admin | attach a gate for a name to the calling task |

`fetch` is a shell builtin on top of `dial`. It is not a syscall.
`svc status net` still means "is the process up," through init.
`link` means "is the wire up."

## Budgets

Fixed, so a peer cannot grow them. Changing a number is a milestone,
not a tuning pass.

| Budget | Value |
| --- | --- |
| Devices | 1 virtio-net |
| Lanes | 32 live |
| Lane buffer | 16 KiB each direction |
| Gates | 8 |
| Accept queue | 4 pending lanes per gate |
| Frame pool | 64 buffers, 2048 bytes each, inside `net` |
| MTU | 1500. No jumbo frames |
| Handshake | the service drops a lane that has not finished TLS 1.3 within 5 s of its own clock |
| DMA | one descriptor per physically contiguous run, page-bounded. The virtio-blk lesson (`dmasplit`) applies to the wire on day one |

The interrupt handler notifies. It does not parse IP. Checksums are
computed in the service. Device offload is an optimization that cites
`PERF.md` before anyone turns it on.

## Our address

The service has to have an address before it can talk to anyone. Milestone 80 is that step. Milestone 81's TCP handshake sits on top of it.

| Piece | Where it comes from |
| --- | --- |
| MAC | virtio-net device config, read in Milestone 78. The service does not invent one. |
| Mode | The record below. `ask` runs DHCP. `fixed` uses the locator stored in the record and does not ask. |
| Address, prefix, gateway | DHCP ack when the mode is `ask`. QEMU user networking offers `10.0.2.15/24` from `10.0.2.2`. A `fixed` record uses the bytes the admin stored. |
| Gateway MAC | ARP for the gateway, before any unicast IP packet. The cache holds 8 entries. IPv6 neighbor solicitation is the same step, covered by host tests. The QEMU smoke is IPv4. |
| First packet a peer must answer | After the ack and the ARP reply, one UDP datagram to the gateway. The payload is `galexy`. The host test receives it. That is Milestone 80. It is not a TCP handshake and it is not a lane. |

DHCP here is a client. Discover, offer, request, ack. The first offer wins. Galexy does not serve DHCP in this phase. A lease is learned state: address, prefix, gateway, expiry. On expiry the service asks again. A missing record means mode `ask` and an empty directory, so a fresh QEMU boot still gets an address.

`link` reports the result. Any logged-in seat may read it. The locator in that reply is our address, not a dial argument. Programs still cannot pass an address or a port into a syscall.

## The record

Names, grants, pinned keys, and the address mode survive a restart of `net`. The service table in init does not: that table is compiled in. The network record is a file.

| | |
| --- | --- |
| Path | `/net` in the admin tree. One component, one file. |
| Format | Versioned bytes, version 1, defined by `crates/galexy-netcfg`. Magic, version, mode, optional fixed locator, learned lease, then the name directory (name, locator, 32-byte key, actors who may dial). |
| Who parses it | The crate. Host tests round-trip a record. `net` links the same crate. A version it does not know is refused: the service logs the version and will not dial until an admin rewrites the file. |
| Who may read it | `net`, and admin. Init creates an empty version-1 record (mode `ask`, no names) when the file is absent, then spawns `net` with one token: `READ` and `WRITE` on that object and on nothing else. Init holds the admin credential, so it can mint that card. The service does not receive the admin root. |
| Who writes it | `net`, from `allow`, `grant`, and `revoke`, and from a finished DHCP lease. The shell does not poke the file. |
| Disk version | Unchanged. `DISK_VERSION` stays 12. This is a file in the sealed table, not a new GALF layout. |

There is no environment block and no `/etc`. Shell history is the other file a program already keeps (`shell.history` in the user's tree). The network record follows that idea and is typed, because a lease and a key are not a line of history.

## Identity and secrecy

A directory entry pins a 32-byte public key (a raw key, the RFC 7250
shape, not a chain). The verifier accepts that key and rejects every
other. The audit line for a dial carries the actor, the name, the
first eight bytes of the fingerprint in hex, and the result. It does
not carry the key, the session secrets, or the plaintext. Serial,
`dmesg`, and the console follow the same rule as passwords
(`STYLE.md`).

Cipher suites for this phase: `TLS_AES_128_GCM_SHA256` and
`TLS_CHACHA20_POLY1305_SHA256`, X25519, TLS 1.3 only. The password KDF
stays PBKDF2. Poly1305 here is the lane AEAD, not a replacement for
the volume MAC (`THREAT.md` still refuses to swap that primitive).

IPv4 and IPv6 are two locator forms inside `net`. Programs do not
branch. The QEMU smoke uses user networking (slirp), which is IPv4.
IPv6 packet build and parse are host tests. Happy eyeballs is not a
second API and is not this phase.

DNS, when a name is not a literal locator, is resolved by `net` over
TLS to a resolver the admin pinned. Cleartext DNS is not implemented.
Milestone 83 ships literal locators so dial works without a resolver.
Milestone 84's `fetch` may use either. A name that needs DNS and has
no pinned resolver fails closed.

## Adversary this adds

`THREAT.md` adversary **A6**, once Milestone 78 merges. Until then the
row is planned, not defended.

| Has | Wants | What answers |
| --- | --- | --- |
| Packets on the virtio-net link, including a peer who finishes TCP and fails TLS | kernel memory, a lane without a grant, plaintext, a stalled seat | Kernel parses no IP. DMA stays inside the frame buffers. Only `net` holds the wire. `lane` is `AccessDenied` for everyone else. Dial checks the grant. TLS 1.3 with a pinned key. Budgets above. Seats stay scheduled: a flood is the Milestone 85 test, and `svc status shell` must still answer. |

A logged-in seat that was not granted the name is the other half of
A6. The reply is `access denied`. The service does not open a
connection first and check later.

## Milestones

Each one leaves the tree bootable. The suite stays green. No milestone
adds a socket syscall "just for now."

### 78 Wire

virtio-net 1.x, MSI-X, on the PCI path Milestone 65 already uses
(`drivers/virtio_pci.rs`, next to `virtio_blk.rs`). The runner attaches
`virtio-net-pci` to QEMU user networking beside the existing
`virtio-blk-pci`. Reserved Cap `0x8009`. The test kernel is the holder.
The driver reads the MAC from the device config and logs it.

Prove: the link comes up; the logged MAC matches the device; a transmit
descriptor is consumed and the interrupt fires; a buffer that straddles
two non-adjacent physical frames does not touch a sentinel in the next
frame; a second task that reads `0x8009` is `AccessDenied`. This
milestone does not parse an IP header. A peer's reply waits for
Milestone 80.

### 79 Service

Ramdisk program `net`. A row in init's table: `restart`, autostart.
It is the only holder of `0x8009`. It counts frames and drops them.
`svc status net` answers `net running`. Serial logs link up and the
MAC. The kernel still has no IP. The record and the token arrive in
Milestone 80.

### 80 Address

The machine acquires an address and exchanges one datagram with
another device.

- `crates/galexy-netcfg` defines the version-1 record. Host tests
  round-trip one. A wrong version is an error.
- Init creates `/net` in the admin tree when it is missing (mode
  `ask`, no names) and passes `net` a token for that file only.
- Mode `ask`: DHCP discover, offer, request, ack. On QEMU user
  networking the ack is `10.0.2.15/24`, gateway `10.0.2.2`.
- ARP for the gateway. Then one UDP datagram, payload `galexy`, to
  that gateway. The host receives it.
- `link` from a logged-in seat answers `up` and the locator.
- Mode `fixed` skips DHCP and uses the stored locator. A lease is
  written back into the learned half of the record with its expiry.
- IPv6 parse and neighbor solicitation are host tests. No application
  Cap yet.

### 81 Carriers

TCP inside `net`, on the address from Milestone 80. Software
checksums. Host tests for the TCP machine. One guest smoke: a
handshake with a host listener through the same user network. UDP
from Milestone 80 stays the "we can talk" proof. TCP is the carrier
the lane will use. Still no application Cap.

### 82 Lane

TLS 1.3 in `net`, pinned key, rustls-class, pure Rust crypto.
`Syscall::Lane` (34) mints a pair; only `net` may call it. `read` /
`write` move plaintext. Budgets in the table above are enforced, even
before the flood test.

Prove, on the host: a handshake fixture against a known key, and a
rejected key. Prove, in the guest: the service finishes TLS with the
host peer and a test task reads `lane ok` from the lane Cap. A shell
task that calls `lane` is `AccessDenied`. Session keys do not appear
on serial or `dmesg`. `docs/ABI.md` marks syscall 34 and index
`0x800A` experimental.

### 83 Names and gates

The directory, `allow` / `grant` / `revoke` / `serve` / `dial`.
Each change rewrites the record, so a restart of `net` keeps it.
A name the seat was not granted does not open a TCP connection.
Admin grants it; dial then returns a lane. A gate delivers the next
lane to the listener task and to nobody else. Locators and ports
exist in the record and in the admin opcode payload. They do not
appear in `galexy-abi` as a general argument, and `fetch` will not
take them. Literal locators are enough; DNS is wired here only as
the fail-closed path (no pinned resolver, no lookup).

### 84 Fetch

Shell builtin `fetch <name>`. Typed on a login seat that holds the
grant: the command prints a small body served by the host peer, then
the prompt returns. A seat without the grant sees `access denied`
and the host sees no handshake. `std::net` is still unsupported.

### 85 Solid — Phase 10 gate

The budgets hold under a peer that opens handshakes and never
finishes them. `svc status shell` still answers. Audit lines match
the secrecy rule. `docs/ABI.md` may mark syscall 34 and the net
opcodes stable in this milestone, or leave them experimental until
the `v1.0` tag; they must not change shape after 85 without an ABI
note. `THREAT.md` A6 moves from planned to defended, with the tests
named.

The **phase gate** is Milestone 80's datagram, Milestone 84's typed
`fetch`, and Milestone 85's flood, on the release image, TCG
acceptable, KVM when the host can create a vCPU.

## Changing this subsystem

A network change is one milestone, and the same set of pages moves
together. That is the rule that keeps the lane from growing a second
API beside it.

1. Say which milestone in `TODO.md` and in this file. Do not add a
   syscall, an opcode, or a record field in a drive-by fix.
2. A new opcode appends after `serve`. A new record field bumps the
   `galexy-netcfg` version. Old versions are refused, not guessed.
3. Update `docs/ABI.md` when a reserved index or syscall number is
   involved, `docs/STYLE.md` when a rule changes, and `docs/DEVICES.md`
   when a device row changes.
4. The wiki page Network is the reading copy. It stays in step with
   this file.
5. A test is named before the box is checked. A box is checked only
   after a run shows the behavior.

## After this phase

USB is Phase 13 (`docs/USB.md`), Milestones **86–88**, after this
phase and before the `v1.0` tag. That phase sees devices and locks
the driver table. It does not implement vendor drivers, and it does
not carry IP. A future NIC that is not virtio delivers Ethernet
frames to this service. It does not grow its own stack.

QUIC, 0-RTT, device checksum offload, and a CA bundle are later
carriers and later rights. They attach to the lane. They do not add
`socket`.

## Files this phase will touch

When the milestones start, not in the plan PR:

| Piece | Where |
| --- | --- |
| Wire driver | `crates/galexy-os/src/drivers/virtio_net.rs` |
| Address record | `crates/galexy-netcfg` |
| Lane object | `crates/galexy-os/src/sched/` next to `pipe.rs` |
| ABI | `crates/galexy-abi`, `docs/ABI.md` |
| Service | `crates/userspace/net/` (a `std` or `no_std` program, whichever links the TLS crate), packed like `util` |
| Init row | `crates/userspace/init/src/main.rs` (the row, the empty record, the one token) |
| Shell | `fetch` and the admin `net` opcodes in `crates/userspace/shell` |
| Device row | `docs/DEVICES.md` — virtio-net |
| Tests | `bin/test-wire`, host tests for the record, DHCP, ARP, and TCP, a UDP receipt on the host, a typed `fetch` |

`MAX_PROC_CAPS` stays 16. `net` is one more long-running init child.
Adding it spends one of the four slots left after the twelve seats
(`docs` Services). Do not raise the ceiling to make room.
