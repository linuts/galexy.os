//! Repository chores that do not fit a build script.
//!
//! `cargo xtask sysroot` ships the `x86_64-unknown-galexy` standard library
//! the way Xous does (`docs/RUSTC.md`, Stage 3): build `library/sysroot`
//! from the `std-pal/` overlay for the Galexy target, copy the `.rlib`s
//! into `lib/rustlib/x86_64-unknown-galexy/lib` of a toolchain directory
//! that otherwise mirrors the pinned nightly, and `rustup toolchain link`
//! it. Afterwards `cargo +galexy build --target x86_64-unknown-galexy`
//! needs no `-Zbuild-std`, no wrapper, and no JSON path: the target spec
//! ships inside the toolchain.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

const TARGET: &str = "x86_64-unknown-galexy";
const MARKER: &str = "galexy-toolchain.txt";

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  sysroot [--out DIR] [--name NAME] [--target-dir DIR] [--no-link] [--check]
      Build the x86_64-unknown-galexy std sysroot and link it as a rustup
      toolchain.
        --out DIR         toolchain directory (default target/galexy-toolchain)
        --name NAME       rustup toolchain name (default galexy)
        --target-dir DIR  cargo target dir for the std build
                          (default target/galexy-sysroot)
        --no-link         skip `rustup toolchain link`
        --check           compile a std hello world with the result through
                          rustc and cargo and validate the ELF like the loader
";

fn main() {
    if let Err(e) = run() {
        eprintln!("xtask: {e}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(|a| a.to_str()) {
        Some("sysroot") => sysroot(args.collect()),
        None | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n{USAGE}")),
    }
}

struct SysrootOpts {
    out: PathBuf,
    name: String,
    target_dir: PathBuf,
    link: bool,
    check: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask sits two levels below the repo root")
        .to_path_buf()
}

fn parse_sysroot_opts(args: Vec<OsString>) -> Result<SysrootOpts, String> {
    let root = repo_root();
    let mut opts = SysrootOpts {
        out: root.join("target/galexy-toolchain"),
        name: "galexy".to_string(),
        target_dir: root.join("target/galexy-sysroot"),
        link: true,
        check: false,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let arg = arg.to_string_lossy().into_owned();
        let mut value = |what: &str| -> Result<PathBuf, String> {
            it.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{what} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--out" => opts.out = absolute(value("--out")?),
            "--target-dir" => opts.target_dir = absolute(value("--target-dir")?),
            "--name" => opts.name = value("--name")?.to_string_lossy().into_owned(),
            "--no-link" => opts.link = false,
            "--check" => opts.check = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                process::exit(0);
            }
            other => return Err(format!("unknown option `{other}`\n{USAGE}")),
        }
    }
    Ok(opts)
}

fn absolute(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        env::current_dir().expect("cwd").join(p)
    }
}

/// The cargo that invoked us, and the rustc beside it (so the sysroot we
/// mirror is the one `rust-toolchain.toml` pinned, not whatever is on PATH).
fn tools() -> (PathBuf, PathBuf) {
    let cargo = env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| "cargo".into());
    let sibling = cargo.parent().map(|d| d.join("rustc"));
    let rustc = match sibling {
        Some(p) if p.is_file() => p,
        _ => env::var_os("RUSTC")
            .map(PathBuf::from)
            .unwrap_or_else(|| "rustc".into()),
    };
    (cargo, rustc)
}

fn output_of(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("{what}: cannot run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn status_of(cmd: &mut Command, what: &str) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("{what}: cannot run: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

fn sysroot(args: Vec<OsString>) -> Result<(), String> {
    let opts = parse_sysroot_opts(args)?;
    let root = repo_root();
    let (cargo, rustc) = tools();

    let real_sysroot = PathBuf::from(output_of(
        Command::new(&rustc).arg("--print").arg("sysroot"),
        "rustc --print sysroot",
    )?);
    let version = output_of(Command::new(&rustc).arg("--version"), "rustc --version")?;
    eprintln!("xtask: toolchain {version}");
    eprintln!("xtask: sysroot   {}", real_sysroot.display());

    // 1. The std source overlay: upstream `library/` plus `std-pal/`.
    let overlay = PathBuf::from(output_of(
        Command::new("python3")
            .arg(root.join("scripts/galexy-std-sysroot.py"))
            .arg("--out")
            .arg(root.join("target/sysroot-overlay"))
            .env("RUSTC", &rustc),
        "scripts/galexy-std-sysroot.py",
    )?);
    let library = overlay.join("lib/rustlib/src/rust/library");
    eprintln!("xtask: overlay   {}", overlay.display());

    // 2. A toolchain skeleton. rustc only treats the target as the plain
    //    triple `x86_64-unknown-galexy` when it loads the spec from its
    //    own sysroot; building against the JSON path stamps every rlib
    //    with a hashed triple (`x86_64-unknown-galexy-<hash>`) that later
    //    compiles then refuse. So the spec has to be installed first and
    //    the std build has to go through this toolchain's rustc.
    assemble(&opts.out, &real_sysroot, &root, &version, &overlay)?;

    // 3. `cargo build` of `library/sysroot`. The environment is what
    //    cargo's own `-Zbuild-std` sets: bootstrap gates open, stable
    //    crate metadata, `-Zforce-unstable-if-unmarked` so hashbrown's
    //    const fns count as unstable inside std, and the vendored
    //    dependency sources so the build is offline.
    let rlibs = build_std(
        &cargo,
        &opts.out.join("bin/rustc"),
        &library,
        &opts.target_dir,
    )?;
    install_rlibs(&opts.out, &rlibs)?;
    eprintln!("xtask: built     {} rlibs", rlibs.len());
    eprintln!("xtask: toolchain {}", opts.out.display());

    // 4. `rustup toolchain link`.
    if opts.link {
        status_of(
            Command::new("rustup")
                .arg("toolchain")
                .arg("link")
                .arg(&opts.name)
                .arg(&opts.out),
            "rustup toolchain link",
        )?;
        eprintln!(
            "xtask: linked    `rustup run {0} cargo build --target {TARGET}` / `cargo +{0} …`",
            opts.name
        );
    }

    // 5. Prove it: a std hello world through rustc and through cargo.
    if opts.check {
        check(&opts)?;
    }
    Ok(())
}

fn build_std(
    cargo: &Path,
    rustc: &Path,
    library: &Path,
    target_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let vendor = serde_json::to_string(&library.join("vendor")).expect("path is a string");
    let mut cmd = Command::new(cargo);
    cmd.arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(library.join("sysroot/Cargo.toml"))
        .arg("--target")
        .arg(TARGET)
        .arg("-Zbinary-dep-depinfo")
        .arg("--no-default-features")
        .arg("--features")
        .arg("compiler-builtins-mem")
        .arg("--config")
        .arg("source.crates-io.replace-with='vendored-sources'")
        .arg("--config")
        .arg(format!("source.vendored-sources.directory={vendor}"))
        .arg("--message-format=json-render-diagnostics")
        .env("RUSTC", rustc)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("__CARGO_DEFAULT_LIB_METADATA", "stablestd")
        .env("CARGO_TARGET_DIR", target_dir)
        .env("RUSTFLAGS", "-Zforce-unstable-if-unmarked")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("GALEXY_SYSROOT")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cargo build (sysroot): cannot run: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let mut rlibs = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("cargo build (sysroot): read: {e}"))?;
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if msg["reason"] != "compiler-artifact" {
            continue;
        }
        let name = msg["target"]["name"].as_str().unwrap_or("");
        if name == "sysroot" {
            // The aggregator crate itself is empty.
            continue;
        }
        for f in msg["filenames"].as_array().into_iter().flatten() {
            let Some(f) = f.as_str() else { continue };
            let p = PathBuf::from(f);
            let for_target = p.components().any(|c| c.as_os_str() == TARGET);
            if for_target && p.extension().is_some_and(|e| e == "rlib") {
                rlibs.push(p);
            }
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("cargo build (sysroot): {e}"))?;
    if !status.success() {
        return Err(format!("cargo build (sysroot) failed ({status})"));
    }
    if rlibs.iter().all(|p| !file_stem_starts(p, "libstd-")) {
        return Err("cargo build (sysroot) produced no libstd rlib".to_string());
    }
    rlibs.sort();
    rlibs.dedup();
    Ok(rlibs)
}

fn file_stem_starts(p: &Path, prefix: &str) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(prefix))
}

/// Front for `rustc`. The target spec shipped in this toolchain is still
/// a custom target, and rustc rejects `--target x86_64-unknown-galexy`
/// without `-Zunstable-options`. Inject it, then exec the real compiler
/// that sits next to this script so `$ORIGIN/../lib` — and therefore the
/// sysroot — stays this toolchain.
const RUSTC_WRAPPER: &str = "\
#!/bin/sh
# `cargo run` and some environments put the host toolchain `lib/` on
# LD_LIBRARY_PATH. The dynamic linker searches that before $ORIGIN, so
# this rustc would load the host librustc_driver and report the host
# sysroot. The driver lives next to this script; nothing else is needed.
unset LD_LIBRARY_PATH
unset DYLD_LIBRARY_PATH
here=$(CDPATH= cd -- \"$(dirname \"$0\")\" && pwd)
exec \"$here/rustc.real\" -Zunstable-options \"$@\"
";

/// `out` mirrors `real`. `bin/` and the shared libraries in `lib/` are
/// hardlinks, not symlinks: rustc derives its sysroot from the path of
/// the `librustc_driver` it loads (`$ORIGIN/../lib` of the binary it
/// execs), and both `$ORIGIN` and `dladdr` follow symlinks back to the
/// pinned nightly. `lib/rustlib/<TARGET>` is ours — the rlibs and
/// `target.json`. Everything else is a symlink.
fn assemble(
    out: &Path,
    real: &Path,
    root: &Path,
    version: &str,
    overlay: &Path,
) -> Result<(), String> {
    let io_err = |what: &str, e: io::Error| format!("{what}: {e}");
    if out.exists() {
        if !out.join(MARKER).is_file() {
            return Err(format!(
                "{} exists and is not a galexy toolchain directory (no {MARKER}); refusing to replace it",
                out.display()
            ));
        }
        fs::remove_dir_all(out).map_err(|e| io_err("remove old toolchain", e))?;
    }
    fs::create_dir_all(out).map_err(|e| io_err("create toolchain dir", e))?;

    for entry in fs::read_dir(real).map_err(|e| io_err("read sysroot", e))? {
        let entry = entry.map_err(|e| io_err("read sysroot", e))?;
        let name = entry.file_name();
        if name == "lib" || name == "bin" {
            continue;
        }
        symlink(&entry.path(), &out.join(&name))?;
    }

    let bin = out.join("bin");
    fs::create_dir(&bin).map_err(|e| io_err("create bin", e))?;
    for entry in fs::read_dir(real.join("bin")).map_err(|e| io_err("read bin", e))? {
        let entry = entry.map_err(|e| io_err("read bin", e))?;
        let mut dst_name = entry.file_name();
        if dst_name == "rustc" {
            dst_name = "rustc.real".into();
        }
        place_file(&entry.path(), &bin.join(&dst_name))?;
    }
    fs::write(bin.join("rustc"), RUSTC_WRAPPER).map_err(|e| io_err("write rustc wrapper", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(bin.join("rustc"))
            .map_err(|e| io_err("stat rustc wrapper", e))?
            .permissions();
        perm.set_mode(0o755);
        fs::set_permissions(bin.join("rustc"), perm)
            .map_err(|e| io_err("chmod rustc wrapper", e))?;
    }

    let lib = out.join("lib");
    fs::create_dir(&lib).map_err(|e| io_err("create lib", e))?;
    for entry in fs::read_dir(real.join("lib")).map_err(|e| io_err("read lib", e))? {
        let entry = entry.map_err(|e| io_err("read lib", e))?;
        let name = entry.file_name();
        if name == "rustlib" {
            continue;
        }
        place_file(&entry.path(), &lib.join(&name))?;
    }
    let rustlib = lib.join("rustlib");
    fs::create_dir(&rustlib).map_err(|e| io_err("create rustlib", e))?;
    for entry in fs::read_dir(real.join("lib/rustlib")).map_err(|e| io_err("read rustlib", e))? {
        let entry = entry.map_err(|e| io_err("read rustlib", e))?;
        let name = entry.file_name();
        if name == TARGET {
            continue;
        }
        symlink(&entry.path(), &rustlib.join(&name))?;
    }

    fs::create_dir_all(rustlib.join(TARGET).join("lib"))
        .map_err(|e| io_err("create target lib", e))?;
    fs::copy(
        root.join("targets").join(format!("{TARGET}.json")),
        rustlib.join(TARGET).join("target.json"),
    )
    .map_err(|e| io_err("copy target.json", e))?;

    let manifest = format!(
        "Galexy std toolchain, assembled by `cargo xtask sysroot`.\n\
         host toolchain: {version}\n\
         host sysroot:   {}\n\
         std overlay:    {}\n\
         target spec:    lib/rustlib/{TARGET}/target.json\n",
        real.display(),
        overlay.display()
    );
    fs::write(out.join(MARKER), manifest).map_err(|e| io_err("write marker", e))?;
    Ok(())
}

fn install_rlibs(out: &Path, rlibs: &[PathBuf]) -> Result<(), String> {
    let target_lib = out.join("lib/rustlib").join(TARGET).join("lib");
    fs::create_dir_all(&target_lib).map_err(|e| format!("create {}: {e}", target_lib.display()))?;
    let mut listed = String::from("rlibs:\n");
    for rlib in rlibs {
        let name = rlib.file_name().expect("rlib has a name");
        fs::copy(rlib, target_lib.join(name))
            .map_err(|e| format!("copy {}: {e}", rlib.display()))?;
        listed.push_str("  ");
        listed.push_str(&name.to_string_lossy());
        listed.push('\n');
        // This nightly writes full crate metadata to a sibling `.rmeta`
        // and leaves a stub (`lib.rmeta-link`) in the rlib. rustc refuses
        // the stub unless that sibling travels with it.
        let rmeta = rlib.with_extension("rmeta");
        if rmeta.is_file() {
            let rmeta_name = rmeta.file_name().expect("rmeta has a name");
            fs::copy(&rmeta, target_lib.join(rmeta_name))
                .map_err(|e| format!("copy {}: {e}", rmeta.display()))?;
            listed.push_str("  ");
            listed.push_str(&rmeta_name.to_string_lossy());
            listed.push('\n');
        }
    }
    let marker = out.join(MARKER);
    let mut text = fs::read_to_string(&marker).map_err(|e| format!("read marker: {e}"))?;
    text.push_str(&listed);
    fs::write(&marker, text).map_err(|e| format!("write marker: {e}"))
}

/// Hardlink a regular file so the directory entry lives in this toolchain.
/// A same-directory relative symlink is recreated (lib name aliases);
/// anything else would escape the tree and is an error. Cross-device
/// filesystems cannot hardlink, so those files are copied.
fn place_file(src: &Path, dst: &Path) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        let _ = (src, dst);
        return Err("cargo xtask sysroot needs a Unix host (hardlinks)".to_string());
    }
    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(src).map_err(|e| format!("stat {}: {e}", src.display()))?;
        if meta.file_type().is_symlink() {
            let target =
                fs::read_link(src).map_err(|e| format!("readlink {}: {e}", src.display()))?;
            if target.is_absolute() || target.components().any(|c| c.as_os_str() == "..") {
                return Err(format!(
                    "{} is a symlink pointing outside its directory; refusing to mirror it",
                    src.display()
                ));
            }
            return std::os::unix::fs::symlink(&target, dst)
                .map_err(|e| format!("symlink {} -> {}: {e}", dst.display(), target.display()));
        }
        if !meta.is_file() {
            return Err(format!("{} is not a file", src.display()));
        }
        match fs::hard_link(src, dst) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(src, dst)
                    .map(|_| ())
                    .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
                fs::set_permissions(dst, meta.permissions())
                    .map_err(|e| format!("chmod {}: {e}", dst.display()))
            }
            Err(e) => Err(format!(
                "hardlink {} -> {}: {e}",
                dst.display(),
                src.display()
            )),
        }
    }
}

fn symlink(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .map_err(|e| format!("symlink {} -> {}: {e}", link.display(), target.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err("cargo xtask sysroot needs a Unix host (symlinks)".to_string())
    }
}

const HELLO: &str = r#"use std::collections::HashMap;

fn main() {
    let mut m = HashMap::new();
    m.insert("hello", vec![1u32, 2, 3]);
    let t = std::thread::spawn(move || m["hello"].iter().sum::<u32>());
    println!("hello from the galexy toolchain: {}", t.join().unwrap());
}
"#;

fn check(opts: &SysrootOpts) -> Result<(), String> {
    let dir = opts.target_dir.join("check");
    let io_err = |what: &str, e: io::Error| format!("check: {what}: {e}");
    fs::create_dir_all(dir.join("src")).map_err(|e| io_err("mkdir", e))?;
    fs::write(dir.join("hello.rs"), HELLO).map_err(|e| io_err("write hello.rs", e))?;

    // Same link line the userspace crates pass from their build scripts:
    // a static image at the ABI load address.
    let image_base = format!("-Clink-arg=--image-base={:#x}", galexy_abi::USER_IMAGE_BASE);
    let rustflags = format!("{image_base} -Clink-arg=--no-pie");

    // rustc alone: the target is found by name through the sysroot.
    // `cargo run` puts the host toolchain's `lib/` on `LD_LIBRARY_PATH`
    // so this binary can find the host libstd. That variable is searched
    // before `$ORIGIN`, so a child rustc would load the host
    // `librustc_driver` and report the host sysroot. Drop it; rustc.real
    // finds its driver through `$ORIGIN/../lib`.
    let rustc_out = dir.join("hello-rustc");
    status_of(
        Command::new(opts.out.join("bin/rustc"))
            .arg("--target")
            .arg(TARGET)
            .arg("-O")
            .arg(&image_base)
            .arg("-Clink-arg=--no-pie")
            .arg("-o")
            .arg(&rustc_out)
            .arg(dir.join("hello.rs"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("DYLD_LIBRARY_PATH"),
        "check: rustc hello.rs",
    )?;
    validate(&rustc_out)?;
    eprintln!("xtask: check     rustc  ok ({})", rustc_out.display());

    // cargo: a plain package, no `-Zbuild-std`, no wrapper.
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"hello-check\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .map_err(|e| io_err("write Cargo.toml", e))?;
    fs::write(dir.join("src/main.rs"), HELLO).map_err(|e| io_err("write main.rs", e))?;
    let cargo_target = dir.join("target");
    let mut cmd = if opts.link {
        let mut c = Command::new("rustup");
        c.arg("run").arg(&opts.name).arg("cargo");
        c
    } else {
        let mut c = Command::new(opts.out.join("bin/cargo"));
        c.env("RUSTC", opts.out.join("bin/rustc"));
        c
    };
    status_of(
        cmd.arg("build")
            .arg("--release")
            .arg("--target")
            .arg(TARGET)
            .arg("--manifest-path")
            .arg(dir.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &cargo_target)
            .env("RUSTFLAGS", &rustflags)
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("DYLD_LIBRARY_PATH"),
        "check: cargo build",
    )?;
    let cargo_out = cargo_target.join(TARGET).join("release/hello-check");
    validate(&cargo_out)?;
    eprintln!("xtask: check     cargo  ok ({})", cargo_out.display());
    Ok(())
}

/// What `sched/loader.rs` accepts: ELF64 `ET_EXEC`, entry inside an
/// executable `PT_LOAD`, every load W^X and inside the 512 MiB window
/// above [`galexy_abi::USER_IMAGE_BASE`], page ranges disjoint. `rust-lld`
/// does not page-align each segment's `p_vaddr` (the first bytes of a
/// segment can share their page with the previous one); the loader maps
/// whole pages and allows that, so this check does too.
fn validate(elf: &Path) -> Result<(), String> {
    let bytes = fs::read(elf).map_err(|e| format!("check: read {}: {e}", elf.display()))?;
    loadable(&bytes).map_err(|detail| {
        format!(
            "check: {} is not a loadable Galexy image: {detail}",
            elf.display()
        )
    })
}

fn loadable(bytes: &[u8]) -> Result<(), String> {
    const WINDOW: u64 = 512 * 1024 * 1024;
    const PAGE: u64 = 4096;
    const PT_LOAD: u32 = 1;
    const PF_X: u32 = 1;
    const PF_W: u32 = 2;
    if bytes.len() < 64 {
        return Err("ELF too small".to_string());
    }
    if &bytes[0..4] != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
        return Err("need ELF64 little-endian".to_string());
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    if u16_at(16) != 2 {
        return Err("need ET_EXEC".to_string());
    }
    let entry = u64_at(24);
    let base = galexy_abi::USER_IMAGE_BASE;
    let window_hi = base + WINDOW;
    if entry < base || entry >= window_hi {
        return Err(format!("entry {entry:#x} outside the image window"));
    }
    let phoff = u64_at(32) as usize;
    let phentsize = u16_at(54) as usize;
    let phnum = u16_at(56) as usize;
    if phentsize != 56 || phnum == 0 {
        return Err("bad program header table".to_string());
    }
    let mut loads: Vec<(u64, u64)> = Vec::new();
    let mut entry_in_text = false;
    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + 56 > bytes.len() {
            return Err("phdr truncated".to_string());
        }
        if u32_at(off) != PT_LOAD {
            continue;
        }
        let flags = u32_at(off + 4);
        let offset = u64_at(off + 8);
        let vaddr = u64_at(off + 16);
        let filesz = u64_at(off + 32);
        let memsz = u64_at(off + 40);
        if filesz > memsz || offset.saturating_add(filesz) > bytes.len() as u64 {
            return Err("PT_LOAD sizes exceed the file".to_string());
        }
        if memsz == 0 {
            continue;
        }
        let end = vaddr
            .checked_add(memsz)
            .ok_or_else(|| "PT_LOAD end overflows".to_string())?;
        if vaddr < base || end > window_hi {
            return Err(format!(
                "PT_LOAD {vaddr:#x}..{end:#x} outside the image window"
            ));
        }
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err("W|X PT_LOAD forbidden (W^X)".to_string());
        }
        let first = vaddr / PAGE;
        let last = (end - 1) / PAGE;
        if loads.iter().any(|&(f, l)| first <= l && f <= last) {
            return Err("PT_LOAD segments overlap".to_string());
        }
        loads.push((first, last));
        if flags & PF_X != 0 && entry >= vaddr && entry < end {
            entry_in_text = true;
        }
    }
    if loads.is_empty() || !entry_in_text {
        return Err("entry is not in an executable PT_LOAD".to_string());
    }
    Ok(())
}

/// Milestone 91. `scripts/build-galexy-python.sh` writes
/// `target/galexy-python/python`. CI does not fetch RustPython, so a
/// missing file is a skip. When the file is there it has to pass the
/// same check as `cargo xtask sysroot --check`.
#[cfg(test)]
mod python_elf {
    use super::loadable;

    #[test]
    fn pinned_python_elf_is_loadable() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/galexy-python/python");
        if !path.exists() {
            eprintln!(
                "pinned_python_elf_is_loadable: skipped (no {})",
                path.display()
            );
            return;
        }
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        loadable(&bytes).unwrap_or_else(|detail| {
            panic!(
                "{} is not a loadable Galexy image: {detail}",
                path.display()
            )
        });
    }
}
