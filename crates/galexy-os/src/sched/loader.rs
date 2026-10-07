//! Program loader: real ELF64 user programs from the ramdisk tar.
//!
//! Contract (see `docs/DESIGN.md`, boundary rule 6 + the ABI's
//! `USER_IMAGE_BASE`): programs are STATIC, NON-PIE ELF64 executables
//! linked at `galexy_abi::USER_IMAGE_BASE`. The loader maps every PT_LOAD
//! segment into the task's own (fresh, non-active) tree with per-segment
//! flags (RX/RW + always USER|PRESENT), zeros `p_memsz > p_filesz` tails,
//! and enters at `e_entry`. Blob-based `spawn_user_task` remains the
//! test-side path; both feed the same rotation + lifecycle.

use alloc::vec;

use x86_64::instructions::interrupts;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};
use xmas_elf::program::{ProgramHeader, SegmentData, Type};
use xmas_elf::ElfFile;

use crate::arch::mm;
use crate::sched::context;
use crate::sched::{
    register_user_task, Grants, TaskInit, THREAD_STACK_SIZE, USER_STACK_OFFSET, USER_STACK_PAGES,
};
use crate::serial_println;

/// The result of loading a program.
#[derive(Debug, Clone, Copy)]
pub struct ProgramRegion {
    /// Where the image was mapped (== `USER_IMAGE_BASE`).
    pub image: VirtAddr,
    /// Entry point (`e_entry`, kernel-validated).
    pub entry: VirtAddr,
    /// RW scratch page in the task's own space (user side), plus its
    /// backing frame's physical address (kernel-side pollers).
    pub scratch: VirtAddr,
    pub scratch_phys: PhysAddr,
}

/// True when `bytes` starts with the ELF magic. A ramdisk text file does not.
pub fn looks_like_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
}

/// Loads `bytes` as a static ELF64 program and spawns the task running it.
///
/// The task is granted the console only. Same rotation/lifecycle as every
/// task: kernel stack via TSS.RSP0, CR3 own tree, tombstone + tree-walk
/// reaping. Owner CPU round-robins.
pub fn spawn_program(name: &str, bytes: &[u8]) -> ProgramRegion {
    spawn_program_placed(
        name,
        bytes,
        None,
        false,
        Grants::console(),
        &[],
        0,
        crate::sched::galfs::admin_cred(),
    )
}

/// Like [`spawn_program`], with an explicit grant set, a startup argument,
/// a console index, and galfs credentials inherited from the parent.
///
/// The argument is placed above the child's initial stack pointer. `rdi`
/// is its user address and `rsi` is the length. An empty slice passes zeros.
pub(crate) fn spawn_launched(
    name: &str,
    bytes: &[u8],
    grants: Grants,
    arg: &[u8],
    tty: u8,
    fs: crate::sched::galfs::FsCred,
) -> ProgramRegion {
    spawn_program_placed(name, bytes, None, false, grants, arg, tty, fs)
}

/// Like [`spawn_program`], pinned to the BSP, and idle CPUs do not steal it.
///
/// F1's shell lives here. Each F-key shell is pinned the same way: the
/// framebuffer has one painter, and that painter is the BSP. The shell
/// receives the launcher grant (console, keyboard, loader, queries, power),
/// admin's root token, and writes the console `tty` names.
pub fn spawn_program_bsp(name: &str, bytes: &[u8]) -> ProgramRegion {
    spawn_shell_on(name, bytes, 0)
}

/// Pins a shell named `name` to the BSP on console `tty`.
///
/// Every seat starts **logged out** (console + keyboard only). `login`
/// installs the session; `logout` returns to this state.
pub fn spawn_shell_on(name: &str, bytes: &[u8], tty: u8) -> ProgramRegion {
    spawn_program_placed(
        name,
        bytes,
        Some(0),
        true,
        Grants::pre_login(),
        &[],
        tty,
        crate::sched::galfs::unauth_cred(),
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_program_placed(
    name: &str,
    bytes: &[u8],
    owner: Option<u8>,
    no_steal: bool,
    grants: Grants,
    arg: &[u8],
    tty: u8,
    fs: crate::sched::galfs::FsCred,
) -> ProgramRegion {
    let elf = ElfFile::new(bytes).expect("spawn_program: invalid ELF");
    // Only static executables: relocatable/DYN would need relocation work.
    match elf.header.pt2.type_().as_type() {
        xmas_elf::header::Type::Executable => {}
        other => panic!("spawn_program: unsupported ELF type {other:?} (static EXEC only)"),
    }
    let entry_vaddr = elf.header.pt2.entry_point();

    interrupts::without_interrupts(|| {
        // The loader allocates. A syscall runs with interrupts off, so the
        // load stays on the main loop, which is the kernel table.
        assert!(
            mm::on_kernel_tree(),
            "spawn_program: must run on the kernel tree (main-loop context)"
        );
        let fresh = mm::FreshL4::new().expect("no frame for a fresh task table");
        let root = fresh.frame;
        let image = VirtAddr::new(galexy_abi::USER_IMAGE_BASE);
        let entry = VirtAddr::new(entry_vaddr);
        assert!(
            entry >= image && entry.as_u64() < image.as_u64() + 512 * 1024 * 1024,
            "spawn_program: entry {:#x} outside the image region",
            entry.as_u64()
        );

        // Map every PT_LOAD segment into the task's tree.
        // SAFETY: a FreshL4 root: coherent, freshly cloned, not active.
        unsafe {
            mm::with_table(root, |mapper| {
                for ph in elf.program_iter() {
                    match ph.get_type() {
                        Ok(Type::Load) => map_segment(mapper, &elf, &ph),
                        // The rest of the phdr types are irrelevant for
                        // static freestanding binaries.
                        Ok(_) => {}
                        Err(e) => panic!("spawn_program: bad phdr: {e:?}"),
                    }
                }
            });
        }

        // Task-private stack + scratch (stack at +1 GiB with its guard
        // fence of absence below; scratch right above the stack pages).
        let stack_base = image + USER_STACK_OFFSET;
        let scratch = stack_base + (USER_STACK_PAGES * 4096) as u64 + 4096;
        let mut stack_frames: [PhysFrame<Size4KiB>; USER_STACK_PAGES] =
            [PhysFrame::from_start_address(PhysAddr::new(0)).unwrap(); USER_STACK_PAGES];
        for slot in &mut stack_frames {
            let frame = mm::allocate_frame().expect("no frame for user stack");
            *slot = frame;
        }
        let scratch_frame = mm::allocate_frame().expect("no frame for user scratch");
        // SAFETY: allocator-owned frame (exclusive per contract).
        unsafe {
            core::ptr::write_bytes(
                mm::frame_virt(scratch_frame.start_address()).as_mut_ptr::<u64>(),
                0,
                512,
            );
        }
        // SAFETY: fresh tree, not active.
        unsafe {
            mm::with_table(root, |mapper| {
                for (i, frame) in stack_frames.iter().enumerate() {
                    let page = Page::containing_address(stack_base + (i * 4096) as u64);
                    mapper
                        .map_to(
                            page,
                            *frame,
                            PageTableFlags::PRESENT
                                | PageTableFlags::WRITABLE
                                | PageTableFlags::USER_ACCESSIBLE
                                | PageTableFlags::NO_EXECUTE,
                            &mut mm::TaskFrameAlloc,
                        )
                        .expect("user stack map failed")
                        .flush();
                }
                let scratch_page = Page::containing_address(scratch);
                mapper
                    .map_to(
                        scratch_page,
                        scratch_frame,
                        PageTableFlags::PRESENT
                            | PageTableFlags::WRITABLE
                            | PageTableFlags::USER_ACCESSIBLE
                            | PageTableFlags::NO_EXECUTE,
                        &mut mm::TaskFrameAlloc,
                    )
                    .expect("user scratch map failed")
                    .flush();
            });
        }

        // Initial ring-3 frame: fabricated via the phys-map image of the
        // top stack page; RIP = the ELF's entry.
        let stack_top = (stack_base + (USER_STACK_PAGES * 4096) as u64).as_u64() & !0xF;
        debug_assert!(stack_top.is_multiple_of(4096));
        let fab_vaddr = mm::frame_virt(stack_frames[USER_STACK_PAGES - 1].start_address()) + 4096;
        let (arg_user, arg_len) = place_arg(fab_vaddr.as_u64(), stack_top, arg);
        let (cs, ss) = context::user_cs_ss();
        let ctx = unsafe {
            context::init_user_frame(
                fab_vaddr.as_u64(),
                stack_top - 512, // user RSP (user-space address!)
                entry.as_u64(),
                arg_user,
                arg_len,
                cs,
                ss,
            )
        };

        // Kernel-mode stack for ring 3→0 crossings: heap-backed; the
        // scheduler paints the canary on registration.
        let kstack = vec![0u8; THREAD_STACK_SIZE];
        let kstack_top = (kstack.as_ptr() as u64 + kstack.len() as u64) & !0xF;

        register_user_task(TaskInit {
            name,
            ctx,
            kstack,
            kstack_top,
            cr3: root.start_address().as_u64(),
            user_p4: p4_index_of(image),
            owner,
            no_steal,
            grants,
            tty,
            fs,
        });
        serial_println!(
            "[loader] program '{}' ready (own tree cr3={:#x}, entry {:#x})",
            name,
            root.start_address().as_u64(),
            entry.as_u64()
        );

        ProgramRegion {
            image,
            entry,
            scratch,
            scratch_phys: scratch_frame.start_address(),
        }
    })
}

/// Copies `arg` into the gap above the child's initial stack pointer and
/// below the fabricated context frame. Returns `(user address, length)`.
///
/// Stack growth moves away from this gap, so `_start` can copy the bytes
/// before `main`. An empty argument returns `(0, 0)`.
fn place_arg(fab_top: u64, stack_top: u64, arg: &[u8]) -> (u64, u64) {
    let n = arg.len().min(crate::sched::ARG_MAX);
    if n == 0 {
        return (0, 0);
    }
    let arg_user = stack_top - context::FABRICATED_FRAME_BYTES - crate::sched::ARG_MAX as u64;
    debug_assert!(
        arg_user >= stack_top - 512,
        "argument overlaps the user stack"
    );
    let dst = (fab_top - (stack_top - arg_user)) as *mut u8;
    // SAFETY: `dst` is the phys-map image of the fresh stack page, in the
    // gap the fabricated frame does not occupy. The page is exclusively owned.
    unsafe {
        core::ptr::write_bytes(dst, 0, crate::sched::ARG_MAX);
        core::ptr::copy_nonoverlapping(arg.as_ptr(), dst, n);
    }
    (arg_user, n as u64)
}

/// Maps one PT_LOAD segment: frames per 4 KiB page covering
/// `[p_vaddr, p_vaddr + p_memsz)`, file contents copied for the first
/// `p_filesz` bytes, zeroed beyond (BSS) + page slack.
///
/// # Safety
///
/// `mapper` must be over a coherent, non-active task tree.
unsafe fn map_segment(mapper: &mut OffsetPageTable<'static>, elf: &ElfFile, ph: &ProgramHeader) {
    let vaddr = VirtAddr::new(ph.virtual_addr());
    let memsz = ph.mem_size();
    let filesz = ph.file_size();
    if memsz == 0 {
        return;
    }

    // STRICT: every segment's pages must live under the task's own P4
    // entry (USER_IMAGE_BASE's entry). Anything else would map through
    // kernel-shared page-table subtrees — rejected loudly, never silently
    // polluting the shared tables.
    let own_p4 = (galexy_abi::USER_IMAGE_BASE >> 39) as usize;
    let seg_first_p4 = (vaddr.as_u64() >> 39) as usize;
    let seg_last_p4 = ((vaddr + memsz - 1).as_u64() >> 39) as usize;
    assert!(
        seg_first_p4 == own_p4 && seg_last_p4 == own_p4,
        "loader: segment {:#x}..{:#x} must sit under P4 entry {}",
        vaddr.as_u64(),
        vaddr.as_u64() + memsz,
        own_p4
    );
    assert!(
        vaddr.as_u64() < 0x0000_8000_0000_0000,
        "loader: segment vaddr {:#x} is kernel space",
        vaddr.as_u64()
    );

    // Flags: R (PRESENT) + W (WRITABLE) + X (no NO_EXECUTE); always USER.
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | (if ph.flags().is_write() {
            PageTableFlags::WRITABLE
        } else {
            PageTableFlags::empty()
        })
        | (if ph.flags().is_execute() {
            PageTableFlags::empty()
        } else {
            PageTableFlags::NO_EXECUTE
        });

    let data = match ph.get_data(elf) {
        Ok(SegmentData::Undefined(bytes)) => bytes,
        Ok(_) => panic!("loader: unexpected segment data kind"),
        Err(e) => panic!("loader: phdr data: {e:?}"),
    };
    assert_eq!(data.len() as u64, filesz, "loader: file/data size mismatch");

    let first_page = Page::containing_address(vaddr);
    let last_byte = vaddr + memsz - 1;
    let last_page = Page::containing_address(last_byte);
    let seg_start = vaddr.as_u64();
    let seg_end = seg_start + filesz;

    let mut pages = first_page;
    while pages <= last_page {
        let frame = mm::allocate_frame().expect("no frame for program page");
        // SAFETY: allocator-owned frame; zero-then-copy (BSS tail + slack).
        unsafe {
            let dst = mm::frame_virt(frame.start_address()).as_mut_ptr::<u8>();
            core::ptr::write_bytes(dst, 0, 4096);
            let page_start = pages.start_address().as_u64();
            let page_end = page_start + 4096;
            let overlap_lo = page_start.max(seg_start);
            let overlap_hi = page_end.min(seg_end);
            if overlap_hi > overlap_lo {
                let off_in_seg = (overlap_lo - seg_start) as usize;
                let count = (overlap_hi - overlap_lo) as usize;
                core::ptr::copy_nonoverlapping(
                    data.as_ptr().byte_add(off_in_seg),
                    dst.byte_add((overlap_lo - page_start) as usize),
                    count,
                );
            }
        }
        // SAFETY: task's own (non-active) tree.
        unsafe {
            mapper
                .map_to(pages, frame, flags, &mut mm::TaskFrameAlloc)
                .expect("loader: map failed")
                .flush();
        }
        pages += 1;
    }
}

/// P4 entry index of the user image base.
fn p4_index_of(_vaddr: VirtAddr) -> u16 {
    (galexy_abi::USER_IMAGE_BASE >> 39) as u16
}
