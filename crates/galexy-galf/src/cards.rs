//! Card (token) algebra: the pure part of galfs authorization.
//!
//! A task holds up to [`TOKEN_SLOTS`] cards. Each names one object and a
//! rights mask; a card on an object covers every descendant of that
//! object (ancestor closure). The kernel's `sched::galfs` owns the object
//! table and wraps these functions with its parent lookup; the host
//! enumerates them exhaustively (`cards_test.rs`).

use galexy_abi::SysError;

/// Cards per task.
pub const TOKEN_SLOTS: usize = 8;

/// No object / empty card.
pub const NO_OBJECT: u16 = 0xffff;

pub const RIGHT_READ: u8 = 1;
pub const RIGHT_WRITE: u8 = 2;
pub const RIGHT_LIST: u8 = 4;
pub const RIGHT_CREATE: u8 = 8;
pub const RIGHT_REMOVE: u8 = 16;
pub const RIGHT_ALL: u8 = RIGHT_READ | RIGHT_WRITE | RIGHT_LIST | RIGHT_CREATE | RIGHT_REMOVE;
/// Task-local flag: revoke this card on the first card-based `su` it authorizes.
pub const RIGHT_ONCE: u8 = 128;

/// One card: an object index plus a rights mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub object: u16,
    pub rights: u8,
}

impl Token {
    pub const fn empty() -> Self {
        Self {
            object: NO_OBJECT,
            rights: 0,
        }
    }

    pub const fn is_live(self) -> bool {
        self.object != NO_OBJECT
    }
}

/// Whether `ancestor` is `object` or one of its ancestors under
/// `parent_of` (`None` = no parent / not an object). Walks at most
/// `max_steps` links, so a corrupt cyclic parent chain terminates.
pub fn covers(
    parent_of: impl Fn(u16) -> Option<u16>,
    ancestor: u16,
    object: u16,
    max_steps: usize,
) -> bool {
    let mut cur = object;
    for _ in 0..max_steps {
        if cur == ancestor {
            return true;
        }
        let Some(parent) = parent_of(cur) else {
            return false;
        };
        cur = parent;
    }
    false
}

/// Whether `tokens` hold every right in `need` on `object` or an ancestor.
///
/// `need` is masked to [`RIGHT_ALL`]; an empty `need` is never allowed.
/// A single card must carry the whole `need` — rights do not sum across
/// cards on different ancestors.
pub fn allows(
    parent_of: impl Fn(u16) -> Option<u16>,
    tokens: &[Token],
    object: u16,
    need: u8,
    max_steps: usize,
) -> bool {
    let need = need & RIGHT_ALL;
    if need == 0 {
        return false;
    }
    tokens.iter().any(|token| {
        token.is_live()
            && token.rights & RIGHT_ALL & need == need
            && covers(&parent_of, token.object, object, max_steps)
    })
}

/// Installs a card into a fixed slot array. Same object merges rights.
pub fn push_token(
    tokens: &mut [Token; TOKEN_SLOTS],
    object: u16,
    rights: u8,
) -> Result<(), SysError> {
    if object == NO_OBJECT || rights == 0 {
        return Err(SysError::BadValue);
    }
    if let Some(slot) = tokens.iter_mut().find(|t| t.object == object) {
        slot.rights |= rights;
        return Ok(());
    }
    if let Some(slot) = tokens.iter_mut().find(|t| !t.is_live()) {
        *slot = Token { object, rights };
        Ok(())
    } else {
        Err(SysError::NoResource)
    }
}

/// Clears `rights` from the card that names `object` exactly. Cards on
/// ancestors are untouched — revocation does not climb the tree.
pub fn revoke_token(
    tokens: &mut [Token; TOKEN_SLOTS],
    object: u16,
    rights: u8,
) -> Result<(), SysError> {
    if object == NO_OBJECT || rights == 0 {
        return Err(SysError::BadValue);
    }
    let Some(slot) = tokens.iter_mut().find(|t| t.object == object) else {
        return Err(SysError::NotFound);
    };
    slot.rights &= !rights;
    if slot.rights == 0 {
        *slot = Token::empty();
    }
    Ok(())
}

/// AND inherited card rights with `mask`. `mask == 0` keeps every right.
/// A card whose rights fall to zero is dropped; [`RIGHT_ONCE`] survives.
pub fn attenuate_tokens(tokens: &mut [Token; TOKEN_SLOTS], mask: u8) {
    if mask == 0 {
        return;
    }
    let mask = mask & RIGHT_ALL;
    for token in tokens.iter_mut() {
        if !token.is_live() {
            continue;
        }
        let once = token.rights & RIGHT_ONCE;
        token.rights = (token.rights & RIGHT_ALL & mask) | once;
        if token.rights & RIGHT_ALL == 0 {
            *token = Token::empty();
        }
    }
}

/// Drops a one-shot card that names `object` exactly. Returns whether one was removed.
pub fn consume_once(tokens: &mut [Token; TOKEN_SLOTS], object: u16) -> bool {
    let mut hit = false;
    for token in tokens.iter_mut() {
        if token.is_live() && token.object == object && token.rights & RIGHT_ONCE != 0 {
            *token = Token::empty();
            hit = true;
        }
    }
    hit
}

/// Drops every card whose object is in `objects`.
pub fn drop_tokens_on(tokens: &mut [Token; TOKEN_SLOTS], objects: &[u16]) {
    for token in tokens.iter_mut() {
        if objects.contains(&token.object) {
            *token = Token::empty();
        }
    }
}
