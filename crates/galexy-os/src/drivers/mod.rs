//! Hardware device drivers.
//!
//! Drivers speak to hardware through `arch` APIs only (the port wall) and to
//! each other through `kcore` primitives — never directly to another driver.

pub mod keyboard;
pub mod screen;
pub mod serial;
