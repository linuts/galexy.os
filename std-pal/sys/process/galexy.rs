//! `process::Command` over `Spawn` + `Pipe` + `Wait`.
//!
//! The program is a ramdisk label (`echo`, `ls`); the arguments travel as
//! one NUL-separated blob of at most `SPAWN_ARG_MAX` bytes, without the
//! program name. The child inherits the parent's galfs tokens and query
//! grant (the shell's utility spawn shape) and always has the console.
//!
//! Stdio is two file slots, moved with `SPAWN_WITH_CAPS`: stdin lands in
//! the child's slot 0 and stdout in slot 1 when both are set; a lone
//! stdout or stdin is slot 0. That is the layout `galexy-rt` and this
//! PAL's `startup` probe for. Limits worth knowing:
//!
//! - stderr is not separable from stdout (one console stream, one pipe);
//!   a stderr request yields no pipe and the child's `eprintln!` lands
//!   wherever its stdout goes.
//! - `Stdio::inherit()` / `Stdio::null()` for stdout mean "the console":
//!   a Cap moves, it cannot be shared, so a parent in a pipeline cannot
//!   lend its own stdout.
//! - `current_dir` and environment changes are ignored: there is neither.

use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::io::Read;
use crate::num::NonZero;
use crate::os::galexy::ffi::OsStrExt;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sys::fs::File;
use crate::sys::pal::abi::{self, Cap, SysError};
use crate::sys::pal::syscall;
use crate::sys::pipe::{Pipe, pipe};
use crate::{fmt, io};

////////////////////////////////////////////////////////////////////////////////
// Command
////////////////////////////////////////////////////////////////////////////////

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,

    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    /// An open file whose Cap moves into the child.
    InheritFile(File),
    /// A pipe end whose Cap moves into the child.
    Pipe(Pipe),
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }

    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(|cs| Path::new(cs))
    }

    /// The NUL-separated argument blob (program name excluded).
    fn arg_blob(&self) -> io::Result<Vec<u8>> {
        let mut blob = Vec::new();
        for (i, arg) in self.args.iter().skip(1).enumerate() {
            if i > 0 {
                blob.push(0);
            }
            blob.extend_from_slice(arg.as_bytes());
        }
        if blob.len() > abi::SPAWN_ARG_MAX {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "spawn arguments exceed SPAWN_ARG_MAX bytes"
            ));
        }
        Ok(blob)
    }

    pub fn spawn(
        &mut self,
        default: Stdio,
        needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let program = self.program.as_bytes();
        if program.is_empty() || program.len() > abi::SPAWN_NAME_MAX {
            return Err(io::const_error!(io::ErrorKind::InvalidInput, "bad program label"));
        }
        let blob = self.arg_blob()?;

        // Take the caller's choices; what is left unset follows `default`.
        let stdin = self.stdin.take();
        let stdout = self.stdout.take();
        let _stderr = self.stderr.take(); // shares the child's stdout sink
        // For stdin, `default` only matters when it asks for a pipe; an
        // inherited stdin is "no slot" (the child reads nothing).
        let stdin = match stdin {
            Some(s) => s,
            None if needs_stdin && matches!(default, Stdio::MakePipe) => Stdio::MakePipe,
            None => Stdio::Null,
        };
        let stdout = stdout.unwrap_or(default);

        let mut pipes = StdioPipes { stdin: None, stdout: None, stderr: None };
        // (Cap that moves into the child, if any.)
        let child_in: Option<Cap> = match stdin {
            Stdio::MakePipe => {
                let (read, write) = pipe()?;
                pipes.stdin = Some(write);
                Some(read.into_cap())
            }
            Stdio::InheritFile(file) => Some(file.into_cap()),
            Stdio::Pipe(p) => Some(p.into_cap()),
            Stdio::Inherit | Stdio::Null | Stdio::ParentStdout | Stdio::ParentStderr => None,
        };
        let child_out: Option<Cap> = match stdout {
            Stdio::MakePipe => {
                let (read, write) = pipe()?;
                pipes.stdout = Some(read);
                Some(write.into_cap())
            }
            Stdio::InheritFile(file) => Some(file.into_cap()),
            Stdio::Pipe(p) => Some(p.into_cap()),
            Stdio::Inherit | Stdio::Null | Stdio::ParentStdout | Stdio::ParentStderr => None,
        };

        let mut grants = abi::SPAWN_INHERIT | abi::SPAWN_GRANT_QUERY;
        let slot_of = |cap: Cap| cap.index().wrapping_sub(abi::FILE_CAP_BASE);
        match (child_in, child_out) {
            (Some(i), Some(o)) => {
                grants |= abi::SPAWN_WITH_CAPS
                    | (slot_of(i) << abi::SPAWN_CAP_SHIFT)
                    | (slot_of(o) << (abi::SPAWN_CAP_SHIFT + 4));
            }
            (Some(one), None) | (None, Some(one)) => {
                grants |= abi::SPAWN_WITH_CAPS
                    | (slot_of(one) << abi::SPAWN_CAP_SHIFT)
                    | (abi::SPAWN_CAP_NONE << (abi::SPAWN_CAP_SHIFT + 4));
            }
            (None, None) => {}
        }

        match syscall::spawn(program, &blob, grants) {
            Ok(cap) => Ok((Process { cap, status: None }, pipes)),
            Err(e) => {
                // The kernel did not take the Caps; give the slots back.
                for cap in child_in.into_iter().chain(child_out) {
                    let _ = syscall::close(cap);
                }
                Err(syscall::io_error(e))
            }
        }
    }
}

pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let (mut process, mut pipes) = cmd.spawn(Stdio::MakePipe, false)?;
    drop(pipes.stdin.take());
    let mut stdout = Vec::new();
    if let Some(out) = pipes.stdout.take() {
        out.read_to_end(&mut stdout)?;
    }
    let status = process.wait()?;
    Ok((status, stdout, Vec::new()))
}

impl From<ChildPipe> for Stdio {
    fn from(pipe: ChildPipe) -> Stdio {
        Stdio::Pipe(pipe)
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}

impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    // show all attributes
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            let mut debug_command = f.debug_struct("Command");
            debug_command.field("program", &self.program).field("args", &self.args);
            if !self.env.is_unchanged() {
                debug_command.field("env", &self.env);
            }

            if self.cwd.is_some() {
                debug_command.field("cwd", &self.cwd);
            }

            if self.stdin.is_some() {
                debug_command.field("stdin", &self.stdin);
            }
            if self.stdout.is_some() {
                debug_command.field("stdout", &self.stdout);
            }
            if self.stderr.is_some() {
                debug_command.field("stderr", &self.stderr);
            }

            debug_command.finish()
        } else {
            if let Some(ref cwd) = self.cwd {
                write!(f, "cd {cwd:?} && ")?;
            }
            if self.env.does_clear() {
                write!(f, "env -i ")?;
                // Altered env vars will be printed next, that should exactly work as expected.
            } else {
                // Removed env vars need the command to be wrapped in `env`.
                let mut any_removed = false;
                for (key, value_opt) in self.get_envs() {
                    if value_opt.is_none() {
                        if !any_removed {
                            write!(f, "env ")?;
                            any_removed = true;
                        }
                        write!(f, "-u {} ", key.to_string_lossy())?;
                    }
                }
            }
            // Altered env vars can just be added in front of the program.
            for (key, value_opt) in self.get_envs() {
                if let Some(value) = value_opt {
                    write!(f, "{}={value:?} ", key.to_string_lossy())?;
                }
            }
            if self.program != self.args[0] {
                write!(f, "[{:?}] ", self.program)?;
            }
            write!(f, "{:?}", self.args[0])?;

            for arg in &self.args[1..] {
                write!(f, " {:?}", arg)?;
            }
            Ok(())
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// Processes
////////////////////////////////////////////////////////////////////////////////

/// The exit code a child handed to `Exit` (`137` when it was killed).
#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct ExitStatus(u64);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        match NonZero::new(self.0 as i32) {
            None => Ok(()),
            Some(code) => Err(ExitStatusError(code)),
        }
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.0 as i32)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status: {}", self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatusError(NonZero<i32>);

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0.get() as u64)
    }
}

impl ExitStatusError {
    pub fn code(self) -> Option<NonZero<i32>> {
        Some(self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

pub struct Process {
    cap: Cap,
    /// Filled by the first successful `wait`; the Cap is stale afterwards.
    status: Option<ExitStatus>,
}

impl Process {
    /// The child's process-Cap slot. Not a global PID (there are none).
    pub fn id(&self) -> u32 {
        self.cap.index() as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "invalid argument: can't kill an exited process"
            ));
        }
        syscall::kill(self.cap).map_err(syscall::io_error)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        loop {
            match syscall::wait(self.cap) {
                Ok(code) => {
                    let status = ExitStatus(code);
                    self.status = Some(status);
                    return Ok(status);
                }
                Err(SysError::Interrupted) => continue,
                Err(e) => return Err(syscall::io_error(e)),
            }
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        match syscall::wait_poll(self.cap) {
            Ok(code) => {
                let status = ExitStatus(code);
                self.status = Some(status);
                Ok(Some(status))
            }
            Err(SysError::NoResource) => Ok(None),
            Err(e) => Err(syscall::io_error(e)),
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // A waited Cap is already gone; closing an unwaited one drops the
        // handle (the child keeps running, as on Unix).
        if self.status.is_none() {
            let _ = syscall::close(self.cap);
        }
    }
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(|os| &**os)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

pub type ChildPipe = Pipe;

/// Both pipes present: drain stdout, then stderr. This PAL never hands
/// out a stderr pipe, so the second read is a no-op in practice.
pub fn read_output(
    out: ChildPipe,
    stdout: &mut Vec<u8>,
    err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    out.read_to_end(stdout)?;
    err.read_to_end(stderr)?;
    Ok(())
}

/// The task's debug id from the self snapshot (`id=…`), or `0`.
pub fn getpid() -> u32 {
    let mut buf = [0u8; 160];
    let Ok(n) = syscall::read(abi::reserved::self_cap(), &mut buf) else {
        return 0;
    };
    let Ok(text) = core::str::from_utf8(&buf[..n]) else {
        return 0;
    };
    text.split_once("id=")
        .and_then(|(_, rest)| rest.split(' ').next())
        .and_then(|id| id.trim().parse().ok())
        .unwrap_or(0)
}
