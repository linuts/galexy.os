# USB — see the bus, then one way to add a driver

Phase 13. Milestones **86–88**. After Phase 10, before the `v1.0`
tag. Checkboxes live in `TODO.md`. Nothing in this phase is built
yet. Phase 14 (RustPython, Milestones 91–96) is the other `v1.0`
requirement. It does not use this bus.

virtio-net does not need this bus. Phase 10 talks to QEMU through
`virtio-net-pci`. USB is for a device that is actually plugged in:
a keyboard, a stick, a dongle. The first cut does not drive those.
It sees them, and it locks the place a driver will live so the next
one is not a special case.

## What you can see

A logged-in seat can list what enumeration found. A row is a device
the controller enumerated, not a claim that a driver is attached.

| Word | What it is |
| --- | --- |
| **Controller** | The one xHCI host on q35. A reserved Cap. The `usb` service is the only task that can touch the rings. |
| **Device** | One enumerated function: bus address, vendor, product, class, subclass, protocol, and a short string. |
| **Driver** | A Rust module named in the one driver table, matched by an ID from `docs/DEVICES.md`. Milestone 88 creates the table. It ships empty of vendor code. |
| **Matrix** | `docs/DEVICES.md`. The list of devices we intend to support, the public document that defines each one, and the Linux driver we used as an index. |

The service is a child of init, `restart`, autostart, same shape as
`net`. `MAX_PROC_CAPS` stays 16. `usb` spends one more slot. The
ceiling does not move.

## What this refuses

| Refused | Why |
| --- | --- |
| Pasting a Linux driver into this tree | Linux drivers are GPL. This tree is MIT. The matrix names the Linux file as the index of what that driver supports. The Galexy module is new Rust, written from the public specification or the vendor's programming manual. |
| Loading a Linux module, or a wrapper that calls one | A second kernel inside the kernel. The translation is a row in the matrix, not an ABI compatibility layer. |
| A driver that is not in the table | One list, `crates/userspace/usb/src/drivers/mod.rs`. A module the list does not name is not a driver. A list line without a `DEVICES.md` row is not a driver either. |
| Class match means "now type on every seat" | Enumeration prints a USB keyboard. It does not inject keys. virtio-input and PS/2 stay the keyboards until a later milestone says otherwise. |
| Parsing descriptors in the interrupt handler | The kernel moves transfer rings. The service interprets descriptors. A bad descriptor is `BadValue`, never a panic. |
| IP on this bus in this phase | A USB Ethernet dongle is a matrix row with status `unwritten`. Frames, when a driver exists, are delivered to `net`. The dongle does not grow a second stack (`NET.md`). |

## Shape

```text
xHCI (qemu-xhci on q35)
    │  transfer rings, DMA bounded like virtio-blk
    ▼
controller Cap 0x800B       only the task named usb
    │
    ▼
usb  (ring 3, init child)
    │  device and configuration descriptors
    │  the driver table (empty of vendor code in Milestone 88)
    │
    └── list ── read by any logged-in seat on Cap 0x800C
```

`0x800C` is a control Cap in the init style. `send` parks for a
reply. The kernel stamps the same 20-byte header. Logged-out is
`AccessDenied`. Opcode 1 is `list`. The reply is one device per
line: bus address, vendor, product, class, subclass, protocol,
string. Later opcodes append. They do not reuse the net opcodes.

## Budgets

| Budget | Value |
| --- | --- |
| Controllers | 1 xHCI |
| Devices | 32 |
| Interfaces per device | 8 |
| Configuration descriptor | 4096 bytes, then `BadValue` |
| String | 64 bytes after decode. USB allows 126. We keep the prefix. |
| Configurations | the first one. Alternate settings wait for a driver that needs them. |

DMA is one descriptor per physically contiguous run. The same rule
as virtio-blk and virtio-net.

## The matrix

`docs/DEVICES.md` is the only inventory. Columns:

| Column | Meaning |
| --- | --- |
| Kind | `pci` or `usb` |
| ID | PCI vendor:device, or USB vendor:product, or a class triple |
| Linux index | Path of the open driver that already covers this device. A citation. Not source we import. |
| Contract | The public spec or datasheet the Galexy code is written from |
| Module | Where the Rust will live, or `unwritten` |
| Milestone | The number that is allowed to create that module, or blank |
| Status | `planned`, `unwritten`, or `landed` |

A status changes when a milestone box changes. Adding a device is a
row in that file in the same change as the module. NICs that are not
virtio are rows of this kind. virtio-net's row points at Phase 10.
It is not reimplemented from a Linux file.

## Milestones

### 86 Controller

xHCI on the q35 path. QEMU device `qemu-xhci`. Reserved Cap
`0x800B`. The test kernel is the holder long enough to show the
controller came up, then Milestone 87 moves the Cap to the service.

Prove: the controller reports a link, MSI-X or the interrupt the
adapter uses fires, a second task reading `0x800B` is `AccessDenied`,
a straddling DMA buffer does not touch a sentinel. No descriptor
walk yet.

### 87 Devices

Ramdisk `usb` under init holds `0x800B`. It reads the device
descriptor and the first configuration descriptor for each device,
inside the budgets above. A hostile length, a truncated descriptor,
or a string that is not the USB string shape is `BadValue` and the
rest of the bus stays up.

The runner attaches one QEMU USB device with a known vendor and
product (a keyboard is fine). `list` on `0x800C` prints that ID.
The keyboard does not type. Serial shows the same line. `svc status
usb` answers `usb running`.

### 88 Contract — Phase 13 gate

The driver table exists and matches zero vendor modules. A device
with no row is listed and not claimed. `docs/DEVICES.md` has the
virtio-net row, the xHCI row, and the unwritten examples (a PCI NIC
and a USB Ethernet class) so the columns are real.

Prove: adding a fake row in the test points at a module, the module
runs, and a device that does not match stays unclaimed. Removing the
row means the module is not called. The phase gate is that test plus
Milestone 87's typed list.

## Adding a driver

After Milestone 88, this is the only way in.

1. Add a row to `docs/DEVICES.md`. Name the Linux driver as the
   index and the public document as the contract. Leave status
   `unwritten` until the module exists.
2. Write the module under `crates/userspace/usb/src/drivers/` for a
   USB device, or under `crates/galexy-os/src/drivers/` when it has
   to touch a PCI register block the service cannot. One device, one
   file.
3. Add one line to the driver table. The line's ID is the row's ID.
4. A NIC delivers Ethernet frames to `net`. It does not parse IP and
   it does not open a lane.
5. A test enumerates the device and shows the driver claimed it. A
   second device with a different ID is listed and not claimed.
6. Move the row to `landed` in the same change that checks the box.
   Update the wiki page USB.

A driver that skips the row does not build. The table is the list
the service walks. There is nowhere else to register.

## Changing this subsystem

Same habit as `NET.md`.

1. One milestone in `TODO.md` and in this file.
2. A new list opcode appends. A new budget is a milestone, not a
   constant edited in place.
3. `docs/ABI.md` for `0x800B` and `0x800C`. `docs/DEVICES.md` for a
   device. `docs/STYLE.md` when a rule changes.
4. The wiki page USB stays in step with this file.
5. A box is checked only after a run shows the behavior.

## Files this phase will touch

When the milestones start, not in this plan:

| Piece | Where |
| --- | --- |
| Controller | `crates/galexy-os/src/drivers/usb/xhci.rs` |
| Service | `crates/userspace/usb/` |
| Driver table | `crates/userspace/usb/src/drivers/mod.rs` |
| Init row | `crates/userspace/init/src/main.rs` |
| Inventory | `docs/DEVICES.md` |
| Tests | `bin/test-usb`, a typed `list` against a known QEMU device |

Vendor modules are not in Milestones 86–88. They wait on a row and
on the checklist above.
