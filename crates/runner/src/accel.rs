//! QEMU accelerator selection.
//!
//! KVM is usable only when creating a vCPU succeeds. Opening `/dev/kvm`
//! is not enough: a snapshot restore can leave `CR4.VMXE` set after the
//! hypervisor has dropped VMXON (the kernel did `VMXON` at boot because
//! `kvm.enable_virt_at_load=Y`). The next `VMCLEAR`, inside
//! `alloc_loaded_vmcs`, faults and the kernel BUGs in
//! `kvm_spurious_fault`. QEMU stays up and the guest writes nothing.
//!
//! The probe runs in a child. That fault delivers `SIGSEGV` to the
//! caller, so an in-process `ioctl` would take down `cargo test`.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Accelerator the runner will pass to QEMU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accel {
    /// `-accel kvm -cpu host`
    Kvm,
    /// `-accel tcg -cpu max,+x2apic`
    Tcg,
}

/// `GALEXY_ACCEL=kvm` was set and this host cannot create a vCPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvmUnavailable {
    /// `/dev/kvm` opened. The failure is `KVM_CREATE_VCPU`, not a missing device.
    opened: bool,
}

impl std::fmt::Display for KvmUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.opened {
            write!(
                f,
                "GALEXY_ACCEL=kvm but KVM_CREATE_VCPU failed. /dev/kvm opened, \
                 then VMCLEAR faulted in alloc_loaded_vmcs (kvm_spurious_fault). \
                 Nested VMX state does not match CR4.VMXE, which a snapshot \
                 restore leaves behind when the kernel ran VMXON at boot \
                 (kvm.enable_virt_at_load=Y). The guest would write no serial"
            )
        } else {
            write!(f, "GALEXY_ACCEL=kvm but /dev/kvm did not open")
        }
    }
}

impl std::error::Error for KvmUnavailable {}

#[derive(Clone, Copy)]
struct Probe {
    opened: bool,
    vcpu: bool,
}

/// `GALEXY_ACCEL` when it is `kvm` or `tcg`. Any other value is ignored.
fn forced_accel() -> Option<&'static str> {
    match std::env::var("GALEXY_ACCEL").ok().as_deref() {
        Some("kvm") => Some("kvm"),
        Some("tcg") => Some("tcg"),
        _ => None,
    }
}

/// Pure selection rule. `vcpu` is ignored unless `opened` is true.
pub(crate) fn decide(
    forced: Option<&str>,
    opened: bool,
    vcpu: bool,
) -> Result<Accel, KvmUnavailable> {
    let vcpu = opened && vcpu;
    match forced {
        Some("tcg") => Ok(Accel::Tcg),
        Some("kvm") => {
            if vcpu {
                Ok(Accel::Kvm)
            } else {
                Err(KvmUnavailable { opened })
            }
        }
        _ => {
            if vcpu {
                Ok(Accel::Kvm)
            } else {
                Ok(Accel::Tcg)
            }
        }
    }
}

fn probe_cached() -> Probe {
    static CACHE: OnceLock<Probe> = OnceLock::new();
    *CACHE.get_or_init(probe_once)
}

fn probe_once() -> Probe {
    // Linux x86_64 `_IO(KVMIO, nr)` with `KVMIO = 0xAE`.
    const KVM_CREATE_VM: u64 = 0xae01;
    const KVM_CREATE_VCPU: u64 = 0xae41;
    const O_RDWR: i32 = 2;
    // `RLIMIT_CORE`. A failed vCPU create delivers SIGSEGV; skip the core file.
    const RLIMIT_CORE: u32 = 4;

    // SAFETY: libc process-control and ioctl calls. The child uses only
    // async-signal-safe functions (`setrlimit`, `open`, `ioctl`, `_exit`)
    // so a `fork` from a multithreaded test runner cannot deadlock on a
    // lock the child did not take. The parent only `waitpid`s.
    unsafe {
        let pid = fork();
        if pid < 0 {
            return Probe {
                opened: false,
                vcpu: false,
            };
        }
        if pid == 0 {
            let limit = Rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let _ = setrlimit(RLIMIT_CORE, &limit);
            let fd = open(c"/dev/kvm".as_ptr(), O_RDWR, 0);
            if fd < 0 {
                _exit(2);
            }
            let vm = ioctl(fd, KVM_CREATE_VM, 0);
            if vm < 0 {
                _exit(3);
            }
            let vcpu = ioctl(vm, KVM_CREATE_VCPU, 0);
            _exit(if vcpu < 0 { 4 } else { 0 });
        }
        let mut status = 0;
        loop {
            let waited = waitpid(pid, &mut status, 0);
            if waited == pid {
                break;
            }
            if waited < 0 && errno_is_eintr() {
                continue;
            }
            return Probe {
                opened: false,
                vcpu: false,
            };
        }
        // Exit 0: vCPU created. Exit 2: `/dev/kvm` did not open.
        // Anything else (exit 3/4, or a signal from the host BUG) means
        // the device opened and a vCPU could not be created.
        let exited = (status & 0x7f) == 0;
        let code = (status >> 8) & 0xff;
        if exited && code == 0 {
            Probe {
                opened: true,
                vcpu: true,
            }
        } else if exited && code == 2 {
            Probe {
                opened: false,
                vcpu: false,
            }
        } else {
            Probe {
                opened: true,
                vcpu: false,
            }
        }
    }
}

fn errno_is_eintr() -> bool {
    // SAFETY: reading `errno` after a failed libc call on this thread.
    unsafe { errno_location().read() == 4 }
}

/// KVM when a vCPU can be created, unless `GALEXY_ACCEL=tcg`.
/// `GALEXY_ACCEL=kvm` on a host that fails the probe is an error.
pub fn selected_accel() -> Result<Accel, KvmUnavailable> {
    let forced = forced_accel();
    if forced == Some("tcg") {
        return Ok(Accel::Tcg);
    }
    let probe = probe_cached();
    decide(forced, probe.opened, probe.vcpu)
}

/// True when this process will boot QEMU with KVM.
///
/// Panics when `GALEXY_ACCEL=kvm` and the probe failed. Callers that
/// want a clean exit should use [`selected_accel`].
pub fn use_kvm() -> bool {
    match selected_accel() {
        Ok(Accel::Kvm) => true,
        Ok(Accel::Tcg) => false,
        Err(err) => panic!("{err}"),
    }
}

/// Note appended to the `accel=tcg` log when KVM was present but rejected.
pub fn tcg_fallback_note() -> Option<&'static str> {
    if forced_accel() == Some("tcg") {
        return None;
    }
    let probe = probe_cached();
    if probe.opened && !probe.vcpu {
        Some("/dev/kvm opened but KVM_CREATE_VCPU failed; host VMX is not usable")
    } else {
        None
    }
}

/// Append `-accel` / `-cpu` and print `[runner] accel=` once.
///
/// Returns the refusal when `GALEXY_ACCEL=kvm` and the probe failed,
/// after printing it. No QEMU arguments are added in that case.
pub fn configure_accel(cmd: &mut Command) -> Result<(), KvmUnavailable> {
    static PRINTED: AtomicBool = AtomicBool::new(false);
    let print = PRINTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok();
    match selected_accel() {
        Ok(Accel::Kvm) => {
            if print {
                eprintln!("[runner] accel=kvm");
            }
            cmd.arg("-accel").arg("kvm").arg("-cpu").arg("host");
            Ok(())
        }
        Ok(Accel::Tcg) => {
            if print {
                match tcg_fallback_note() {
                    Some(note) => eprintln!("[runner] accel=tcg ({note})"),
                    None => eprintln!("[runner] accel=tcg"),
                }
            }
            cmd.arg("-accel").arg("tcg").arg("-cpu").arg("max,+x2apic");
            Ok(())
        }
        Err(err) => {
            if print {
                eprintln!("[runner] {err}");
            }
            Err(err)
        }
    }
}

#[repr(C)]
struct Rlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

unsafe extern "C" {
    fn fork() -> i32;
    fn open(path: *const i8, flags: i32, ...) -> i32;
    fn ioctl(fd: i32, request: u64, arg: u64) -> i32;
    fn setrlimit(resource: u32, limit: *const Rlimit) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(status: i32) -> !;
    // `errno` is a macro. `__errno_location` is the glibc symbol.
    #[link_name = "__errno_location"]
    fn errno_location() -> *mut i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_kvm_only_when_a_vcpu_is_created() {
        assert_eq!(decide(None, false, false).unwrap(), Accel::Tcg);
        assert_eq!(decide(None, true, false).unwrap(), Accel::Tcg);
        assert_eq!(decide(None, false, true).unwrap(), Accel::Tcg);
        assert_eq!(decide(None, true, true).unwrap(), Accel::Kvm);
    }

    #[test]
    fn tcg_override_skips_a_working_kvm() {
        assert_eq!(decide(Some("tcg"), true, true).unwrap(), Accel::Tcg);
    }

    #[test]
    fn kvm_override_refuses_a_dead_accelerator() {
        let missing = decide(Some("kvm"), false, false).unwrap_err();
        assert!(!missing.opened);
        assert!(missing.to_string().contains("/dev/kvm did not open"));
        let broken = decide(Some("kvm"), true, false).unwrap_err();
        assert!(broken.opened);
        assert!(broken.to_string().contains("KVM_CREATE_VCPU failed"));
        assert_eq!(decide(Some("kvm"), true, true).unwrap(), Accel::Kvm);
    }

    #[test]
    fn unknown_override_is_auto() {
        assert_eq!(decide(Some("nope"), true, true).unwrap(), Accel::Kvm);
        assert_eq!(decide(Some("nope"), true, false).unwrap(), Accel::Tcg);
    }

    /// A broken host BUGs once in `dmesg` (`kvm_spurious_fault`) and
    /// still returns. The child takes the signal, not this process.
    #[test]
    fn live_probe_returns() {
        let probe = probe_once();
        let auto = decide(None, probe.opened, probe.vcpu).unwrap();
        if probe.vcpu {
            assert_eq!(auto, Accel::Kvm);
        } else {
            assert_eq!(auto, Accel::Tcg);
            assert!(decide(Some("kvm"), probe.opened, probe.vcpu).is_err());
        }
    }
}
