//! Integration test: negative suite (Milestone 51) — what must NOT work.
//!
//! Two ring-3 blobs report syscall results through their scratch page:
//!
//! 1. `prelogin` — a seat-grade task (launcher grants) with the logged-out
//!    credential. `spawn hello` and `create notes` must be AccessDenied:
//!    the kernel, not the shell's login screen, is the gate.
//! 2. `bare` — a program spawned without `SPAWN_INHERIT`: it keeps the
//!    session root for path context but holds no cards. It can open a
//!    ramdisk file, but every galfs create / open / remove under the
//!    session's own tree is AccessDenied.
//!
//! Both blobs run to `exit`; frames return to baseline.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, SysError, Syscall};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x0000_4E45_4741_5456;
const TICK_TIMEOUT: u64 = 4000;

/// Scratch layout both blobs share: `done`, then (ok, err) pairs.
#[repr(C)]
struct Report {
    done: u64,
    r: [[u64; 2]; 5],
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-negative] running");
    serial_println!("[test-negative] running");

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

    // A real file under admin's Desktop for the bare blob to covet.
    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("admin Desktop");
    let secret = galfs::create_file_under(desktop, "secret").expect("secret file");
    galfs::append_file(secret, b"shh\n").expect("write secret");

    let baseline = mm::free_frames();

    // 1. Logged-out seat: launcher grants, no session root.
    let report = run_blob("prelogin", galfs::unauth_cred(), true, build_prelogin);
    assert_eq!(report.r[0][0], 0, "pre-login spawn must fail");
    assert_eq!(
        report.r[0][1],
        SysError::AccessDenied as u64,
        "pre-login spawn is AccessDenied"
    );
    assert_eq!(report.r[1][0], 0, "pre-login create must fail");
    assert_eq!(report.r[1][1], SysError::AccessDenied as u64);
    serial_println!("[test-negative] pre-login spawn denied");

    // 2. Bare program: session root, no cards.
    let bare = galfs::FsCred {
        root: admin,
        tokens: [galfs::Token::empty(); galfs::TOKEN_SLOTS],
    };
    let report = run_blob("bare", bare, false, build_bare);
    assert_eq!(report.r[0][0], 1, "bare program may open a ramdisk file");
    assert_eq!(report.r[1][0], 0, "bare create under Desktop must fail");
    assert_eq!(report.r[1][1], SysError::AccessDenied as u64);
    assert_eq!(report.r[2][0], 0, "bare open of Desktop/secret must fail");
    assert_eq!(report.r[2][1], SysError::AccessDenied as u64);
    assert_eq!(report.r[3][0], 0, "bare remove of Desktop/secret must fail");
    assert_eq!(report.r[3][1], SysError::AccessDenied as u64);
    assert_eq!(report.r[4][0], 0, "bare create at the root must fail");
    assert_eq!(report.r[4][1], SysError::AccessDenied as u64);
    serial_println!("[test-negative] bare spawn cannot touch galfs");

    // The secret is intact and still exactly one child was added.
    let mut buf = [0u8; 16];
    let n = galfs::read_file_bytes(secret, &mut buf).expect("secret still there");
    assert_eq!(&buf[..n], b"shh\n");
    assert!(galfs::find_under(desktop, "evil").is_none());
    assert_eq!(
        mm::free_frames(),
        baseline,
        "both blobs must return every frame"
    );

    println!("[test-negative] negative suite ok");
    serial_println!("[test-negative] passed");
    exit_qemu(QemuExitCode::Success);
}

/// Spawns one blob, waits for its DONE mark, reaps it, returns the report.
fn run_blob(
    name: &str,
    fs: galfs::FsCred,
    launcher: bool,
    build: impl FnOnce(u64, u64) -> alloc::vec::Vec<u8>,
) -> Report {
    let make = |gr: sched::UserRegion| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build(gr.code.as_u64(), gr.scratch.as_u64())
    };
    let (region, _) = if launcher {
        sched::spawn_user_launcher_with(name, fs, make)
    } else {
        sched::spawn_user_with(name, fs, make)
    };
    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    x86_64::instructions::interrupts::enable();
    let report = loop {
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("{name} blob never finished");
        }
    };
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }
    report
}

/// Embeds `strings` after a short jump; returns their user addresses.
fn embed<const N: usize>(
    code: &mut alloc::vec::Vec<u8>,
    code_base: u64,
    strings: [&[u8]; N],
) -> [u64; N] {
    let total: usize = strings.iter().map(|s| s.len()).sum();
    code.push(0xEB);
    code.push(total as u8);
    let mut addrs = [0u64; N];
    let mut at = code_base + 2;
    for (i, s) in strings.iter().enumerate() {
        addrs[i] = at;
        code.extend_from_slice(s);
        at += s.len() as u64;
    }
    addrs
}

fn build_prelogin(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    let [hello, notes] = embed(&mut code, code_base, [b"hello", b"notes"]);
    mov_r64_imm(&mut code, 15, scratch);

    // spawn hello with the real loader Cap (the seat has the grant).
    let loader = reserved::loader(CapRights::EXEC).bits();
    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, hello);
    mov_r64_imm(&mut code, 2, 5);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, 0);
    syscall(&mut code);
    store(&mut code, 2, 0x08);
    store(&mut code, 0, 0x10);

    // create notes in the (absent) session tree.
    mov_eax(&mut code, Syscall::Create as u32);
    mov_r64_imm(&mut code, 7, notes);
    mov_r64_imm(&mut code, 6, 5);
    mov_r64_imm(&mut code, 2, 0);
    syscall(&mut code);
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);

    finish(&mut code);
    code
}

fn build_bare(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    let [banner, evil, secret, top] = embed(
        &mut code,
        code_base,
        [b"banner.txt", b"Desktop/evil", b"Desktop/secret", b"loot"],
    );
    mov_r64_imm(&mut code, 15, scratch);

    // open banner.txt (ramdisk) — allowed.
    mov_eax(&mut code, Syscall::Open as u32);
    mov_r64_imm(&mut code, 7, banner);
    mov_r64_imm(&mut code, 6, 10);
    syscall(&mut code);
    store(&mut code, 2, 0x08);
    store(&mut code, 0, 0x10);

    // create Desktop/evil — no CREATE card.
    mov_eax(&mut code, Syscall::Create as u32);
    mov_r64_imm(&mut code, 7, evil);
    mov_r64_imm(&mut code, 6, 12);
    mov_r64_imm(&mut code, 2, 0);
    syscall(&mut code);
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);

    // open Desktop/secret — no READ card.
    mov_eax(&mut code, Syscall::Open as u32);
    mov_r64_imm(&mut code, 7, secret);
    mov_r64_imm(&mut code, 6, 14);
    syscall(&mut code);
    store(&mut code, 2, 0x28);
    store(&mut code, 0, 0x30);

    // remove Desktop/secret — no REMOVE card.
    mov_eax(&mut code, Syscall::Remove as u32);
    mov_r64_imm(&mut code, 7, secret);
    mov_r64_imm(&mut code, 6, 14);
    syscall(&mut code);
    store(&mut code, 2, 0x38);
    store(&mut code, 0, 0x40);

    // create loot at the session root — no CREATE card there either.
    mov_eax(&mut code, Syscall::Create as u32);
    mov_r64_imm(&mut code, 7, top);
    mov_r64_imm(&mut code, 6, 4);
    mov_r64_imm(&mut code, 2, 0);
    syscall(&mut code);
    store(&mut code, 2, 0x48);
    store(&mut code, 0, 0x50);

    finish(&mut code);
    code
}

fn finish(code: &mut alloc::vec::Vec<u8>) {
    mov_r64_imm(code, 0, DONE);
    store(code, 0, 0x00);
    mov_eax(code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
}

fn syscall(code: &mut alloc::vec::Vec<u8>) {
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn mov_r64_imm(code: &mut alloc::vec::Vec<u8>, reg: u8, imm: u64) {
    let rex = 0x48 | u8::from(reg >= 8);
    code.push(rex);
    code.push(0xB8 + (reg & 7));
    code.extend_from_slice(&imm.to_le_bytes());
}

fn mov_eax(code: &mut alloc::vec::Vec<u8>, imm: u32) {
    code.push(0xB8);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `mov [r15 + disp], reg`.
fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
