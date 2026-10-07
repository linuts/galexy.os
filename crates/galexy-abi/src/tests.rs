//! ABI stability, numbering, and roundtrip checks under `cargo test -p galexy-abi`.
#![cfg(test)]

use super::*;

#[test]
fn syscall_numbers_are_unique_and_dense() {
    // Every entry at index N must behave as syscall number N; the enum
    // variants must not repeat. Dense 0..=len because dispatch tables
    // index by number.
    let mut seen = [false; 64];
    for (n, call) in SYSCALLS.iter().enumerate() {
        assert!(n < 64, "ABI slotted past its 64-call cap: {n}");
        let variant = *call as usize;
        assert_eq!(variant, n, "Syscall variant order != SYSCALLS order at {n}");
        assert!(!seen[variant], "duplicate syscall number {n}");
        seen[variant] = true;
    }
    assert_eq!(MAX_SYSCALL, (SYSCALLS.len() - 1) as u64);
}

#[test]
fn exit_is_zero_and_removal_is_never_a_renumber() {
    // `exit` must stay syscall 0 — a task's very first code path.
    assert!(matches!(SYSCALLS[0], Syscall::Exit));
}

#[test]
fn cap_roundtrips() {
    let rights = CapRights::READ.union(CapRights::WRITE);
    let cap = Cap::new(0x1234, rights);
    assert_eq!(cap.index(), 0x1234);
    assert_eq!(cap.rights(), rights);
    assert_eq!(cap.bits() >> 48, rights.bits() as u64);
    // A big index gets clamped into the 48-bit field without corrupting
    // the rights half.
    let clamped = Cap::new(0xFFFF_0000_0000_0001, rights);
    assert_eq!(clamped.index(), 1);
    assert_eq!(clamped.rights(), rights);
}

#[test]
fn null_cap_is_never_something() {
    let null = Cap::null();
    assert_eq!(null.bits(), 0);
    assert_eq!(null.index(), 0);
    assert_eq!(null.rights(), CapRights::NONE);
}

#[test]
fn rights_masks() {
    assert!(CapRights::ALL.contains(CapRights::WRITE));
    assert!(CapRights::WRITE.contains(CapRights::WRITE));
    assert!(!CapRights::READ.contains(CapRights::WRITE));
    assert!(!CapRights::NONE.contains(CapRights::READ));
    assert_eq!(CapRights::READ.union(CapRights::SIGNAL).bits(), 0b101);
    assert_eq!(
        CapRights::READ
            .union(CapRights::WRITE)
            .intersection(CapRights::READ),
        CapRights::READ
    );
}

#[test]
fn reserved_caps_have_permanent_indexes() {
    // These ARE the ABI for future programs; changing them must be a
    // conscious ABI break. Pin them.
    assert_eq!(reserved::CONSOLE_INDEX, 1);
    assert_eq!(reserved::SELF_INDEX, 2);
    assert_eq!(FILE_CAP_BASE, 3);
    assert_eq!(reserved::KEYBOARD_INDEX, 0x8000);
    assert_eq!(reserved::LOADER_INDEX, 0x8001);
    assert_eq!(reserved::STATS_INDEX, 0x8002);
    assert_eq!(reserved::TASKS_INDEX, 0x8003);
    assert_eq!(reserved::THREADS_INDEX, 0x8004);
    assert_eq!(reserved::POWER_INDEX, 0x8005);
    assert_eq!(reserved::FILES_INDEX, 0x8006);
    assert_eq!(POWER_SHUTDOWN, 0);
    assert_eq!(POWER_REBOOT, 1);
    assert_eq!(CapRights::POWER.bits(), 1 << 5);
    assert_eq!(CapRights::PROC_WAIT.bits(), 1 << 6);
    assert_eq!(CapRights::PROC_KILL.bits(), 1 << 7);
    assert_eq!(CapRights::PROC_TRANSFER.bits(), 1 << 8);
    assert_eq!(CapRights::PROC_INSPECT.bits(), 1 << 9);
    assert!(CapRights::PROC_PARENT.contains(CapRights::PROC_WAIT));
    assert!(CapRights::PROC_PARENT.contains(CapRights::PROC_KILL));
    assert!(CapRights::PROC_PARENT.contains(CapRights::PROC_TRANSFER));
    assert!(CapRights::PROC_PARENT.contains(CapRights::PROC_INSPECT));
    assert!(!CapRights::PROC_WAIT.contains(CapRights::WAIT));
    assert!(reserved::KEYBOARD_INDEX > FILE_CAP_BASE);
    assert!(matches!(SYSCALLS[4], Syscall::Open));
    assert!(matches!(SYSCALLS[5], Syscall::Read));
    assert!(matches!(SYSCALLS[6], Syscall::Close));
    assert!(matches!(SYSCALLS[7], Syscall::Spawn));
    assert!(matches!(SYSCALLS[8], Syscall::Power));
    assert!(matches!(SYSCALLS[9], Syscall::Create));
    assert!(matches!(SYSCALLS[10], Syscall::Remove));
    assert!(matches!(SYSCALLS[11], Syscall::Grant));
    assert!(matches!(SYSCALLS[12], Syscall::Revoke));
    assert!(matches!(SYSCALLS[13], Syscall::Pipe));
    assert!(matches!(SYSCALLS[14], Syscall::Give));
    assert!(matches!(SYSCALLS[15], Syscall::Seek));
    assert!(matches!(SYSCALLS[16], Syscall::User));
    assert!(matches!(SYSCALLS[17], Syscall::Rename));
    assert!(matches!(SYSCALLS[18], Syscall::Truncate));
    assert!(matches!(SYSCALLS[19], Syscall::Stat));
    assert!(matches!(SYSCALLS[20], Syscall::Sync));
    assert!(matches!(SYSCALLS[21], Syscall::Share));
    assert!(matches!(SYSCALLS[22], Syscall::Unshare));
    assert!(matches!(SYSCALLS[23], Syscall::Wait));
    assert!(matches!(SYSCALLS[24], Syscall::Kill));
    assert_eq!(PROC_CAP_BASE, 0x40);
    assert_eq!(MAX_PROC_CAPS, 8);
    assert!(PROC_CAP_BASE > FILE_CAP_BASE + 7);
    assert!(PROC_CAP_BASE < reserved::KEYBOARD_INDEX);
    assert_eq!(SPAWN_GRANT_QUERY, 1);
    assert_eq!(SPAWN_WAIT, 2);
    assert_eq!(SPAWN_INHERIT, 4);
    assert_eq!(SPAWN_NAME_MAX, 64);
    assert_eq!(SPAWN_ARG_MAX, 256);
    assert_eq!(STAT_LEN, 48);
    assert_eq!(USER_TOKENS, 8);
    assert_eq!(STAT_FILE, 1);
    assert_eq!(STAT_DIR, 2);
    assert_eq!(TOKEN_ALL, 31);
    assert_eq!(SEEK_SET, 0);
    assert_eq!(SEEK_CUR, 1);
    assert_eq!(SEEK_END, 2);
    assert_eq!(USER_WHOAMI, 0);
    assert_eq!(USER_USERS, 1);
    assert_eq!(USER_ADD, 2);
    assert_eq!(USER_DEL, 3);
    assert_eq!(USER_SU, 4);
    assert_eq!(USER_QUOTA, 9);
    assert_eq!(USER_SETQUOTA, 10);
    assert_eq!(QUOTA_LEN, 16);
    let keyboard = reserved::keyboard(CapRights::READ);
    assert_eq!(keyboard.index(), reserved::KEYBOARD_INDEX);
    assert!(keyboard.rights().contains(CapRights::READ));
    let loader = reserved::loader(CapRights::EXEC);
    assert_eq!(loader.index(), reserved::LOADER_INDEX);
    assert!(loader.rights().contains(CapRights::EXEC));
    let stats = reserved::stats(CapRights::READ);
    assert_eq!(stats.index(), reserved::STATS_INDEX);
    assert!(stats.rights().contains(CapRights::READ));
    assert_eq!(
        reserved::tasks(CapRights::READ).index(),
        reserved::TASKS_INDEX
    );
    assert_eq!(
        reserved::threads(CapRights::READ).index(),
        reserved::THREADS_INDEX
    );
    let power = reserved::power(CapRights::POWER);
    assert_eq!(power.index(), reserved::POWER_INDEX);
    assert!(power.rights().contains(CapRights::POWER));
    let files = reserved::files(CapRights::READ);
    assert_eq!(files.index(), reserved::FILES_INDEX);
    assert!(files.rights().contains(CapRights::READ));
    let console = reserved::console(CapRights::WRITE);
    assert_eq!(console.index(), 1);
    let self_cap = reserved::self_cap();
    assert_eq!(self_cap.index(), 2);
    assert!(self_cap.rights().contains(CapRights::READ));
    assert!(self_cap.rights().contains(CapRights::PROC_INSPECT));
    assert!(!self_cap.rights().contains(CapRights::PROC_KILL));
    assert!(!self_cap.rights().contains(CapRights::PROC_WAIT));
}

#[test]
fn result_codes_roundtrip() {
    for code in [SysError::BadCap as u64, 2, 3, 4, 5, 6, 7] {
        let r = SyscallResult {
            ok: false,
            value: code,
        };
        assert_eq!(r.to_result(), Err(SysError::from_code(code)));
    }
    let ok = SyscallResult::ok(0x9999);
    assert_eq!(ok.to_result(), Ok(0x9999));
    assert_eq!(SyscallResult::err(SysError::AccessDenied).value, 2);
    assert!(!SyscallResult::err(SysError::BadBuffer).ok);
}
