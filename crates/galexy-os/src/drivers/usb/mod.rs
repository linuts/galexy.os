//! USB host controllers.
//!
//! The kernel moves transfer rings. Descriptor bytes are copied out to
//! the `usb` service, which is the only task allowed to interpret them.

pub mod xhci;
