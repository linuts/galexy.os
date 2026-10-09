//! Integration test: hostile ELF suite (Milestone 51).
//!
//! The `spawn` syscall runs `loader::validate_elf` before the loader
//! touches a page table. Every forged image here must come back as a
//! `SysError` — never a kernel panic — while every real ramdisk program
//! still validates. Mutations are applied to a copy of the real `hello`,
//! so the forgeries are one field away from a program that loads.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::SysError;
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// ELF64 header field offsets.
const E_TYPE: usize = 16;
const E_ENTRY: usize = 24;
const E_PHOFF: usize = 32;
const E_PHENTSIZE: usize = 54;
const E_PHNUM: usize = 56;
/// ELF64 program-header field offsets (relative to the phdr).
const P_TYPE: usize = 0;
const P_FLAGS: usize = 4;
const P_VADDR: usize = 16;
const P_FILESZ: usize = 32;
const P_MEMSZ: usize = 40;
const PHDR_SIZE: usize = 56;
const PT_LOAD: u32 = 1;

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(raw)
}

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Byte offsets of every `PT_LOAD` program header in `elf`.
fn load_phdrs(elf: &[u8]) -> Vec<usize> {
    let phoff = u64_at(elf, E_PHOFF) as usize;
    let phnum = u16_at(elf, E_PHNUM) as usize;
    assert_eq!(u16_at(elf, E_PHENTSIZE) as usize, PHDR_SIZE);
    (0..phnum)
        .map(|i| phoff + i * PHDR_SIZE)
        .filter(|&ph| {
            u32::from_le_bytes([elf[ph], elf[ph + 1], elf[ph + 2], elf[ph + 3]]) == PT_LOAD
        })
        .collect()
}

/// One forgery: `mutate` a copy of `base`, then expect `want`.
fn forge(label: &str, base: &[u8], want: SysError, mutate: impl FnOnce(&mut Vec<u8>)) {
    let mut img = base.to_vec();
    mutate(&mut img);
    let got = sched::loader::validate_elf(&img);
    assert_eq!(
        got,
        Err(want),
        "[test-badelf] {label}: expected {want:?}, got {got:?}"
    );
    serial_println!("[test-badelf] {}: refused with {:?}", label, want);
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-badelf] running");
    serial_println!("[test-badelf] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    let ramdisk_len = boot_info.ramdisk_len as usize;

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    // SAFETY: the bootloader mapped the ramdisk at [ramdisk_addr, +len).
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            ramdisk_len,
        )
    };
    sched::ramdisk::init(archive);

    // Every real program on the ramdisk validates; text files do not.
    let mut names: Vec<alloc::string::String> = Vec::new();
    sched::ramdisk::for_each_name(|n| names.push(n.into()));
    let mut programs = 0;
    for name in &names {
        let bytes = sched::ramdisk::find(name).unwrap();
        if sched::loader::looks_like_elf(bytes) {
            assert_eq!(
                sched::loader::validate_elf(bytes),
                Ok(()),
                "[test-badelf] real program '{name}' must validate"
            );
            programs += 1;
        } else {
            assert_eq!(
                sched::loader::validate_elf(bytes),
                Err(SysError::Unsupported),
                "[test-badelf] text file '{name}' must be Unsupported"
            );
        }
    }
    assert!(programs >= 4, "ramdisk should carry init/shell/util/hello");
    serial_println!("[test-badelf] {} ramdisk program(s) validate", programs);

    let hello = sched::ramdisk::find("hello").expect("hello in ramdisk");
    let loads = load_phdrs(hello);
    assert!(
        loads.len() >= 2,
        "hello needs two PT_LOADs for the overlap case"
    );
    let ph0 = loads[0];
    let ph1 = loads[1];

    // Not an ELF at all.
    assert_eq!(
        sched::loader::validate_elf(b"hello world\n"),
        Err(SysError::Unsupported)
    );
    // Truncated header: the magic is there, the rest is not.
    assert_eq!(
        sched::loader::validate_elf(&hello[..40]),
        Err(SysError::BadValue),
        "truncated header"
    );
    // Truncated inside the program-header table (xmas_elf would slice
    // past the end here if the validator did not check first).
    let phoff = u64_at(hello, E_PHOFF) as usize;
    assert_eq!(
        sched::loader::validate_elf(&hello[..phoff + 10]),
        Err(SysError::BadValue),
        "truncated phdr table"
    );
    serial_println!("[test-badelf] truncated header/table: refused with BadValue");

    forge("filesz > memsz", hello, SysError::BadValue, |img| {
        let memsz = u64_at(img, ph0 + P_MEMSZ);
        put_u64(img, ph0 + P_FILESZ, memsz + 1);
    });
    forge(
        "memsz past the user window",
        hello,
        SysError::BadValue,
        |img| {
            put_u64(img, ph0 + P_MEMSZ, sched::loader::USER_IMAGE_WINDOW);
        },
    );
    forge(
        "memsz wraps the address space",
        hello,
        SysError::BadValue,
        |img| {
            put_u64(img, ph0 + P_MEMSZ, u64::MAX - 0x100);
        },
    );
    forge(
        "filesz past end of file",
        hello,
        SysError::BadValue,
        |img| {
            let len = img.len() as u64;
            put_u64(img, ph0 + P_FILESZ, len);
            put_u64(img, ph0 + P_MEMSZ, len + 0x1000);
        },
    );
    forge("overlapping PT_LOAD", hello, SysError::BadValue, |img| {
        let v0 = u64_at(img, ph0 + P_VADDR);
        put_u64(img, ph1 + P_VADDR, v0);
    });
    forge(
        "segment below the image base",
        hello,
        SysError::BadValue,
        |img| {
            put_u64(img, ph0 + P_VADDR, 0x1000);
        },
    );
    forge(
        "segment in kernel space",
        hello,
        SysError::BadValue,
        |img| {
            put_u64(img, ph0 + P_VADDR, 0xFFFF_8000_0000_0000);
        },
    );
    forge("W|X segment", hello, SysError::BadValue, |img| {
        put_u32(img, ph0 + P_FLAGS, 7);
    });
    forge(
        "phdr table past end of file",
        hello,
        SysError::BadValue,
        |img| {
            let len = img.len() as u64;
            put_u64(img, E_PHOFF, len - 8);
        },
    );
    forge(
        "phdr table offset wraps",
        hello,
        SysError::BadValue,
        |img| {
            put_u64(img, E_PHOFF, u64::MAX - 16);
        },
    );
    forge("no program headers", hello, SysError::BadValue, |img| {
        put_u16(img, E_PHNUM, 0);
    });
    forge("absurd phdr count", hello, SysError::BadValue, |img| {
        put_u16(img, E_PHNUM, u16::MAX);
    });
    forge(
        "entry outside every segment",
        hello,
        SysError::BadValue,
        |img| {
            put_u64(img, E_ENTRY, 0);
        },
    );
    forge(
        "entry in a non-executable segment",
        hello,
        SysError::BadValue,
        |img| {
            // Point the entry at the first non-X PT_LOAD, if any; else at
            // the stack window (outside every segment).
            let mut target = galexy_abi::USER_IMAGE_BASE + sched::loader::USER_IMAGE_WINDOW - 8;
            for &ph in &loads {
                let flags = u32::from_le_bytes([
                    img[ph + P_FLAGS],
                    img[ph + P_FLAGS + 1],
                    img[ph + P_FLAGS + 2],
                    img[ph + P_FLAGS + 3],
                ]);
                if flags & 1 == 0 {
                    target = u64_at(img, ph + P_VADDR);
                    break;
                }
            }
            put_u64(img, E_ENTRY, target);
        },
    );
    forge("ET_DYN (PIE)", hello, SysError::Unsupported, |img| {
        put_u16(img, E_TYPE, 3);
    });
    forge(
        "ET_REL (object file)",
        hello,
        SysError::Unsupported,
        |img| {
            put_u16(img, E_TYPE, 1);
        },
    );
    forge("ELFCLASS32", hello, SysError::Unsupported, |img| {
        img[4] = 1;
    });
    forge("wrong machine", hello, SysError::Unsupported, |img| {
        put_u16(img, 18, 0x28); // EM_ARM
    });
    forge("bad phdr type word", hello, SysError::BadValue, |img| {
        put_u32(img, ph0 + P_TYPE, 0x8000_0000);
    });

    // The pristine program still loads and runs to completion: the gate
    // refused the forgeries, not the real thing.
    let baseline = galexy_os::arch::mm::free_frames();
    let _ = sched::loader::spawn_program("hello", hello).expect("hello elf");
    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
        polls += 1;
        if polls > 4000 {
            panic!("hello never exited");
        }
    }
    let mut stable = 0u32;
    while stable < 16 {
        x86_64::instructions::hlt();
        let before = galexy_os::arch::mm::free_frames();
        sched::reap();
        stable = if galexy_os::arch::mm::free_frames() == before {
            stable + 1
        } else {
            0
        };
    }
    assert_eq!(
        galexy_os::arch::mm::free_frames(),
        baseline,
        "pristine hello must return every frame"
    );

    println!("[test-badelf] hostile ELF suite ok");
    serial_println!("[test-badelf] passed");
    exit_qemu(QemuExitCode::Success);
}
