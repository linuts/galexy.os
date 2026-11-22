//! The only driver registration site.
//!
//! Milestone 88 ships zero vendor modules. A function that is not in
//! [`TABLE`] is not called. Listing a device does not attach a driver
//! and does not inject keys. A network driver, were one added later,
//! would hand frames to `net` and would not speak TCP.

use galexy_usb::Driver;

/// Vendor and class modules. Empty on purpose.
pub static TABLE: &[Driver] = &[];
