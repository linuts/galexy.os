//! Integration test: confused-deputy share rules and token/share slot caps.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::SysError;
use galexy_os::sched::galfs::{self, Token};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-cards] running");
    serial_println!("[test-cards] running");

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

    token_slot_exhaustion();
    share_slot_exhaustion();
    confused_deputy_share();

    println!("[test-cards] deputy rules and slot caps ok");
    serial_println!("[test-cards] passed");
    exit_qemu(QemuExitCode::Success);
}

fn token_slot_exhaustion() {
    assert_eq!(galfs::TOKEN_SLOTS, 8, "document: 8 tokens per task");
    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");

    let mut objs = [0u16; galfs::TOKEN_SLOTS];
    for (i, slot) in objs.iter_mut().enumerate() {
        let mut name = [b'a'; 2];
        name[1] = b'0' + i as u8;
        let label = core::str::from_utf8(&name).unwrap();
        *slot = galfs::create_file_under(desktop, label).expect("token file");
    }

    let mut tokens = [Token::empty(); galfs::TOKEN_SLOTS];
    for &obj in &objs {
        galfs::push_token(&mut tokens, obj, galfs::RIGHT_READ).expect("fill token");
    }
    let extra = galfs::create_file_under(desktop, "extra").expect("extra");
    assert!(
        matches!(
            galfs::push_token(&mut tokens, extra, galfs::RIGHT_READ),
            Err(SysError::NoResource)
        ),
        "full token table is NoResource"
    );

    galfs::revoke_token(&mut tokens, objs[0], galfs::RIGHT_READ).expect("free slot");
    galfs::push_token(&mut tokens, extra, galfs::RIGHT_READ).expect("reuse freed slot");

    // Cleanup for later tests — remove created files.
    for i in 0..galfs::TOKEN_SLOTS {
        let mut path = [0u8; 16];
        path[..8].copy_from_slice(b"Desktop/");
        path[8] = b'a';
        path[9] = b'0' + i as u8;
        let p = core::str::from_utf8(&path[..10]).unwrap();
        galfs::remove_as_admin(p).expect("rm token file");
    }
    galfs::remove_as_admin("Desktop/extra").expect("rm extra");
}

fn share_slot_exhaustion() {
    assert_eq!(galfs::SHARE_SLOTS, 32, "document: 32 durable shares");
    let admin = galfs::admin_root();
    let admin_cred = galfs::FsCred::launcher(admin);
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");
    galfs::add_user("eve", b"eve-pass").expect("add eve");

    let mut names = [[0u8; 3]; galfs::SHARE_SLOTS];
    for i in 0..galfs::SHARE_SLOTS {
        names[i][0] = b's';
        names[i][1] = b'0' + ((i / 10) as u8);
        names[i][2] = b'0' + ((i % 10) as u8);
        let label = core::str::from_utf8(&names[i]).unwrap();
        galfs::create_file_under(desktop, label).expect("share file");
        let mut path = [0u8; 16];
        path[..8].copy_from_slice(b"Desktop/");
        path[8..11].copy_from_slice(&names[i]);
        let p = core::str::from_utf8(&path[..11]).unwrap();
        galfs::add_share(
            admin,
            &admin_cred.tokens,
            p,
            galfs::RIGHT_READ,
            "eve",
        )
        .expect("fill share");
    }

    let overflow = galfs::create_file_under(desktop, "sxx").expect("overflow file");
    let _ = overflow;
    assert!(
        matches!(
            galfs::add_share(
                admin,
                &admin_cred.tokens,
                "Desktop/sxx",
                galfs::RIGHT_READ,
                "eve",
            ),
            Err(SysError::NoResource)
        ),
        "full share table is NoResource"
    );

    galfs::remove_share(
        admin,
        &admin_cred.tokens,
        "Desktop/s00",
        galfs::RIGHT_READ,
        "eve",
    )
    .expect("free share");
    galfs::add_share(
        admin,
        &admin_cred.tokens,
        "Desktop/sxx",
        galfs::RIGHT_READ,
        "eve",
    )
    .expect("reuse freed share");

    // Drop eve (and shares naming her) so later tests start clean.
    // Files under admin remain; userdel eve requires empty eve tree only.
    galfs::remove_user("eve").expect("userdel eve");
    galfs::remove_as_admin("Desktop/sxx").expect("rm sxx");
    for name in &names {
        let label = core::str::from_utf8(name).unwrap();
        let mut path = [0u8; 16];
        path[..8].copy_from_slice(b"Desktop/");
        path[8..11].copy_from_slice(name);
        let p = core::str::from_utf8(&path[..11]).unwrap();
        // Shares on these objects were cleared with eve; remove files.
        galfs::remove_as_admin(p).expect("rm share file");
    }
}

fn confused_deputy_share() {
    galfs::add_user("dan", b"dan-pass").expect("add dan");
    galfs::add_user("eve", b"eve-pass").expect("add eve");
    let dan = galfs::root_named("dan").expect("dan");
    let desk = galfs::find_under(dan, "Desktop").expect("dan Desktop");

    // LIST-only card: may share LIST, must not share WRITE.
    let mut toks = [Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut toks, desk, galfs::RIGHT_LIST).expect("list card");
    assert!(
        matches!(
            galfs::add_share(dan, &toks, "Desktop", galfs::RIGHT_WRITE, "eve"),
            Err(SysError::AccessDenied)
        ),
        "LIST-only must not share WRITE"
    );
    galfs::add_share(dan, &toks, "Desktop", galfs::RIGHT_LIST, "eve")
        .expect("LIST-only may share LIST");

    // Cannot mint a share on a tree the caller cannot cover.
    assert!(
        matches!(
            galfs::add_share(
                dan,
                &toks,
                "admin@Desktop",
                galfs::RIGHT_LIST,
                "eve",
            ),
            Err(SysError::AccessDenied)
        ),
        "must not share a path without a covering card"
    );

    // Unresolvable path.
    assert!(
        matches!(
            galfs::add_share(dan, &toks, "Desktop/missing", galfs::RIGHT_LIST, "eve"),
            Err(SysError::NotFound)
        ),
        "missing path is NotFound"
    );
}
