//! Hand-written x86_64 codegen for gxr v0 (Milestone 60).
//!
//! Emits a `.text` blob and a `.rodata` blob plus the relocations that tie
//! them together. Addresses are the linker's business (`gxld`, Milestone
//! 69): every string reference is a `R_X86_64_64` against `.rodata`.

use crate::ast::{Expr, Program, Stmt};
use galexy_abi::{reserved, CapRights, Syscall};

/// Machine code + read-only data for one program, before linking.
#[derive(Debug, Clone)]
pub struct ObjectCode {
    /// Executable bytes (`_start` at offset 0).
    pub text: Vec<u8>,
    /// Byte-string payloads concatenated.
    pub rodata: Vec<u8>,
    /// `(text_offset, rodata_offset)`: an 8-byte absolute-address field in
    /// `.text` that must receive the address of `.rodata + rodata_offset`.
    pub relocs: Vec<(u32, u32)>,
    /// Exit status from `main`'s return expression.
    pub exit_code: i32,
}

/// Lower a checked program to machine code with relocatable string refs.
pub fn codegen(program: &Program) -> ObjectCode {
    let mut rodata = Vec::new();
    let mut text = Vec::with_capacity(128 + program.body.len() * 40);
    let mut relocs = Vec::new();
    let console = reserved::console(CapRights::WRITE).bits();
    let sys_write = Syscall::Write as u64;
    let sys_exit = Syscall::Exit as u64;

    for stmt in &program.body {
        match stmt {
            Stmt::WriteConsole(bytes) => {
                let off = rodata.len() as u32;
                rodata.extend_from_slice(bytes);
                // mov rax, imm64
                emit_mov_imm64(&mut text, Reg::Rax, sys_write);
                // mov rdi, imm64 (console cap)
                emit_mov_imm64(&mut text, Reg::Rdi, console);
                // mov rsi, imm64 (user buffer VA — filled by the linker)
                let field = emit_mov_imm64(&mut text, Reg::Rsi, 0);
                relocs.push((field, off));
                // mov rdx, imm32 (len) — zero-extends into rdx
                emit_mov_imm32(&mut text, Reg::Rdx, bytes.len() as u32);
                // syscall
                text.extend_from_slice(&[0x0F, 0x05]);
            }
        }
    }
    let Expr::Int(exit_code) = program.ret;

    // exit(exit_code as u64 sign-extended)
    emit_mov_imm64(&mut text, Reg::Rax, sys_exit);
    emit_mov_imm64(&mut text, Reg::Rdi, exit_code as i64 as u64);
    text.extend_from_slice(&[0x0F, 0x05]);

    // Park if exit returns (kernel bug).
    text.push(0xFA); // cli
    text.push(0xF4); // hlt
    text.extend_from_slice(&[0xEB, 0xFC]); // jmp -4 (back to cli)

    ObjectCode {
        text,
        rodata,
        relocs,
        exit_code,
    }
}

#[derive(Clone, Copy)]
enum Reg {
    Rax = 0,
    Rdx = 2,
    Rsi = 6,
    Rdi = 7,
}

/// Returns the text offset of the immediate field.
fn emit_mov_imm64(out: &mut Vec<u8>, reg: Reg, imm: u64) -> u32 {
    // REX.W + B8+rd + imm64
    out.push(0x48);
    out.push(0xB8 + reg as u8);
    let field = out.len() as u32;
    out.extend_from_slice(&imm.to_le_bytes());
    field
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
        // Contains syscall opcode.
        assert!(obj.text.windows(2).any(|w| w == [0x0F, 0x05]));
        // Write syscall number 2 in the first mov rax.
        assert_eq!(&obj.text[2..10], &2u64.to_le_bytes());
        // One string reference: the rsi immediate (third mov, offset 22).
        assert_eq!(obj.relocs, vec![(22, 0)]);
        assert_eq!(&obj.text[22..30], &[0u8; 8]);
    }
}
