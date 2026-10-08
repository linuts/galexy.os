//! Dual-slot selection properties: the newest valid generation wins, a
//! damaged newer slot falls back to the older one, and a sequence of
//! writes never makes the chosen generation go backwards.

use super::*;

extern crate std;
use std::boxed::Box;
use std::vec;
use std::vec::Vec;

const SLOT_BYTES: usize = DISK_SECTORS * SECTOR;
const PASS: &[u8] = DEFAULT_VOLUME_PASSPHRASE;
const VK: [u8; KEY_LEN] = [0x42; KEY_LEN];

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

    fn nonces(&mut self) -> SealNonces {
        let mut n = SealNonces {
            kdf_salt: [0; SALT_LEN],
            wrap_nonce: [0; NONCE_LEN],
            data_nonce: [0; NONCE_LEN],
        };
        for b in n
            .kdf_salt
            .iter_mut()
            .chain(n.wrap_nonce.iter_mut())
            .chain(n.data_nonce.iter_mut())
        {
            *b = self.next() as u8;
        }
        n
    }
}

/// Smallest table `collect_issues` accepts: admin with an empty root.
fn minimal_table() -> Box<Table> {
    let mut table = Box::new(Table::empty());
    let admin = &mut table.actors[0];
    admin.used = true;
    admin.name[..ADMIN_NAME.len()].copy_from_slice(ADMIN_NAME.as_bytes());
    admin.name_len = ADMIN_NAME.len() as u8;
    admin.root = 0;
    admin.kdf_iters = 10_000;
    let root = &mut table.objects[0];
    root.kind = KIND_DIR;
    root.parent = NO_PARENT;
    root.actor = 0;
    root.name[..4].copy_from_slice(b"root");
    root.name_len = 4;
    table
}

/// Stamps `generation` into a slot payload so decoded tables differ.
fn table_at(generation: u64) -> Box<Table> {
    let mut table = minimal_table();
    table.actors[0].max_bytes = generation as u32;
    table
}

fn write_slot(image: &mut [u8], slot: usize, generation: u64, rng: &mut Rng) {
    let table = table_at(generation);
    let nonces = rng.nonces();
    let flat = &mut image[slot * SLOT_BYTES..(slot + 1) * SLOT_BYTES];
    assert!(encode_slot(&table, generation, PASS, &VK, &nonces, flat));
}

fn check(image: &[u8]) -> Report {
    let mut slot_buf = vec![0u8; SLOT_BYTES];
    let mut best = Box::new(Table::empty());
    let mut cand = Box::new(Table::empty());
    let report = check_image(image, PASS, &mut slot_buf, &mut best, &mut cand);
    if let Some(gen) = report.generation {
        // The table that came back is the one written at that generation.
        assert_eq!(best.actors[0].max_bytes as u64, gen);
    }
    report
}

fn issues(report: &Report) -> Vec<Issue> {
    report.issues[..report.issue_count].to_vec()
}

#[test]
fn encode_decode_round_trip() {
    let mut rng = Rng(0x5EED_0001);
    let mut table = table_at(7);
    let mut flat = vec![0u8; SLOT_BYTES];
    assert!(encode_slot(&table, 7, PASS, &VK, &rng.nonces(), &mut flat));
    let mut decoded = Box::new(Table::empty());
    assert_eq!(decode_slot(&mut flat, PASS, &mut decoded), Some(7));
    assert!(decoded.actors[0].name_is(ADMIN_NAME));
    assert_eq!(decoded.objects[0].kind, KIND_DIR);
    assert_eq!(decoded.actors[0].max_bytes, 7);
    assert_eq!(decoded.actors[0].kdf_iters, 10_000);

    // An older stored cost round-trips; verify stays the caller's job.
    table.actors[0].kdf_iters = 1_000;
    let mut flat_old = vec![0u8; SLOT_BYTES];
    assert!(encode_slot(
        &table,
        7,
        PASS,
        &VK,
        &rng.nonces(),
        &mut flat_old
    ));
    assert_eq!(decode_slot(&mut flat_old, PASS, &mut decoded), Some(7));
    assert_eq!(decoded.actors[0].kdf_iters, 1_000);

    // Wrong passphrase: the KEK does not unwrap the volume key.
    let mut flat2 = vec![0u8; SLOT_BYTES];
    assert!(encode_slot(&table, 7, PASS, &VK, &rng.nonces(), &mut flat2));
    assert_eq!(decode_slot(&mut flat2, b"wrong", &mut decoded), None);

    // Too short a buffer is refused, not panicked on.
    let mut tiny = vec![0u8; 64];
    assert!(!encode_slot(&table, 7, PASS, &VK, &rng.nonces(), &mut tiny));
}

#[test]
fn newest_generation_wins_regardless_of_slot_order() {
    let mut rng = Rng(0x5EED_0002);
    // PBKDF2 dominates (three derivations per image); keep the count modest.
    for _ in 0..16 {
        let g0 = rng.next() % 1000;
        let g1 = rng.next() % 1000;
        let mut image = vec![0u8; DISK_SLOT_COUNT * SLOT_BYTES];
        write_slot(&mut image, 0, g0, &mut rng);
        write_slot(&mut image, 1, g1, &mut rng);
        let report = check(&image);
        assert!(report.ok, "{:?}", issues(&report));
        assert_eq!(report.generation, Some(g0.max(g1)));
        let want_slot = if g1 >= g0 { 1 } else { 0 };
        assert_eq!(report.slot, Some(want_slot), "g0={g0} g1={g1}");
    }
}

#[test]
fn damaged_newer_slot_falls_back_to_the_older_one() {
    let mut rng = Rng(0x5EED_0003);
    for _ in 0..12 {
        let older = rng.next() % 1000;
        let newer = older + 1 + rng.next() % 10;
        let new_slot = (rng.next() % 2) as usize;
        let mut image = vec![0u8; DISK_SLOT_COUNT * SLOT_BYTES];
        write_slot(&mut image, new_slot, newer, &mut rng);
        write_slot(&mut image, 1 - new_slot, older, &mut rng);
        assert_eq!(check(&image).generation, Some(newer));

        // Flip one payload byte of the newer slot: CRC and AEAD both
        // reject it; the older generation is served.
        let off = new_slot * SLOT_BYTES + DISK_HEADER + (rng.next() as usize % PAYLOAD_LEN);
        image[off] ^= 0x01;
        let report = check(&image);
        assert_eq!(report.generation, Some(older));
        assert_eq!(report.slot, Some((1 - new_slot) as u32));
        assert!(report.ok);

        // Damage the older one too: nothing valid remains, and the
        // report says so with the current-version marker intact.
        let off2 = (1 - new_slot) * SLOT_BYTES + DISK_HEADER + (rng.next() as usize % PAYLOAD_LEN);
        image[off2] ^= 0x80;
        let report = check(&image);
        assert_eq!(report.generation, None);
        assert_eq!(issues(&report), vec![Issue::BothSlotsCorrupt]);
    }
}

#[test]
fn foreign_version_slot_is_ignored_not_corrupt() {
    let mut rng = Rng(0x5EED_0004);
    let mut image = vec![0u8; DISK_SLOT_COUNT * SLOT_BYTES];
    write_slot(&mut image, 0, 5, &mut rng);
    write_slot(&mut image, 1, 9, &mut rng);
    // Slot 1 claims an older GALF version: skipped entirely.
    image[SLOT_BYTES + 4..SLOT_BYTES + 6].copy_from_slice(&(DISK_VERSION - 1).to_le_bytes());
    let report = check(&image);
    assert_eq!(report.generation, Some(5));
    assert_eq!(report.slot, Some(0));
    assert!(report.ok);
    // Both foreign: no valid slot, and not "corrupt" either.
    image[4..6].copy_from_slice(&(DISK_VERSION + 1).to_le_bytes());
    let report = check(&image);
    assert_eq!(issues(&report), vec![Issue::NoValidSlot]);
}

#[test]
fn alternating_writes_never_move_the_chosen_generation_backwards() {
    let mut rng = Rng(0x5EED_0005);
    for _ in 0..2 {
        let mut image = vec![0u8; DISK_SLOT_COUNT * SLOT_BYTES];
        let mut generation = rng.next() % 100;
        write_slot(&mut image, 0, generation, &mut rng);
        let mut seen = generation;
        let mut slot = 1usize;
        for step in 0..20 {
            generation += 1 + rng.next() % 3;
            // The kernel writes the slot it did not read from; a torn
            // write leaves the other slot's generation visible.
            write_slot(&mut image, slot, generation, &mut rng);
            let torn = rng.next().is_multiple_of(5);
            if torn {
                let off = slot * SLOT_BYTES + DISK_HEADER + (rng.next() as usize % PAYLOAD_LEN);
                image[off] ^= 0x10;
            }
            let report = check(&image);
            let got = report.generation.expect("one valid slot always remains");
            assert!(got >= seen, "step {step}: {got} < {seen}");
            assert_eq!(got, if torn { seen } else { generation });
            seen = got;
            slot = report.slot.expect("slot") as usize ^ 1;
        }
    }
}
