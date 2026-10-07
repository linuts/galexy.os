//! Hardware device drivers.
//!
//! Drivers speak to hardware through `arch` APIs only (the port wall) and to
//! each other through `kcore` primitives — never directly to another driver.

pub mod ata;
pub mod block;
pub mod console;
pub mod keyboard;
pub mod pci;
pub mod screen;
pub mod serial;
pub mod virtio_blk;
