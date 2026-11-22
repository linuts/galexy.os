//! Offline checker for sealed GALF dual-slot images (`galfs.img`).
//!
//! Usage:
//!   galfs-fsck <path> [--passphrase <str>] [--repair]
//!
//! Exit 0 when the newest valid slot passes structural checks. With
//! `--repair`, a newest slot whose only problems are bad share rows is
//! sealed into the other slot at the next generation and the file is
//! rewritten. Any other problem is left on disk. Exit 1 on problems
//! that were not repaired; 2 on usage / I/O errors.

use galexy_galf::{
    check_image, repair_image, Issue, RepairStop, SealNonces, Table, DEFAULT_VOLUME_PASSPHRASE,
    DISK_SECTORS, DISK_SLOT_COUNT, SECTOR,
};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: galfs-fsck <galfs.img> [--passphrase <str>] [--repair]");
        return ExitCode::from(2);
    };
    let mut passphrase = DEFAULT_VOLUME_PASSPHRASE.to_vec();
    let mut repair = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--passphrase" => {
                let Some(p) = args.next() else {
                    eprintln!("galfs-fsck: --passphrase needs a value");
                    return ExitCode::from(2);
                };
                passphrase = p.into_bytes();
            }
            "--repair" => repair = true,
            other => {
                eprintln!("galfs-fsck: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
    }

    let image = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("galfs-fsck: read {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let need = DISK_SLOT_COUNT * DISK_SECTORS * SECTOR;
    if image.len() < need {
        eprintln!(
            "galfs-fsck: image too short ({} < {} bytes)",
            image.len(),
            need
        );
        return ExitCode::from(1);
    }

    let mut slot_buf = vec![0u8; DISK_SECTORS * SECTOR];
    let mut best = Box::new(Table::empty());
    let mut cand = Box::new(Table::empty());
    let mut image = image;
    if repair {
        let mut nonces = SealNonces {
            kdf_salt: [0; 8],
            wrap_nonce: [0; 12],
            data_nonce: [0; 12],
        };
        // Host repair draws fixed public nonces from the path length and
        // a counter. These are not volume secrets: the slot still wraps
        // the existing volume key under the passphrase KDF.
        let seed = (path.len() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        fill_nonces(&mut nonces, seed);
        match repair_image(
            &mut image,
            &passphrase,
            &nonces,
            &mut slot_buf,
            &mut best,
            &mut cand,
        ) {
            Ok(Some(done)) => {
                if let Err(e) = std::fs::write(&path, &image) {
                    eprintln!("galfs-fsck: write {path}: {e}");
                    return ExitCode::from(2);
                }
                println!(
                    "repaired slot {} generation {} cleared {}",
                    done.slot, done.generation, done.cleared_shares
                );
            }
            Ok(None) => {}
            Err(RepairStop::Unrepairable) => {}
            Err(RepairStop::NoValidSlot) | Err(RepairStop::BothSlotsCorrupt) => {}
            Err(RepairStop::Encode) => {
                eprintln!("galfs-fsck: could not seal the repaired slot");
                return ExitCode::from(1);
            }
        }
    }
    let report = check_image(&image, &passphrase, &mut slot_buf, &mut best, &mut cand);

    if let (Some(slot), Some(gen)) = (report.slot, report.generation) {
        println!("slot {slot} generation {gen}");
    }
    if report.ok {
        println!("ok");
        ExitCode::SUCCESS
    } else {
        for issue in &report.issues[..report.issue_count] {
            print_issue(*issue);
        }
        println!("FAIL ({} issue(s))", report.issue_count);
        ExitCode::from(1)
    }
}

fn fill_nonces(nonces: &mut SealNonces, mut seed: u64) {
    for b in nonces
        .kdf_salt
        .iter_mut()
        .chain(nonces.wrap_nonce.iter_mut())
        .chain(nonces.data_nonce.iter_mut())
    {
        seed = seed.wrapping_mul(0x6C07_8965).wrapping_add(0xB504_F333);
        *b = (seed >> 33) as u8;
    }
}

fn print_issue(issue: Issue) {
    match issue {
        Issue::NoValidSlot => println!("error: no_valid_slot"),
        Issue::BothSlotsCorrupt => println!("error: both_slots_corrupt"),
        Issue::MissingAdmin => println!("error: missing_admin"),
        Issue::BadActorRoot { actor } => println!("error: bad_actor_root actor={actor}"),
        Issue::BadObjectKind { object } => println!("error: bad_object_kind object={object}"),
        Issue::OrphanObject { object } => println!("error: orphan_object object={object}"),
        Issue::BadParent { object } => println!("error: bad_parent object={object}"),
        Issue::Cycle { object } => println!("error: cycle object={object}"),
        Issue::BadActorRef { object } => println!("error: bad_actor_ref object={object}"),
        Issue::BlockLeak { block } => println!("error: block_leak block={block}"),
        Issue::BlockDuplicate { block } => println!("error: block_duplicate block={block}"),
        Issue::BlockMissing { block } => println!("error: block_missing block={block}"),
        Issue::FileTooLarge { object } => println!("error: file_too_large object={object}"),
        Issue::DirHasLength { object } => println!("error: dir_has_length object={object}"),
        Issue::BadShare { share } => println!("error: bad_share share={share}"),
    }
}
