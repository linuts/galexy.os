//! Background jobs and pipelines.

use super::state::*;

use core::cell::UnsafeCell;
use core::sync::atomic::Ordering;

use galexy_abi::Cap;
use galexy_rt::{close, files_cap, kill, pipe, read, spawn_with, wait, write_console};

pub(crate) const JOB_MAX: usize = 4;
pub(crate) const STAGE_MAX: usize = 4;

#[derive(Clone, Copy)]
pub(crate) struct Job {
    pub(crate) caps: [u64; STAGE_MAX],
    pub(crate) n: u8,
    pub(crate) label: [u8; 48],
    pub(crate) label_len: u8,
}

impl Job {
    const fn empty() -> Self {
        Self {
            caps: [0; STAGE_MAX],
            n: 0,
            label: [0; 48],
            label_len: 0,
        }
    }
}

pub(crate) struct JobTable {
    pub(crate) jobs: [Job; JOB_MAX],
    pub(crate) count: usize,
    pub(crate) label: [u8; 64],
    pub(crate) label_len: usize,
}

pub(crate) struct JobsCell(UnsafeCell<JobTable>);

// The shell task is the only caller. Background children do not touch this.
unsafe impl Sync for JobsCell {}
pub(crate) static JOBS: JobsCell = JobsCell(UnsafeCell::new(JobTable {
    jobs: [Job::empty(); JOB_MAX],
    count: 0,
    label: [0; 64],
    label_len: 0,
}));

pub(crate) fn jobs_mut() -> &'static mut JobTable {
    // SAFETY: one shell task; dispatch is not re-entered.
    unsafe { &mut *JOBS.0.get() }
}

/// Up to four stages joined by ` | `. `echo text | cat` stays mode 3 so
/// the text still lands in the pipe. Other stages get a NUL argv.
pub(crate) fn try_pipeline(cwd: &Cwd, line: &[u8]) -> bool {
    if find_slice(line, b" | ").is_none() {
        return false;
    }
    let mut stages: [&[u8]; STAGE_MAX] = [&[], &[], &[], &[]];
    let mut n = 0usize;
    let mut rest = line;
    loop {
        if n == STAGE_MAX {
            write_console(b"pipeline: too many stages\n");
            prompt(cwd);
            return true;
        }
        if let Some(at) = find_slice(rest, b" | ") {
            stages[n] = trim(&rest[..at]);
            rest = trim(&rest[at + 3..]);
            n += 1;
        } else {
            stages[n] = rest;
            n += 1;
            break;
        }
    }
    if stages[..n].iter().any(|s| s.is_empty()) {
        write_console(b"pipeline: empty stage\n");
        prompt(cwd);
        return true;
    }
    run_pipeline(cwd, &stages[..n]);
    true
}

/// File-table index of `cap` (offset from [`galexy_abi::FILE_CAP_BASE`]).
pub(crate) fn file_slot(cap: Cap) -> u64 {
    cap.index().saturating_sub(galexy_abi::FILE_CAP_BASE)
}

pub(crate) fn strip_background(line: &[u8]) -> (&[u8], bool) {
    if let Some(rest) = line.strip_suffix(b" &") {
        (trim(rest), true)
    } else {
        (line, false)
    }
}

pub(crate) fn set_cmd_label(line: &[u8]) {
    let jobs = jobs_mut();
    let n = line.len().min(jobs.label.len());
    jobs.label[..n].copy_from_slice(&line[..n]);
    jobs.label_len = n;
}

pub(crate) fn push_job(caps: &[u64]) -> bool {
    if caps.is_empty() || caps.len() > STAGE_MAX {
        return false;
    }
    let jobs = jobs_mut();
    if jobs.count >= JOB_MAX {
        return false;
    }
    let mut job = Job::empty();
    job.n = caps.len() as u8;
    job.caps[..caps.len()].copy_from_slice(caps);
    let n = jobs.label_len.min(job.label.len());
    job.label[..n].copy_from_slice(&jobs.label[..n]);
    job.label_len = n as u8;
    jobs.jobs[jobs.count] = job;
    jobs.count += 1;
    true
}

pub(crate) fn show_jobs() {
    let jobs = jobs_mut();
    if jobs.count == 0 {
        write_console(b"jobs: none\n");
        return;
    }
    for (i, job) in jobs.jobs.iter().enumerate().take(jobs.count) {
        write_console(b"[");
        write_u64_dec((i + 1) as u64);
        write_console(b"] ");
        write_console(&job.label[..job.label_len as usize]);
        write_console(b"\n");
    }
}

pub(crate) fn fg_job(cwd: &Cwd) {
    let taken = {
        let jobs = jobs_mut();
        if jobs.count == 0 {
            None
        } else {
            jobs.count -= 1;
            Some(jobs.jobs[jobs.count])
        }
    };
    let Some(job) = taken else {
        write_console(b"fg: no current job\n");
        prompt(cwd);
        return;
    };
    wait_caps(&job.caps[..job.n as usize]);
    prompt(cwd);
}

pub(crate) fn push_dec(out: &mut alloc::string::String, mut n: u64) {
    if n == 0 {
        out.push('0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    for byte in &tmp[i..] {
        out.push(*byte as char);
    }
}

pub(crate) fn ls_arg(cwd: &Cwd, rest: &[u8], out: &mut [u8]) -> Option<usize> {
    let rest = trim(rest);
    if rest.is_empty() {
        if cwd.len > out.len() {
            return None;
        }
        out[..cwd.len].copy_from_slice(&cwd.buf[..cwd.len]);
        return Some(cwd.len);
    }
    if rest != b"-l" {
        return None;
    }
    if out.len() < 2 {
        return None;
    }
    out[0] = b'-';
    out[1] = b'l';
    if cwd.len == 0 {
        return Some(2);
    }
    if 3 + cwd.len > out.len() {
        return None;
    }
    out[2] = 0;
    out[3..3 + cwd.len].copy_from_slice(&cwd.buf[..cwd.len]);
    Some(3 + cwd.len)
}

pub(crate) fn grep_arg(cwd: &Cwd, rest: &[u8], has_in: bool, out: &mut [u8]) -> Option<usize> {
    let (pat, files) = split_word(rest)?;
    if pat.is_empty() || pat.len() > out.len() {
        return None;
    }
    out[..pat.len()].copy_from_slice(pat);
    let mut n = pat.len();
    let files = trim(files);
    if files.is_empty() {
        if has_in {
            if n + 2 > out.len() {
                return None;
            }
            out[n] = 0;
            out[n + 1] = b'-';
            n += 2;
        }
        return Some(n);
    }
    let mut paths = [0u8; 256];
    let pn = join_paths(cwd, files, &mut paths)?;
    if n + 1 + pn > out.len() {
        return None;
    }
    out[n] = 0;
    n += 1;
    out[n..n + pn].copy_from_slice(&paths[..pn]);
    n += pn;
    if has_in && !words_have_dash(files) {
        if n + 2 > out.len() {
            return None;
        }
        out[n] = 0;
        out[n + 1] = b'-';
        n += 2;
    }
    Some(n)
}

pub(crate) fn join_stage(
    cwd: &Cwd,
    rest: &[u8],
    has_in: bool,
    compose_paths: bool,
    out: &mut [u8; 256],
) -> Option<usize> {
    let rest = trim(rest);
    if compose_paths {
        if !has_in {
            return join_paths(cwd, rest, out);
        }
        if words_have_dash(rest) || rest == b"-" {
            return join_paths(cwd, rest, out);
        }
        let mut body = [0u8; 256];
        let bn = if rest.is_empty() {
            0
        } else {
            join_paths(cwd, rest, &mut body)?
        };
        if bn == 0 {
            out[0] = b'-';
            return Some(1);
        }
        if 2 + bn > out.len() {
            return None;
        }
        out[0] = b'-';
        out[1] = 0;
        out[2..2 + bn].copy_from_slice(&body[..bn]);
        return Some(2 + bn);
    }
    if has_in && !rest.starts_with(b"-") {
        if rest.is_empty() {
            out[0] = b'-';
            return Some(1);
        }
        if 2 + rest.len() > out.len() {
            return None;
        }
        out[0] = b'-';
        out[1] = 0;
        out[2..2 + rest.len()].copy_from_slice(rest);
        return Some(2 + rest.len());
    }
    if rest.len() > out.len() {
        return None;
    }
    out[..rest.len()].copy_from_slice(rest);
    Some(rest.len())
}

pub(crate) fn words_have_dash(rest: &[u8]) -> bool {
    Words::new(rest).any(|word| word == b"-")
}

pub(crate) fn join_paths(cwd: &Cwd, rest: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut n = 0usize;
    let mut any = false;
    for word in Words::new(rest) {
        if word != b"-" && !path_arg_ok(word) {
            return None;
        }
        let mut path = [0u8; PATH_MAX];
        let pn = if word == b"-" {
            path[0] = b'-';
            1
        } else {
            compose(cwd, word, false, &mut path)?
        };
        let extra = usize::from(any);
        if n + extra + pn > out.len() {
            return None;
        }
        if any {
            out[n] = 0;
            n += 1;
        }
        out[n..n + pn].copy_from_slice(&path[..pn]);
        n += pn;
        any = true;
    }
    if any {
        Some(n)
    } else {
        None
    }
}

pub(crate) struct Words<'a> {
    pub(crate) rest: &'a [u8],
}

impl<'a> Words<'a> {
    fn new(rest: &'a [u8]) -> Self {
        Self { rest: trim(rest) }
    }
}

impl<'a> Iterator for Words<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        self.rest = trim(self.rest);
        if self.rest.is_empty() {
            return None;
        }
        let at = self
            .rest
            .iter()
            .position(|b| *b == b' ')
            .unwrap_or(self.rest.len());
        let word = &self.rest[..at];
        self.rest = &self.rest[at..];
        Some(word)
    }
}

pub(crate) fn run_pipeline(cwd: &Cwd, stages: &[&[u8]]) {
    let n = stages.len();
    if !(2..=STAGE_MAX).contains(&n) {
        write_console(b"pipeline: failed\n");
        LAST_STATUS.store(1, Ordering::Relaxed);
        prompt(cwd);
        return;
    }
    let pipes = n - 1;
    let mut ends = [[0u64; 2]; STAGE_MAX - 1];
    for i in 0..pipes {
        let piped = pipe(&mut ends[i]);
        if !piped.ok {
            for prev in &ends[..i] {
                let _ = close(Cap::from_bits(prev[0]));
                let _ = close(Cap::from_bits(prev[1]));
            }
            write_console(b"pipeline: failed\n");
            LAST_STATUS.store(1, Ordering::Relaxed);
            prompt(cwd);
            return;
        }
    }
    let mut args = [[0u8; 256]; STAGE_MAX];
    let mut arg_len = [0usize; STAGE_MAX];
    let mut progs: [&[u8]; STAGE_MAX] = [&[], &[], &[], &[]];
    for (i, stage) in stages.iter().enumerate() {
        let Some((prog, rest)) = split_word(stage) else {
            close_pipe_ends(&ends[..pipes]);
            write_console(b"pipeline: empty stage\n");
            LAST_STATUS.store(1, Ordering::Relaxed);
            prompt(cwd);
            return;
        };
        progs[i] = prog;
        let has_in = i > 0;
        let has_out = i + 1 < n;
        match stage_arg(cwd, prog, rest, has_in, has_out, &mut args[i]) {
            Some(len) => arg_len[i] = len,
            None => {
                close_pipe_ends(&ends[..pipes]);
                write_console(b"pipeline: failed\n");
                LAST_STATUS.store(1, Ordering::Relaxed);
                prompt(cwd);
                return;
            }
        }
    }
    let bg = BACKGROUND.load(Ordering::Relaxed);
    let mut spawned = [0u64; STAGE_MAX];
    let mut spawned_n = 0usize;
    for i in 0..n {
        let stdin = if i > 0 { Some(ends[i - 1][0]) } else { None };
        let stdout = if i + 1 < n { Some(ends[i][1]) } else { None };
        let mut grants = pipe_grants(stdin, stdout);
        if bg {
            grants |= galexy_abi::SPAWN_NO_FG;
        }
        let res = spawn_with(progs[i], &args[i][..arg_len[i]], grants);
        if !res.ok {
            close_unmoved(i, n, &ends[..pipes]);
            for cap in &spawned[..spawned_n] {
                let cap = Cap::from_bits(*cap);
                let _ = kill(cap);
                let _ = wait(cap);
            }
            write_console(b"pipeline: failed\n");
            LAST_STATUS.store(1, Ordering::Relaxed);
            prompt(cwd);
            return;
        }
        spawned[spawned_n] = res.value;
        spawned_n += 1;
    }
    if bg {
        if !push_job(&spawned[..spawned_n]) {
            write_console(b"jobs: table full; waiting\n");
            wait_caps(&spawned[..spawned_n]);
        }
        prompt(cwd);
        return;
    }
    wait_caps(&spawned[..spawned_n]);
    prompt(cwd);
}

pub(crate) fn wait_caps(caps: &[u64]) {
    for (i, bits) in caps.iter().enumerate() {
        let waited = wait(Cap::from_bits(*bits));
        if i + 1 == caps.len() {
            LAST_STATUS.store(if waited.ok { waited.value } else { 1 }, Ordering::Relaxed);
        }
    }
}

pub(crate) fn close_pipe_ends(ends: &[[u64; 2]]) {
    for pair in ends {
        let _ = close(Cap::from_bits(pair[0]));
        let _ = close(Cap::from_bits(pair[1]));
    }
}

/// Closes pipe ends that stage `failed` and later stages have not moved yet.
pub(crate) fn close_unmoved(failed: usize, n: usize, ends: &[[u64; 2]]) {
    let pipes = n.saturating_sub(1);
    for (i, pair) in ends.iter().enumerate().take(pipes) {
        if i >= failed {
            let _ = close(Cap::from_bits(pair[1]));
        }
        if i + 1 >= failed {
            let _ = close(Cap::from_bits(pair[0]));
        }
    }
}

pub(crate) fn pipe_grants(stdin: Option<u64>, stdout: Option<u64>) -> u64 {
    let base =
        galexy_abi::SPAWN_INHERIT | galexy_abi::SPAWN_GRANT_QUERY | galexy_abi::SPAWN_WITH_CAPS;
    let (n0, n1) = match (stdin, stdout) {
        (Some(read_end), Some(write_end)) => (
            file_slot(Cap::from_bits(read_end)),
            file_slot(Cap::from_bits(write_end)),
        ),
        (Some(read_end), None) => (
            file_slot(Cap::from_bits(read_end)),
            galexy_abi::SPAWN_CAP_NONE,
        ),
        (None, Some(write_end)) => (
            file_slot(Cap::from_bits(write_end)),
            galexy_abi::SPAWN_CAP_NONE,
        ),
        (None, None) => return galexy_abi::SPAWN_INHERIT | galexy_abi::SPAWN_GRANT_QUERY,
    };
    base | (n0 << galexy_abi::SPAWN_CAP_SHIFT) | (n1 << (galexy_abi::SPAWN_CAP_SHIFT + 4))
}

pub(crate) fn stage_arg(
    cwd: &Cwd,
    prog: &[u8],
    rest: &[u8],
    has_in: bool,
    has_out: bool,
    out: &mut [u8; 256],
) -> Option<usize> {
    if prog == b"echo" {
        if has_in {
            return None;
        }
        if !has_out {
            return None;
        }
        if rest.len() + 2 > out.len() {
            return None;
        }
        out[0] = 3;
        out[1] = 0;
        out[2..2 + rest.len()].copy_from_slice(rest);
        return Some(2 + rest.len());
    }
    if prog == b"ls" {
        return ls_arg(cwd, rest, out);
    }
    if prog == b"grep" {
        return grep_arg(cwd, rest, has_in, out);
    }
    let paths = matches!(prog, b"cat" | b"head" | b"tail" | b"wc");
    join_stage(cwd, rest, has_in, paths, out)
}

/// Copies `line` into `out`, expanding `*` words against the files snapshot.
/// `None` when the expansion does not fit.
pub(crate) fn expanded_line(cwd: &Cwd, line: &[u8], out: &mut [u8; LINE_MAX]) -> Option<usize> {
    if !line.contains(&b'*') {
        if line.len() > out.len() {
            return None;
        }
        out[..line.len()].copy_from_slice(line);
        return Some(line.len());
    }
    let mut snap = [0u8; 1024];
    let got = read(files_cap(), &mut snap);
    let snap_n = if got.ok {
        (got.value as usize).min(snap.len())
    } else {
        0
    };
    let mut n = 0usize;
    let mut i = 0usize;
    let mut first = true;
    while i < line.len() {
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let start = i;
        while i < line.len() && line[i] != b' ' {
            i += 1;
        }
        let word = &line[start..i];
        if !first && !push_byte(out, &mut n, b' ') {
            return None;
        }
        first = false;
        if word.contains(&b'*') {
            let mut matched = false;
            let mut s = 0usize;
            while s < snap_n {
                let rest = &snap[s..snap_n];
                let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
                if let Some(name) = dir_entry(&cwd.buf[..cwd.len], &rest[..end]) {
                    if glob_one(word, name) {
                        if matched && !push_byte(out, &mut n, b' ') {
                            return None;
                        }
                        if !push_bytes(out, &mut n, name) {
                            return None;
                        }
                        matched = true;
                    }
                }
                s += end + 1;
            }
            if !matched && !push_bytes(out, &mut n, word) {
                return None;
            }
        } else if !push_bytes(out, &mut n, word) {
            return None;
        }
    }
    Some(n)
}

/// One `*` in `pat`. The name is one directory entry (no extra slash).
pub(crate) fn glob_one(pat: &[u8], name: &[u8]) -> bool {
    let Some(star) = pat.iter().position(|b| *b == b'*') else {
        return false;
    };
    if pat[star + 1..].contains(&b'*') {
        return false;
    }
    let prefix = &pat[..star];
    let suffix = &pat[star + 1..];
    let leaf = name.strip_suffix(b"/").unwrap_or(name);
    leaf.len() >= prefix.len() + suffix.len() && leaf.starts_with(prefix) && leaf.ends_with(suffix)
}

/// One child of `cwd` from a files-snapshot line. Same shape as `ls`.
pub(crate) fn dir_entry<'a>(cwd: &[u8], line: &'a [u8]) -> Option<&'a [u8]> {
    if line.is_empty() {
        return None;
    }
    if cwd.is_empty() {
        let slashes = line.iter().filter(|b| **b == b'/').count();
        if slashes == 0 || (slashes == 1 && line.ends_with(b"/")) {
            return Some(line);
        }
        return None;
    }
    if line.len() <= cwd.len() + 1 {
        return None;
    }
    if &line[..cwd.len()] != cwd || line[cwd.len()] != b'/' {
        return None;
    }
    let rest = &line[cwd.len() + 1..];
    let slashes = rest.iter().filter(|b| **b == b'/').count();
    if slashes == 0 || (slashes == 1 && rest.ends_with(b"/")) {
        Some(rest)
    } else {
        None
    }
}
