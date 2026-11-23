# NET — lanes, not sockets

Phase 10. Milestones **78–85**. Required before the `v1.0` tag.
Checkboxes live in `TODO.md`. This page is the design those boxes
implement. Milestones 78–85 have landed. `serve` attaches a gate
(8 gates, 4 pending lanes) and hands the next inbound lane to that
listener. `serve_hands_the_lane_to_its_listener` is the run: the
attaching seat prints the TLS body, and a second admin seat that
holds the write bit receives `held`.

The network is the lane and the card. DHCP, ping, a granted TLS 1.3
lane, listen, one datagram, and SSH are in the tree. A seat logs in
over that one service when it holds `/card/ssh` (`THREAT.md`). The
object is not a socket.

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
| **Locator** | One address and a transport port, stored inside the directory. The address is one type. A v4 value and a v6 value are that type. A locator is how the service finds a peer. A program may pass that stored address or port only when it holds the card. |
| **Dial** | Ask the service for a lane to a name this seat was granted. |
| **Fetch** | The shell command that dials, reads the lane to EOF, and prints the bytes. The phase gate. |

`shell`, `shell2`, … `shell12` stay the seats. `net` is a supervised
service beside them (`PROCESS.md`).

## What this refuses

Phase 10 refuses an ambient socket. A later row names the milestone
that may grant one call. That call does not exist until its box is
checked.

| Refused | Why |
| --- | --- |
| BSD sockets and Winsock (`socket`, `bind`, `listen`, `accept`, `connect`, `sendto`, `recvfrom`, `setsockopt`) | That API is an ambient port namespace. Galexy does not grow one. Milestones 112–116 honor one address, one port, one local bind, one listen, and one datagram peer, each with its card. `setsockopt` stays refused. |
| `std::net::TcpStream` and a POSIX shim in the PAL | Milestone 108 keeps the name `TcpStream::connect`. It asks, then dials a granted name, one stored address, or one stored `address:port`. The value is still a lane. `TcpListener::bind` binds the stored local address when the seat holds write on that card. `accept` on that listener, and `listen_port`, accept on the stored port with write on that card. The value is a lane. `UdpSocket::send_card` sends to the stored peer. `UdpSocket::recv_card` receives one datagram from that same peer. Port 53 stays refused. `setsockopt` stays refused. There is no socket syscall and no POSIX shim. |
| In-kernel TCP/IP | A parse bug on a packet would be ring 0. The kernel moves frames. The `net` service speaks the protocols. A second stack, a C stack, raw sockets, promiscuous mode, and a v6-only opcode stay out. IPv4 and IPv6 are one address type (Milestones 120–124). |
| IP addresses and port numbers in the syscall ABI | Ports stay an internal demux. A program does not pass them to a syscall. Milestones 112 and 113 let a program pass one exact address or port on the existing net cap, and only with that card. An app still cannot bind `0.0.0.0`, spoof a source, or race a port it does not hold. |
| Cleartext as an application right | The bytes an app reads on a lane are the plaintext of a finished TLS 1.3 session. There is no raw TCP Cap. Milestone 116 grants one exact datagram peer, and only with that card. Write on the card does not capture. |
| The public WebPKI as the default trust for a lane | A name is pinned to a key. An empty pin on a dial asks the seat, shows the fingerprint, and stores the key only on yes. A different key later is refused. Trust-on-first-use is that question, not a silent accept. The DNS resolver is the exception. The image carries a system CA bundle, the Mozilla roots. `net pin <ip>` reads the certificate at that address, stores the domain on it, and tells the seat. Later lookups trust a certificate that chains to the bundle, is currently valid, and is valid for that domain. The address you typed stays the address. A key file is still the raw key, and then the bundle is not consulted. SSH host keys and `net allow` do not use the bundle. |
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
    │  one address type (v4 and v6), neighbor discovery, ICMP echo, UDP, TCP, TLS 1.3
    │  directory: name → locator + pinned key + the token that may dial
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
**35**. The table is `0..=35`, and `Date` is 34. Only the task named `net` may
call it. Any other caller is `AccessDenied`. `read` and `write` on a
lane Cap are the existing syscalls, dispatched on the Cap type. There
is no `connect` to forget to check.

Opcodes on `0x800A`, appended and then stable, same rule as init:

| Op | Who | Effect |
| --- | --- | --- |
| 1 `link` | any logged-in seat | `down`, `up`, or `up` plus our locator once Milestone 80 has one |
| 2 `dial` | read on the card | reply carries the app end of a new lane. The actor list is not the check |
| 3 `allow` | admin | install a name: locator, pinned key, no grants yet |
| 4 `grant` | admin | write an actor on the record. That list is not the dial right. The shell writes the read token on `/card/<name>` |
| 5 `revoke` | admin | drop that actor-list entry. The shell drops the card token. The list is not the dial right |
| 6 `serve` | admin, and write on the card | attach a gate for a name to the calling task |
| 7 `ping` | a seat that holds the ping card | ICMP echo to one address, or to a DNS name resolved over the pinned DNS-over-TLS resolver. The reply is `reply from` that address, or `timeout`. No lane Cap. No card, no query. Today the address is four bytes. Milestones 120–124 make it the one address type, on this opcode |
| 8 `resolve` | read on a DNS card | DNS-over-TLS lookup. The reply is one address of the one type. No lane Cap. A key file stays in the record. No key file stores the address and the domain from the certificate, and trusts the bundle for that domain. Milestone 123 may return a v6 value. Until that box is checked, the answer is v4 |
| 9 `pin` | admin | Pin the DNS-over-TLS resolver. Four address bytes connect, read the domain off the certificate, and trust the bundle for that domain. The reply is `ok` and that domain. A 32-byte key, when one is passed, is that pin and the bundle is not consulted. `10.0.2.2` and `10.0.2.3` are refused. Today the address is four bytes. Milestone 120 makes it the one type. The port stays 853 |
| 10 `bind` | write on the address card | Bind the stored local address. The payload is that address. This opcode does not listen |
| 11 `listen` | write on the port card | Listen on the stored port. The next reply carries the inbound lane |
| 12 `datagram` | read on the datagram card | Send one datagram to the stored peer. Port 53 is refused. Write does not send |
| 13 `recv` | read on that same card | Receive one datagram from that peer. Port 53 is refused. A packet from any other peer is ignored |

`fetch` is a shell builtin on top of `dial`. `ping <ip>` and `ping <name>` are shell builtins on top of opcode 7. Neither is a syscall.
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
| Gateway MAC | ARP for the gateway, before any unicast IP packet. The cache holds 8 entries. IPv6 neighbor solicitation is the same step, covered by host tests. The QEMU smoke is IPv4. Milestone 122 is the reach past that path, on the one address type. |
| ICMP echo | `ping <ip>` or `ping <name>` from a logged-in seat that holds the ping card. An address is four bytes until Milestone 120. A name is resolved over DNS-over-TLS, then one echo goes to that answer. The gateway may be the next hop. QEMU user networking answers `10.0.2.2` directly. The seat does not hold the wire. A missing card sends no query. Milestone 122 is one echo to a v6 neighbor or gateway that is not `10.0.2.2`, on this same card. |
| First packet a peer must answer | After the ack and the ARP reply, one UDP datagram to the gateway. The payload is `galexy`. The host test receives it. That is Milestone 80. It is not a TCP handshake and it is not a lane. |

DHCP here is a client. Discover, offer, request, ack. The first offer wins. Galexy does not serve DHCP in this phase. A lease is learned state: address, prefix, gateway, expiry. On expiry the service asks again. A missing record means mode `ask` and an empty directory, so a fresh QEMU boot still gets an address.

`link` reports the result. Any logged-in seat may read it. The locator in that reply is our address, not a dial argument. A program does not pass an address or a port to a new syscall. It passes one stored address or port on net cap `0x800A`, and only with that card.

## The record

Names, grants, pinned keys, and the address mode survive a restart of `net`. The service table in init does not: that table is compiled in. The network record is a file.

| | |
| --- | --- |
| Path | `/net` in the admin tree. One component, one file. |
| Format | Versioned bytes, version 2, defined by `crates/galexy-netcfg`. Version 1 is refused. Magic, version, mode, optional learned lease, the name directory (name, port, kind, address, 32-byte key, actors), then the optional pinned resolver. Tag 0 is absent. Tag 1 is the address and a 32-byte key. Tag 2 is the address and the domain taken from the certificate, and that resolver trusts the bundle for that domain. A tag 2 record with no domain still loads, and lookup fails closed until the seat pins again. An unknown tag is refused. The version stays 2. |
| Who parses it | The crate. Host tests round-trip a record. `net` links the same crate. A version it does not know is refused: the service logs the version and will not dial until an admin rewrites the file. |
| Who may read it | `net`, and admin. Init creates an empty version-2 record (mode `ask`, no names, no resolver) when the file is absent, then spawns `net` with one token: `READ` and `WRITE` on that object and on nothing else. Init holds the admin credential, so it can mint that card. The service does not receive the admin root. |
| Who writes it | `net`, from `allow`, `grant`, `revoke`, and `pin`, and from a finished DHCP lease. The shell does not poke the file. |
| Disk version | Unchanged by this phase. The record is a file in the sealed table, not a new GALF layout. |

There is no environment block and no `/etc`. Shell history is the other file a program already keeps (`shell.history` in the user's tree). The network record follows that idea and is typed, because a lease and a key are not a line of history.

## Identity and secrecy

A directory entry pins a 32-byte public key (a raw key, the RFC 7250
shape, not a chain). The verifier accepts that key and rejects every
other. `net allow` may omit the key file. The first dial of that name
learns the peer key, shows `fp=` (the first eight bytes of SHA-256),
and asks `yes` or `no`. Yes stores the key on the name and writes one
line to `ssh.known` in the seat's tree. No stores nothing. A later
dial that sees a different key replies `changed` and does not ask
again. The audit line for a dial carries the actor, the name, the
first eight bytes of the fingerprint in hex, and the result. It does
not carry the key, the session secrets, or the plaintext. Serial and
`dmesg` follow the same rule as passwords (`STYLE.md`). The question
itself is the one place the fingerprint is shown.

Cipher suites for this phase: `TLS_AES_128_GCM_SHA256` and
`TLS_CHACHA20_POLY1305_SHA256`, X25519, TLS 1.3 only. The password KDF
stays PBKDF2. Poly1305 here is the lane AEAD, not a replacement for
the volume MAC (`THREAT.md` still refuses to swap that primitive).

The address is one type. A v4 value and a v6 value are that type.
Cards, dial, the reply, bind, listen, datagrams, and the route use
it. Programs do not branch, and `net` does not grow a second path.
Today the lease, the resolver, and ping are four bytes, and the
record still has two locator kinds. Milestones 120–124 close that
gap. Until those boxes are checked, the QEMU smoke is IPv4, and
IPv6 packet build and neighbor solicitation stay host tests.
Happy eyeballs is a second path and stays out. There is no second
stack, no raw socket, no plaintext DNS, and no general browser TLS.

DNS, when a name is not a literal locator, is resolved by `net` over
TLS to a resolver the admin pinned. That is DNS-over-TLS (port 853).
Cleartext DNS is not implemented. The lookup does not use UDP/53
and it does not ask QEMU's DNS at `10.0.2.3`. QUIC stays the later
lane carrier, not this lookup. A name that needs DNS and has no
pinned resolver fails closed.

`net pin <ip>` stores that address. It connects on port 853, reads
the certificate, and stores the domain name on it. The seat prints
`ok` and that domain. The certificate must chain to the system CA
bundle in the image, be currently valid, and be valid for that
domain. The address you typed does not have to be on the
certificate. The bundle is the Mozilla roots, linked as pure Rust
(`webpki-roots`). No key file is required. A key file is optional.
When it is passed, that raw 32-byte key is the pin and the bundle
is not consulted for that resolver. A certificate that chains to
neither is refused, and no query is sent. `10.0.2.2` and `10.0.2.3`
stay refused. The pure-Rust provider already in `net` verifies
ECDSA P-256, ECDSA P-384, and Ed25519. An RSA signature is refused.
There is no OpenSSL and no C.

This bundle is the resolver only. A lane, an SSH host key, and
`net allow` stay pinned keys. An empty pin on a dial still asks
the seat. WebPKI is not the trust for those.

`milestones_110_through_114` is the
run: one TLS query, an answer other than `10.0.2.2`, and a dial
whose TCP destination is that answer. The same lookup answers a
dotted name passed to `ping`. The ping card is the right. The name
does not need its own card. A missing ping card, a missing pin, or
a bad pin sends no query. `ping_name_resolves` is that run.
`resolver_trusts_the_bundle` accepts a chain with no key file when
the certificate names the domain taken from it, including when that
domain is not the pinned address, and refuses a certificate that
does not chain. No query is sent while the domain is learned.
`resolver_pin_still_wins`
sends no query for that certificate, accepts only the key in the
file, and still refuses `10.0.2.2` and `10.0.2.3`.

## Adversary this adds

`THREAT.md` adversary **A6** is defended by the tests named there.

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

- `crates/galexy-netcfg` defines the version-2 record. Version 1 is
  refused. Host tests round-trip one. A wrong version is an error.
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
`Syscall::Lane` (35) mints a pair; only `net` may call it. `read` /
`write` move plaintext. Budgets in the table above are enforced, even
before the flood test.

Prove, on the host: a handshake fixture against a known key, and a
rejected key. Prove, in the guest: the service finishes TLS with the
host peer and a test task reads `lane ok` from the lane Cap. A shell
task that calls `lane` is `AccessDenied`. Session keys do not appear
on serial or `dmesg`. `docs/ABI.md` marks syscall 35 and index
`0x800A` experimental.

### 83 Names and gates

The directory, `allow` / `grant` / `revoke` / `serve` / `dial`.
Each change rewrites the record, so a restart of `net` keeps it.
A name the seat was not granted does not open a TCP connection.
Admin grants it; dial then returns a lane. `serve` attaches a gate.
The next inbound lane is delivered to that listener. A second
`serve` from another seat returns `held` and no lane Cap. The lane
carries TLS plaintext. Locators and ports
exist in the record and in the admin opcode payload. They do not
appear in `galexy-abi` as a general argument, and `fetch` will not
take them. Literal locators stay in the record. Milestone 110 adds the
encrypted lookup: DNS-over-TLS to the pinned resolver, and a
missing pin still fails closed.

### 84 Fetch

Shell builtin `fetch <name>`. Typed on a login seat that holds the
grant: the command prints a small body served by the host peer, then
the prompt returns. A seat without the grant sees `access denied`
and the host sees no handshake. `std::net` is the lane.
`TcpStream::connect` asks, then dials.

### 85 Solid — Phase 10 gate

The budgets hold under a peer that opens handshakes and never
finishes them. `svc status shell` still answers. Audit lines match
the secrecy rule. `docs/ABI.md` may mark syscall 35 and the net
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
4. This file is the reading copy. The repo has no `wiki/` directory.
5. A test is named before the box is checked. A box is checked only
   after a run shows the behavior.

## After this phase

USB landed as Phase 13 (`docs/USB.md`), Milestones **86–88**.
That phase sees devices and locks the driver table. It does not
implement vendor drivers, and it does not carry IP. A future NIC
that is not virtio delivers Ethernet frames to this service. It
does not grow its own stack. The `v1.0` tag waits on a KVM pass.

RustPython landed as Phase 14 (`docs/PYTHON.md`), Milestones
**91–96**. Milestone **103** is the later one: `lane.read(name)`
dials a granted name and reads the body. `socket.connect` is
Milestone 109: the same lane, by name. There is no new syscall.

QUIC, 0-RTT, and device checksum offload are later
carriers and later rights. They attach to the lane. They do not add
an ambient socket. The DNS resolver trusts the CA bundle in the
image. A lane does not. Phase 16's extra calls are cards. Milestones
110–117 have landed. SSH is the same lane. Milestones 120–124
are one address type, and they stay open. Milestone 125 is the
resolver bundle.

## Phase 16 — A card is the firewall

Milestones **105**, **107**, **108**, and **109** landed. **108**
and **109** are the name layer. **110–116** are the real stack.
**117** is SSH, and it has landed. Checkboxes in `TODO.md`. This plan
does not tag `v1.0` and does not check the KVM rows.

A network name is a galfs object under `/card` (the same object
as `/admin@card/<name>`). `/card/ping` is the ping card. The
locator and the pinned key stay in the service record. Who may
use the name is a token. Read is dial. Write is accept. The
kernel stamps the held bits into header byte 2 (`TOKEN_READ` is
1, `TOKEN_WRITE` is 2). `net` refuses a dial or a ping without
bit 1, and an accept without bit 2, before TCP, DNS, or ICMP. A
ping of a dotted name uses that same bit, then the pinned
DNS-over-TLS resolver. Actor
strings stay in the record and are not consulted. `may_dial`
remains for that record. Logged-out stays `AccessDenied`. There
is no chain, no interface match, and no packet filter. The
firewall is the card, not a packet-filter language, and it lives
in `net`. Admin's root token covers the per-name cards. A seat
without one does not open TCP. A path or an address is not
authority.

Read on the exact object `/card/out/<program>` is that
program's outbound card (Milestone 118). Header byte 2 also
carries `NET_HELD_OUT` (`0x20`), which is not a galfs right.
The program dials any stored name, including a subdomain, and
any address it passes, with no per-host ask. The reply comes
back on the same lane. An empty pin is a plaintext lane, so an
HTTP client can write the request and read the response. A
stored pin stays the one-shot TLS fetch. The card does not
bind, listen, or accept. Write on the port card is still the
accept. A program without the object keeps the per-name ask.
Logged-out stays denied. DNS-over-TLS on port 853 is unchanged.
`share r` stores that read in the metadata slot (Milestone
119). Init creates `/card/out/` and does not grant it. Login
does not put the object on the shell. Spawn installs it when
the actor's share names that task. The token dies with the
task. `unshare` drops the row. `DISK_VERSION` stays 17.

A foreground program that does not hold the token parks. The
shell, which is Cap-waiting, prints `<name> wants the network`
or `<name> wants storage`, and reads yes or no. Yes installs a
token on that task for this run. The parent may `grant` that
exact path and those rights even when the parent does not hold
the card. The program cannot grant itself. No is `AccessDenied`
and the program stays up. A second ask for the same right in
the same run does not print. A task that already holds the
token does not ask. The kernel does not draw a dialog. There
is no new syscall number. The token dies when the task exits.
A per-name grant is not an always that survives reboot. The
outbound card is: `share r /card/out/<program> <actor>` is
read in the metadata slot, and the next boot installs it on
that named task.

Milestone 108 keeps the name `std::net::TcpStream::connect`. The
call asks (Milestone 107), then dials a granted name through
`net`. The value is still a lane. Milestone 109 is that layer for
Python `socket.connect`: the call asks, then dials, and a no is
`OSError`. There is no socket syscall and no second stack. rustls
stays in `net`. The PAL does not parse TCP. This is not a Berkeley
socket. Those two milestones do not pass an IP or a port.
`setsockopt` stays refused. Listen is Milestone 115. One datagram
peer is Milestone 116.

Milestone 110 is the lookup: DNS-over-TLS to a pinned resolver,
not UDP/53, and not `10.0.2.3`. Milestone 111 is the reach. A
program passes a name. `net` resolves and dials. The TCP
destination is the resolved address, not the QEMU gateway
`10.0.2.2`. The gateway may be the next hop. Outbound TCP and
TLS stay inside `net`. virtio-net stays the device. `net` stays
the only packet stack.

Milestone 112 stores one address on a card. Read on that card
lets the program pass that address and no other. Milestone 113
stores one destination port. An address card does not imply the
port. Milestone 114 binds the stored local address, and only
with write on the address card. Read does not bind. `0.0.0.0`
is denied. The record is version 2; version 1 is refused. No new
syscall, and no new right bit. `milestones_110_through_114` is
that run. Milestone 83 hands the next lane to the one listener
that attached the gate, and only when that seat holds the write
bit. `serve_hands_the_lane_to_its_listener` is that run.

Milestone 115 opens the gate with write on the port card. Without
that card, no gate opens. The runner forwards one host TCP
connection. The guest does not dial `10.0.2.2`. A second listener
receives nothing. `milestones_115_and_116` is that run. Milestone
116 sends one datagram to the peer stored on the card, and only
with read. The same card receives one datagram from that peer.
Write does not send and does not capture. Port 53 stays unused.
The Milestone 80 datagram to the gateway stays inside `net`.

`std::net` and Python `socket` call those cards. `TcpStream::connect`
and `socket.connect` take a name, one stored address, or one stored
`address:port`. `TcpListener::bind` and `socket.bind` use the bind
card. `TcpListener::accept`, `listen_port`, and `socket.listen` use
the listen card. `UdpSocket::send_card` and `socket.send` send.
`UdpSocket::recv_card` and `socket.recv` receive. The kernel asks
when the card is missing. A call that names an address and a port
asks for each card that is missing. No is `AccessDenied` in Rust and `OSError`
in Python. The value is still a lane or the card reply. `setsockopt`
stays refused. There is no second stack.

Milestone 117 is that server. `milestone_117_ssh_reads_a_file` is
the run. One init row, in-tree Rust, no OpenSSH, no libssh, no C,
and no crates.io SSH library. `/card/ssh` write accepts. Read lets
a local program open a session. The program passes no address.
`net` keeps the TCP bytes. Strict key exchange, then
`curve25519-sha256`. Host keys are Ed25519. The cipher is
`chacha20-poly1305@openssh.com`. The MAC is that AEAD. SFTP is a
subsystem of that session, and the files it can touch are the
galfs tokens the seat already holds (the existing PBKDF2 path, not
a new user table). A client that offers only a refused algorithm
gets no session. Serial does not carry the host-key private bytes
or the password. There is no fallback to a legacy group, and no
new syscall. Opcode 14 on net cap `0x800A` is the live lane.
`read` and `write` with `r8` bit 0 set return `NoResource` instead
of parking, so `net` can relay. Both are experimental in `ABI.md`.

An empty pin on the outbound card is a plaintext lane. A stored
pin stays TLS, and a dial without that card stays the one-shot
TLS fetch even when the pin is empty. Still refused after these
milestones: the BSD option surface, `setsockopt`, a port or an
address in the syscall ABI, a program-chosen cleartext mode when
a pin is stored, plaintext DNS, TLS before 1.3, iptables,
raw frames, raw sockets, promiscuous mode, a second stack, telnet,
FTP, rsh, SSHv1, `ssh-rsa`, CBC, and SHA-1. The `v1.0` tag stays
unapplied.

## One address (Milestones 120–124)

v4 and v6 are one address type. The address card, dial, the reply,
bind, listen, the datagram, and the route take that type. Opcodes
stay 1–14. There is no `/card6`, no v6 opcode, and no second stack.
Milestones 118 and 119 have landed. Listening stays write on the
port card. The boxes for 120–124 stay open. This plan does not
tag `v1.0`.

Milestone 120 is the type. A v4 value and a v6 value are it.
Today's four-byte lease, resolver, and ping become that type when
the layout changes, and `galexy-netcfg` bumps. The old version is
refused. `DISK_VERSION` stays 17. A v4 dial, bind, listen, and
datagram keep their opcodes. This row sends no v6 packet.

Milestone 121 is one parser. The shell and `net` call it. It
refuses a form that can be two addresses, or a name and an
address: a v4 octet with a leading zero, a zone id, a second
`::`, `::ffff:192.0.2.1`, `::192.0.2.1`, `0.0.0.0`, and `::`. Two
spellings of one unambiguous value are the same value. A refused
form opens nothing. No packet.

Milestone 122 is the reach. Neighbor discovery finds one v6
neighbor or one v6 gateway that is not the QEMU user path
`10.0.2.2`. One echo goes there, on opcode 7 and `/card/ping`.
The cache stays 8. No lane. The host test of a solicitation does
not check this box.

Milestone 123 is the lookup. Opcode 8, DNS-over-TLS on port 853,
the pinned resolver. The reply may be a v6 value of the one type.
The program passes the name. One reply is one address. There is
no second connection and no UDP/53. This row does not dial.

Milestone 124 is the reply. Read on `/card/out/<program>` dials,
and the reply is read on that connection, when the answer is v6.
The same card covers a v6 address the program passes. A dial by
name does not also need the address card. The card does not bind,
listen, or accept. Inbound stays write on the port. The lane is
the one Milestone 118 already uses.

Milestone 125 is the resolver. `net pin <ip>` is enough. The seat
reads the domain off the certificate and prints it. The certificate
must chain to the Mozilla roots in the image, be currently valid,
and be valid for that domain. The address you typed stays the
address. A key file is that raw key, and the bundle
is not consulted. A certificate that chains to neither is refused,
and no query is sent. `resolver_trusts_the_bundle` and
`resolver_pin_still_wins` are the runs. Lanes, SSH host keys, and
`net allow` do not use the bundle.

Still out: a second stack, a raw socket, plaintext DNS, general
browser TLS, WebPKI on a lane, happy eyeballs, a v6 opcode, and
the `v1.0` tag. The resolver bundle does not extend past the
resolver.

## Files this phase touches

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
