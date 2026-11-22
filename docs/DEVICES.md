# DEVICES — the inventory

One table for every device Galexy intends to speak to. Phase 10 and
Phase 13 both use it. A driver without a row here is not a driver
(`USB.md`, `NET.md`).

`planned` means a milestone owns the row and the code is not in the
tree yet. `unwritten` means the row exists so the columns stay honest,
and no milestone is allowed to create the module until one is numbered.
`landed` means the milestone box is checked. virtio-net, xHCI, the
enumerated-device list, and the empty driver table are landed. The
example NIC and CDC-NCM rows stay unwritten.

## Rules

- The Linux path is an index of an open driver. It tells us the
  device is already supported somewhere and which IDs it claims. We
  do not copy, translate, or build that file. Galexy code is written
  from the contract column.
- Status changes in the same commit as the milestone box.
- A NIC that is not virtio, once it is landed, hands Ethernet frames
  to `net`. It does not implement DHCP, TCP, or TLS itself.
- USB keyboards stay off the input path until a milestone says
  otherwise. Listing a device is not attaching it.

## Rows

| Kind | ID | Linux index | Contract | Module | Milestone | Status |
| --- | --- | --- | --- | --- | --- | --- |
| pci | virtio-net, vendor `1af4` | `drivers/net/virtio_net.c` | virtio 1.x specification | `drivers/virtio_net.rs`, service `net` | 78–85 | landed |
| pci | xHCI (QEMU `qemu-xhci`, `1b36:000d`) | `drivers/usb/host/xhci.c` | eXtensible Host Controller Interface | `drivers/usb/xhci.rs` | 86 | landed |
| usb | any device the controller enumerates | — | USB chapter 9 device and configuration descriptors | service `usb`, list opcode | 87 | landed |
| usb | driver table, no IDs yet | — | `docs/USB.md` → Adding a driver | `userspace/usb/src/drivers/mod.rs` | 88 | landed |
| pci | example: Intel Ethernet `8086:10d3` (e1000e class) | `drivers/net/ethernet/intel/e1000e/` | Intel PCIe GbE controller datasheet | unwritten | — | unwritten |
| usb | example: CDC-NCM class `02` | `drivers/net/usb/cdc_ncm.c` | USB CDC NCM specification | unwritten | — | unwritten |

The last two rows are examples of the shape a vendor device will
take. They are not permission to start those drivers. Delete an
example only by replacing it with a real numbered row.
