//! Integration test: durable home shares survive a disk-backed reboot.
//!
//! Boot 1: create dan/eve, share dan's Desktop (LIST|READ) to eve, sync.
//! Boot 2: load image, apply_shares for eve, prove the card is installed.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::sched::galfs::{self, Token};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn has_card(tokens: &[Token; galfs::TOKEN_SLOTS], object: u16, rights: u8) -> bool {
    tokens
        .iter()
        .any(|t| t.is_live() && t.object == object && t.rights & rights == rights)
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-share-disk] running");
    serial_println!("[test-share-disk] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    assert!(
        galfs::disk_backed(),
        "ATA slave must back galfs for this test"
    );

    let rights = galfs::RIGHT_LIST | galfs::RIGHT_READ;

    // Boot 2: actors from the sealed image; durable share must re-apply.
    if let Ok(eve) = galfs::root_named("eve") {
        let dan = galfs::root_named("dan").expect("dan after reboot");
        let desk = galfs::find_under(dan, "Desktop").expect("dan Desktop");
        let mut toks = [Token::empty(); galfs::TOKEN_SLOTS];
        galfs::push_token(&mut toks, eve, galfs::RIGHT_ALL).expect("eve home");
        assert!(
            !has_card(&toks, desk, rights),
            "home ALL alone must not cover dan Desktop"
        );
        galfs::apply_shares(eve, &mut toks).expect("apply durable shares");
        assert!(
            has_card(&toks, desk, rights),
            "durable share must survive reboot"
        );
        println!("[test-share-disk] share survived reboot");
        serial_println!("[test-share-disk] passed");
        exit_qemu(QemuExitCode::Success);
    }

    // Boot 1: record a durable share and sync to the IDE slave.
    let admin = galfs::admin_root();
    let admin_cred = galfs::FsCred::launcher(admin);
    galfs::add_user("dan", b"dan-pass").expect("add dan");
    galfs::add_user("eve", b"eve-pass").expect("add eve");
    let dan = galfs::root_named("dan").expect("dan");
    let desk = galfs::find_under(dan, "Desktop").expect("dan Desktop");
    let secret = galfs::create_file_under(desk, "secret").expect("secret");
    let _ = galfs::append_file(secret, b"share-disk-marker").expect("write");

    galfs::add_share(admin, &admin_cred.tokens, "dan@Desktop", rights, "eve")
        .expect("share Desktop to eve");
    galfs::sync();

    // Sanity on this boot before reboot.
    let eve = galfs::root_named("eve").expect("eve");
    let mut toks = [Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut toks, eve, galfs::RIGHT_ALL).expect("eve home");
    galfs::apply_shares(eve, &mut toks).expect("apply before reboot");
    assert!(has_card(&toks, desk, rights), "share applies before reboot");

    println!("[test-share-disk] wrote share to disk");
    serial_println!("[test-share-disk] wrote");
    exit_qemu(QemuExitCode::Success);
}
