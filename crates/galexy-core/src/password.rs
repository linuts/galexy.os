//! Password field sizes shared with on-disk GALF actor records.
//!
//! Hashing is in `galexy-crypto` (Argon2id). These lengths must match.

/// Salt length stored on each actor.
pub const SALT_LEN: usize = 8;
/// Hash length stored on each actor.
pub const HASH_LEN: usize = 16;
