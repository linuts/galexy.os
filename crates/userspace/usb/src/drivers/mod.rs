//! The only driver registration site.
//!
//! Milestone 88 shipped this table empty. Milestone 102 adds the HID
//! boot keyboard. A function that is not in [`TABLE`] is not called.
//! A network driver, were one added later, would hand frames to `net`
//! and would not speak TCP.

mod hid_kbd;

use galexy_usb::Driver;

/// Class and vendor modules. The HID boot keyboard is the only row.
pub static TABLE: &[Driver] = &[hid_kbd::ROW];
