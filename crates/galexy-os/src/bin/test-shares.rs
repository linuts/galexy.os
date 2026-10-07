//! Integration test: durable home shares (GALF v10).

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
    println!("[test-shares] running");
    serial_println!("[test-shares] running");

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

    let admin = galfs::admin_root();
    let admin_cred = galfs::FsCred::launcher(admin);

    galfs::add_user("dan", b"dan-pass").expect("add dan");
    galfs::add_user("eve", b"eve-pass").expect("add eve");
    let dan = galfs::root_named("dan").expect("dan root");
    let desk = galfs::find_under(dan, "Desktop").expect("dan Desktop");
    let secret = galfs::create_file_under(desk, "secret").expect("secret file");
    let _ = galfs::append_file(secret, b"hi").expect("write secret");

    // Without a share, eve's home ALL does not cover dan's tree.
    let mut eve_toks = [Token::empty(); galfs::TOKEN_SLOTS];
    let eve = galfs::root_named("eve").expect("eve root");
    galfs::push_token(&mut eve_toks, eve, galfs::RIGHT_ALL).expect("eve home");
    assert!(
        matches!(
            galfs::open_file(eve, &eve_toks, "dan@Desktop/secret"),
            Err(SysError::AccessDenied)
        ),
        "eve must not read dan without a share"
    );

    galfs::add_share(
        admin,
        &admin_cred.tokens,
        "dan@Desktop",
        galfs::RIGHT_LIST | galfs::RIGHT_READ,
        "eve",
    )
    .expect("admin share Desktop to eve");

    // Fresh session: home ALL + durable shares (login path).
    eve_toks = [Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut eve_toks, eve, galfs::RIGHT_ALL).expect("eve home again");
    galfs::apply_shares(eve, &mut eve_toks).expect("apply shares");
    let opened = galfs::open_file(eve, &eve_toks, "dan@Desktop/secret").expect("shared open");
    assert_eq!(opened, secret);

    galfs::remove_share(
        admin,
        &admin_cred.tokens,
        "dan@Desktop",
        galfs::RIGHT_LIST | galfs::RIGHT_READ,
        "eve",
    )
    .expect("unshare");

    eve_toks = [Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut eve_toks, eve, galfs::RIGHT_ALL).expect("eve home post-unshare");
    galfs::apply_shares(eve, &mut eve_toks).expect("apply after unshare");
    assert!(
        matches!(
            galfs::open_file(eve, &eve_toks, "dan@Desktop/secret"),
            Err(SysError::AccessDenied)
        ),
        "unshare must stop login re-apply"
    );

    // userdel clears durable shares naming the deleted actor.
    galfs::add_share(
        admin,
        &admin_cred.tokens,
        "dan@Desktop",
        galfs::RIGHT_READ,
        "eve",
    )
    .expect("re-share for userdel");
    galfs::remove_user("eve").expect("userdel eve");
    galfs::add_user("eve", b"eve-pass").expect("re-add eve");
    let eve2 = galfs::root_named("eve").expect("eve root again");
    eve_toks = [Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut eve_toks, eve2, galfs::RIGHT_ALL).expect("new eve home");
    galfs::apply_shares(eve2, &mut eve_toks).expect("apply after userdel");
    assert!(
        matches!(
            galfs::open_file(eve2, &eve_toks, "dan@Desktop/secret"),
            Err(SysError::AccessDenied)
        ),
        "userdel must drop durable shares for that actor"
    );

    println!("[test-shares] durable shares reapply and clear");
    serial_println!("[test-shares] passed");
    exit_qemu(QemuExitCode::Success);
}
