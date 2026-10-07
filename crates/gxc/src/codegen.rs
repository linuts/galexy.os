//! Hand-written x86_64 codegen for gxr v0 (Milestone 60).
//!
//! Emits a flat `.text` blob and a `.rodata` blob. No Cranelift — hello is
//! a handful of instructions and the ELF shape stays obvious.

use crate::ast::{Expr, Program, Stmt};
use galexy_abi::{reserved, CapRights, Syscall};

/// Machine code + read-only data for one program.
#[derive(Debug, Clone)]
pub struct ObjectCode {
    /// Executable bytes (`_start` at offset 0).
    pub text: Vec<u8>,
    /// Byte-string payloads concatenated; addresses assigned by the ELF
    /// emitter once the virtual base is known.
    pub rodata: Vec<u8>,
    /// `(rodata_offset, len)` for each `write_console` in order.
    pub writes: Vec<(u32, u32)>,
    /// Exit status from `main`'s return expression.
    pub exit_code: i32,
}

/// Lower a checked program to machine code with relocatable string refs.
pub fn codegen(program: &Program) -> ObjectCode {
    let mut rodata = Vec::new();
    let mut writes = Vec::new();
    for stmt in &program.body {
        match stmt {
            Stmt::WriteConsole(bytes) => {
                let off = rodata.len() as u32;
                rodata.extend_from_slice(bytes);
                writes.push((off, bytes.len() as u32));
            }
        }
    }
    let exit_code = match program.ret {
        Expr::Int(n) => n,
    };

    // Text is filled in by [`finish_text`] once the ELF layout knows the
    // absolute VAs for rodata strings. Here we only collect payloads.
    ObjectCode {
        text: Vec::new(),
        rodata,
        writes,
        exit_code,
    }
}

/// Encode `_start` with absolute addresses for each write payload.
///
/// `rodata_va` is the virtual address of the first rodata byte.
/// Entry is the first byte of the returned buffer.
pub fn finish_text(code: &ObjectCode, rodata_va: u64) -> Vec<u8> {
    let mut text = Vec::with_capacity(128 + code.writes.len() * 40);
    let console = reserved::console(CapRights::WRITE).bits();
    let sys_write = Syscall::Write as u64;
    let sys_exit = Syscall::Exit as u64;

    for &(off, len) in &code.writes {
        let addr = rodata_va + u64::from(off);
        // mov rax, imm64
        emit_mov_imm64(&mut text, Reg::Rax, sys_write);
        // mov rdi, imm64 (console cap)
        emit_mov_imm64(&mut text, Reg::Rdi, console);
        // mov rsi, imm64 (user buffer VA)
        emit_mov_imm64(&mut text, Reg::Rsi, addr);
        // mov rdx, imm32 (len) — zero-extends into rdx
        emit_mov_imm32(&mut text, Reg::Rdx, len);
        // syscall
        text.extend_from_slice(&[0x0F, 0x05]);
    }

    // exit(exit_code as u64 sign-extended)
    emit_mov_imm64(&mut text, Reg::Rax, sys_exit);
    emit_mov_imm64(&mut text, Reg::Rdi, code.exit_code as i64 as u64);
    text.extend_from_slice(&[0x0F, 0x05]);

    // Park if exit returns (kernel bug).
    text.push(0xFA); // cli
    text.push(0xF4); // hlt
    text.extend_from_slice(&[0xEB, 0xFC]); // jmp -4 (back to cli)

    text
}

#[derive(Clone, Copy)]
enum Reg {
    Rax = 0,
    Rdx = 2,
    Rsi = 6,
    Rdi = 7,
}

fn emit_mov_imm64(out: &mut Vec<u8>, reg: Reg, imm: u64) {
    // REX.W + B8+rd + imm64
    out.push(0x48);
    out.push(0xB8 + reg as u8);
    out.extend_from_slice(&imm.to_le_bytes());
}

fn emit_mov_imm32(out: &mut Vec<u8>, reg: Reg, imm: u32) {
    // REX.W + C7 /0 + rd + imm32  →  mov r64, sign_extend(imm32)
    out.push(0x48);
    out.push(0xC7);
    out.push(0xC0 | (reg as u8));
    out.extend_from_slice(&imm.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_check;

    #[test]
    fn encodes_syscall_bytes() {
        let prog = compile_check(r#"fn main() -> i32 { write_console(b"Hi\n"); 0 }"#).unwrap();
        let obj = codegen(&prog);
        assert_eq!(obj.rodata, b"Hi\n");
        let text = finish_text(&obj, 0x1000);
        // Contains syscall opcode.
        assert!(text.windows(2).any(|w| w == [0x0F, 0x05]));
        // Write syscall number 2 in the first mov rax.
        assert_eq!(&text[2..10], &2u64.to_le_bytes());
    }
}
