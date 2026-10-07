//! Offline checker for sealed GALF dual-slot images (`galfs.img`).
//!
//! Usage:
//!   galfs-fsck <path> [--passphrase <str>]
//!
//! Exit 0 when the newest valid slot passes structural checks; 1 on
//! problems; 2 on usage / I/O errors.

use galexy_galf::{
    check_image, Issue, Table, DEFAULT_VOLUME_PASSPHRASE, DISK_SECTORS, DISK_SLOT_COUNT, SECTOR,
};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: galfs-fsck <galfs.img> [--passphrase <str>]");
        return ExitCode::from(2);
    };
    let mut passphrase = DEFAULT_VOLUME_PASSPHRASE.to_vec();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--passphrase" => {
                let Some(p) = args.next() else {
                    eprintln!("galfs-fsck: --passphrase needs a value");
                    return ExitCode::from(2);
                };
                passphrase = p.into_bytes();
            }
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
    let report = check_image(&image, &passphrase, &mut slot_buf, &mut best, &mut cand);

    if let (Some(slot), Some(gen)) = (report.slot, report.generation) {
        println!("slot {slot} generation {gen}");
    }
    if report.ok {
        println!("ok");
        ExitCode::SUCCESS
    } else {
        for i in 0..report.issue_count {
            print_issue(report.issues[i]);
        }
        println!("FAIL ({} issue(s))", report.issue_count);
        ExitCode::from(1)
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
