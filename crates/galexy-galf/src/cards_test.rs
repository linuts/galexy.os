//! Property tests of the card algebra over random forests and card sets.
//!
//! Each property is checked against an independent model (an explicit
//! ancestor list) on thousands of generated cases from a fixed-seed
//! xorshift, so failures reproduce.

use super::cards::*;
use galexy_abi::SysError;

extern crate std;
use std::vec::Vec;

/// Objects per generated forest (parent indices stay below the child's).
const N: usize = 24;
const STEPS: usize = N;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

/// Parent of each object, or `None` for a root. Acyclic by construction:
/// a parent index is always smaller than its child's.
fn forest(rng: &mut Rng) -> Vec<Option<u16>> {
    (0..N as u16)
        .map(|i| {
            if i == 0 || rng.chance(4) {
                None
            } else {
                Some(rng.below(u64::from(i)) as u16)
            }
        })
        .collect()
}

fn link(parents: &[Option<u16>]) -> impl Fn(u16) -> Option<u16> + '_ {
    move |cur| parents.get(cur as usize).copied().flatten()
}

/// Independent model: `object` and every ancestor, root last.
fn ancestry(parents: &[Option<u16>], object: u16) -> Vec<u16> {
    let mut chain = std::vec![object];
    let mut cur = object;
    while let Some(p) = parents[cur as usize] {
        chain.push(p);
        cur = p;
    }
    chain
}

fn cards(rng: &mut Rng) -> [Token; TOKEN_SLOTS] {
    let mut tokens = [Token::empty(); TOKEN_SLOTS];
    let count = rng.below(TOKEN_SLOTS as u64 + 1) as usize;
    for _ in 0..count {
        let object = rng.below(N as u64) as u16;
        let mut rights = (rng.next() & u64::from(RIGHT_ALL)) as u8;
        if rights == 0 {
            rights = RIGHT_READ;
        }
        if rng.chance(5) {
            rights |= RIGHT_ONCE;
        }
        let _ = push_token(&mut tokens, object, rights);
    }
    tokens
}

#[test]
fn covers_matches_ancestry_model() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..400 {
        let parents = forest(&mut rng);
        for object in 0..N as u16 {
            let chain = ancestry(&parents, object);
            for ancestor in 0..N as u16 {
                assert_eq!(
                    covers(link(&parents), ancestor, object, STEPS),
                    chain.contains(&ancestor),
                    "parents={parents:?} ancestor={ancestor} object={object}"
                );
            }
            // NO_OBJECT never covers a real object.
            assert!(!covers(link(&parents), NO_OBJECT, object, STEPS));
        }
    }
}

#[test]
fn allows_is_grant_intersect_ancestor_closure() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for _ in 0..400 {
        let parents = forest(&mut rng);
        let tokens = cards(&mut rng);
        for object in 0..N as u16 {
            let chain = ancestry(&parents, object);
            for need in 1..=RIGHT_ALL {
                // Model: some live card on the chain carries all of `need`.
                let model = tokens.iter().any(|t| {
                    t.is_live() && chain.contains(&t.object) && t.rights & RIGHT_ALL & need == need
                });
                assert_eq!(
                    allows(link(&parents), &tokens, object, need, STEPS),
                    model,
                    "parents={parents:?} tokens={tokens:?} object={object} need={need}"
                );
            }
            // Empty need is never allowed; RIGHT_ONCE alone is not a right.
            assert!(!allows(link(&parents), &tokens, object, 0, STEPS));
            assert!(!allows(link(&parents), &tokens, object, RIGHT_ONCE, STEPS));
        }
    }
}

#[test]
fn allowed_on_object_implies_allowed_on_every_descendant() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for _ in 0..400 {
        let parents = forest(&mut rng);
        let tokens = cards(&mut rng);
        for object in 0..N as u16 {
            for need in [RIGHT_READ, RIGHT_WRITE, RIGHT_LIST, RIGHT_ALL] {
                if !allows(link(&parents), &tokens, object, need, STEPS) {
                    continue;
                }
                for child in 0..N as u16 {
                    if parents[child as usize] == Some(object) {
                        assert!(
                            allows(link(&parents), &tokens, child, need, STEPS),
                            "closure broke at child {child} of {object}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn rights_do_not_sum_across_cards() {
    // READ on the root and WRITE on the leaf: READ|WRITE on the leaf is
    // not granted by either card alone, so it is not granted.
    let parents = std::vec![None, Some(0u16), Some(1u16)];
    let mut tokens = [Token::empty(); TOKEN_SLOTS];
    push_token(&mut tokens, 0, RIGHT_READ).unwrap();
    push_token(&mut tokens, 2, RIGHT_WRITE).unwrap();
    assert!(allows(link(&parents), &tokens, 2, RIGHT_READ, STEPS));
    assert!(allows(link(&parents), &tokens, 2, RIGHT_WRITE, STEPS));
    assert!(!allows(
        link(&parents),
        &tokens,
        2,
        RIGHT_READ | RIGHT_WRITE,
        STEPS
    ));
}

#[test]
fn revoke_is_exact_object() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for _ in 0..400 {
        let parents = forest(&mut rng);
        let before = cards(&mut rng);
        let object = rng.below(N as u64) as u16;
        let rights = (rng.below(u64::from(RIGHT_ALL)) + 1) as u8;
        let mut after = before;
        let result = revoke_token(&mut after, object, rights);

        let held = before.iter().find(|t| t.object == object).copied();
        match held {
            None => {
                assert_eq!(result, Err(SysError::NotFound));
                assert_eq!(after, before, "a miss must not touch any card");
            }
            Some(card) => {
                assert_eq!(result, Ok(()));
                let remaining = card.rights & !rights;
                let now = after.iter().find(|t| t.object == object).copied();
                if remaining == 0 {
                    assert!(now.is_none(), "zero rights drops the card");
                } else {
                    assert_eq!(now.map(|t| t.rights), Some(remaining));
                }
                // Every other card is byte-identical.
                for (a, b) in after.iter().zip(before.iter()) {
                    if b.object != object {
                        assert_eq!(a, b, "revoke must not touch other cards");
                    }
                }
                // Access through an ancestor card survives: revocation
                // does not climb the tree.
                let chain = ancestry(&parents, object);
                for need in [RIGHT_READ, RIGHT_WRITE, RIGHT_LIST] {
                    let via_ancestor = before.iter().any(|t| {
                        t.is_live()
                            && t.object != object
                            && chain.contains(&t.object)
                            && t.rights & need == need
                    });
                    if via_ancestor {
                        assert!(
                            allows(link(&parents), &after, object, need, STEPS),
                            "ancestor card must still cover {object}"
                        );
                    }
                }
                // Descendants lose exactly what the exact card lost when no
                // ancestor card covers them.
                for child in 0..N as u16 {
                    if parents[child as usize] != Some(object) {
                        continue;
                    }
                    for need in [
                        RIGHT_READ,
                        RIGHT_WRITE,
                        RIGHT_LIST,
                        RIGHT_CREATE,
                        RIGHT_REMOVE,
                    ] {
                        let child_chain = ancestry(&parents, child);
                        let other = after.iter().any(|t| {
                            t.is_live()
                                && child_chain.contains(&t.object)
                                && t.rights & RIGHT_ALL & need == need
                        });
                        assert_eq!(allows(link(&parents), &after, child, need, STEPS), other);
                    }
                }
            }
        }
        // Invalid arguments never change anything.
        let mut copy = before;
        assert_eq!(
            revoke_token(&mut copy, NO_OBJECT, RIGHT_READ),
            Err(SysError::BadValue)
        );
        assert_eq!(revoke_token(&mut copy, 1, 0), Err(SysError::BadValue));
        assert_eq!(copy, before);
    }
}

#[test]
fn push_merges_same_object_and_caps_at_slots() {
    let mut rng = Rng(0xFEED_FACE_CAFE_BEEF);
    for _ in 0..400 {
        let mut tokens = [Token::empty(); TOKEN_SLOTS];
        let mut model: Vec<(u16, u8)> = Vec::new();
        for _ in 0..16 {
            let object = rng.below(12) as u16;
            let rights = (rng.below(u64::from(RIGHT_ALL)) + 1) as u8;
            let result = push_token(&mut tokens, object, rights);
            if let Some(entry) = model.iter_mut().find(|(o, _)| *o == object) {
                entry.1 |= rights;
                assert_eq!(result, Ok(()));
            } else if model.len() < TOKEN_SLOTS {
                model.push((object, rights));
                assert_eq!(result, Ok(()));
            } else {
                assert_eq!(result, Err(SysError::NoResource));
            }
            let live: Vec<(u16, u8)> = tokens
                .iter()
                .filter(|t| t.is_live())
                .map(|t| (t.object, t.rights))
                .collect();
            assert_eq!(live.len(), model.len());
            for (object, rights) in &model {
                assert!(live.contains(&(*object, *rights)), "{model:?} vs {live:?}");
            }
        }
        assert_eq!(
            push_token(&mut tokens, NO_OBJECT, RIGHT_READ),
            Err(SysError::BadValue)
        );
        assert_eq!(push_token(&mut tokens, 3, 0), Err(SysError::BadValue));
    }
}

#[test]
fn attenuate_never_adds_rights_and_keeps_once() {
    let mut rng = Rng(0x1357_9BDF_2468_ACE0);
    for _ in 0..1000 {
        let before = cards(&mut rng);
        let mask = (rng.next() & 0xFF) as u8;
        let mut after = before;
        attenuate_tokens(&mut after, mask);
        if mask == 0 {
            assert_eq!(after, before, "mask 0 is identity");
            continue;
        }
        for (a, b) in after.iter().zip(before.iter()) {
            if !b.is_live() {
                assert_eq!(a, b);
                continue;
            }
            let expect = b.rights & RIGHT_ALL & mask;
            if expect == 0 {
                assert!(!a.is_live(), "card with no rights left is dropped");
            } else {
                assert_eq!(a.object, b.object);
                assert_eq!(a.rights & RIGHT_ALL, expect);
                assert_eq!(a.rights & RIGHT_ONCE, b.rights & RIGHT_ONCE);
                assert_eq!(a.rights & !b.rights, 0, "no new bits");
            }
        }
    }
}

#[test]
fn consume_once_only_takes_the_marked_exact_card() {
    let mut rng = Rng(0xA5A5_5A5A_0F0F_F0F0);
    for _ in 0..1000 {
        let before = cards(&mut rng);
        let object = rng.below(N as u64) as u16;
        let mut after = before;
        let hit = consume_once(&mut after, object);
        let marked = before
            .iter()
            .any(|t| t.is_live() && t.object == object && t.rights & RIGHT_ONCE != 0);
        assert_eq!(hit, marked);
        for (a, b) in after.iter().zip(before.iter()) {
            if b.is_live() && b.object == object && b.rights & RIGHT_ONCE != 0 {
                assert!(!a.is_live());
            } else {
                assert_eq!(a, b);
            }
        }
    }
}

#[test]
fn drop_tokens_on_removes_exactly_the_listed_objects() {
    let mut rng = Rng(0x0BAD_F00D_DEAD_BEEF);
    for _ in 0..1000 {
        let before = cards(&mut rng);
        let gone: Vec<u16> = (0..rng.below(4))
            .map(|_| rng.below(N as u64) as u16)
            .collect();
        let mut after = before;
        drop_tokens_on(&mut after, &gone);
        for (a, b) in after.iter().zip(before.iter()) {
            if gone.contains(&b.object) {
                assert!(!a.is_live());
            } else {
                assert_eq!(a, b);
            }
        }
    }
}

#[test]
fn covers_terminates_on_a_cyclic_parent_chain() {
    // 0 -> 1 -> 2 -> 0: a corrupt table must not hang the kernel.
    let parents = std::vec![Some(1u16), Some(2u16), Some(0u16)];
    assert!(covers(link(&parents), 2, 0, STEPS));
    assert!(!covers(link(&parents), 7, 0, STEPS));
    let tokens = {
        let mut t = [Token::empty(); TOKEN_SLOTS];
        push_token(&mut t, 9, RIGHT_ALL).unwrap();
        t
    };
    assert!(!allows(link(&parents), &tokens, 0, RIGHT_READ, STEPS));
}
