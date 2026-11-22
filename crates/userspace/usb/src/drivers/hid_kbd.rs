//! HID boot-protocol keyboard.
//!
//! The row matches class 3, subclass 1, protocol 1. A mouse (protocol 2)
//! and any other class stay off this row. The service arms the interrupt
//! pipe before it prints the device. Key bytes are decoded in the kernel
//! and land in the same queue virtio-input and the i8042 use.

use galexy_usb::{Driver, Ids, Match};

pub static ROW: Driver = Driver {
    id: Match::Class(3, 1, 1),
    module: "hid_kbd",
    claim,
};

fn claim(_ids: &Ids) {
    galexy_rt::write_console(b"claimed hid-kbd\n");
}
