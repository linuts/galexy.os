use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "login_compilers.rs"]
mod login_compilers;

fn main() {
    watch_userspace();
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());

    // The main kernel binary comes from the artifact dependency; test kernels
    // are the additional bins in the kernel crate's src/bin directory.
    let mut bins: Vec<(String, PathBuf)> = Vec::new();
    if let Some(path) = std::env::var_os("CARGO_BIN_FILE_GALEXY_OS_galexy-os") {
        bins.push(("galexy-os".into(), path.into()));
    }
    if let Ok(bin_dir) =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../galexy-os/src/bin"))
    {
        for entry in bin_dir.flatten() {
            let stem = entry
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let var = format!("CARGO_BIN_FILE_GALEXY_OS_{}", stem);
            if let Some(path) = std::env::var_os(&var) {
                bins.push((stem, path.into()));
            }
        }
    }

    // One manifest line per kernel binary: `name:bios_path,uefi_path`.
    let mut manifest = String::new();
    // Ramdisk: a tar archive handed to the bootloader via set_ramdisk;
    // the kernel reads it at boot through BootInfo.ramdisk_addr.
    let ramdisk_path = out_dir.join("ramdisk.tar");
    {
        // Host-side context for each file (name → bytes).
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        // Userspace ELFs for `x86_64-unknown-galexy` (`build.rs` invokes
        // cargo; they are not artifact deps — see `build_userspace`).
        for (name, bytes) in build_userspace(&out_dir) {
            entries.push((name, bytes));
        }
        // Standing file: proof-of-plumbing marker for tests.
        entries.push((
            "banner.txt".into(),
            b"galexy ramdisk plumbing works".to_vec(),
        ));

        let mut tar = tar::Builder::new(std::fs::File::create(&ramdisk_path).unwrap());
        for (name, bytes) in &entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, bytes as &[u8]).unwrap();
        }
        tar.finish().unwrap();
    }

    // Supervisor e2e ramdisk: same entries, shell rebuilt with crash-seam.
    let crash_ramdisk = out_dir.join("ramdisk-crash.tar");
    {
        let crash_shell = build_crash_shell(&out_dir);
        write_ramdisk_variant(&ramdisk_path, &crash_ramdisk, |name, body| {
            if name == "shell" {
                crash_shell.clone()
            } else {
                body
            }
        });
    }

    // Milestone 69 differential: every program re-linked by `gxld` (same
    // objects, same rlibs, `-Clinker` swapped). Boot tests on this image
    // must behave exactly like the rust-lld image.
    let gxld_ramdisk = out_dir.join("ramdisk-gxld.tar");
    {
        let gxld_bins = build_gxld_userspace(&out_dir);
        let mut swapped = 0usize;
        write_ramdisk_variant(&ramdisk_path, &gxld_ramdisk, |name, body| {
            match gxld_bins.get(name) {
                Some(bytes) => {
                    swapped += 1;
                    bytes.clone()
                }
                None => body,
            }
        });
        assert!(
            swapped >= 4,
            "gxld ramdisk: expected to swap hello/init/shell/util, swapped {swapped}"
        );
    }

    // Ramdisk measurement (Milestone 51): the SHA-256 of each packed tar is
    // printed at build time and handed to the tests, which compare it with
    // what the kernel measures at boot (`test-ramdisk`). The ramdisk is
    // TRUSTED input — the kernel loads whatever the build packed; this hash
    // is how a reviewer checks that is what they built, not an allowlist.
    for (label, path, env_name) in [
        ("ramdisk", &ramdisk_path, "GALEXY_RAMDISK_SHA256"),
        (
            "ramdisk-crash",
            &crash_ramdisk,
            "GALEXY_RAMDISK_CRASH_SHA256",
        ),
        ("ramdisk-gxld", &gxld_ramdisk, "GALEXY_RAMDISK_GXLD_SHA256"),
    ] {
        let bytes = std::fs::read(path).unwrap();
        let hex = hex_digest(&galexy_crypto::sha256(&bytes));
        let files = count_tar_files(&bytes);
        println!(
            "cargo:warning={label}.tar sha256={hex} ({} bytes, {files} files)",
            bytes.len()
        );
        println!("cargo:rustc-env={env_name}={hex}");
        std::fs::write(
            path.with_extension("sha256"),
            format!("{hex}  {label}.tar\n"),
        )
        .unwrap();
    }

    // `cargo run` builds python and rustc when the ELFs are missing, then
    // the desk image below picks them up. `cargo test` does not: CI keeps
    // the small ramdisk.
    ensure_login_compilers();

    // Milestone 72: the shared ramdisk stays the userspace set. The
    // compiler ramdisk is `test-rustc` / `test-rustc-hello` only.
    // Milestone 74 adds `galexy-os-rustc`: the normal userspace plus
    // `rustc`, `gxld`, the target JSON, and `srlib/*`.
    let rustc_ramdisk = out_dir.join("ramdisk-rustc.tar");
    let have_rustc = write_rustc_ramdisk(&rustc_ramdisk);
    println!(
        "cargo:rustc-env=GALEXY_HAVE_RUSTC={}",
        if have_rustc { "1" } else { "0" }
    );

    // Milestone 93: `python` is ~64 MiB. It rides on `test-python` and,
    // when the ELF exists, on `galexy-os-python` (Milestone 95). It stays
    // off the shared ramdisk. Absent ELF means CI still builds the small
    // image; the Python boots skip.
    let python_ramdisk = out_dir.join("ramdisk-python.tar");
    let have_python = write_python_ramdisk(&python_ramdisk);
    println!(
        "cargo:rustc-env=GALEXY_HAVE_PYTHON={}",
        if have_python { "1" } else { "0" }
    );

    for (name, kernel) in &bins {
        let bios_path = out_dir.join(format!("{}-bios.img", name));
        let uefi_path = out_dir.join(format!("{}-uefi.img", name));
        let ramdisk = if have_rustc && (name == "test-rustc" || name == "test-rustc-hello") {
            &rustc_ramdisk
        } else if have_python && name == "test-python" {
            &python_ramdisk
        } else {
            &ramdisk_path
        };

        bootloader::BiosBoot::new(kernel)
            .set_ramdisk(ramdisk)
            .create_disk_image(&bios_path)
            .unwrap();
        bootloader::UefiBoot::new(kernel)
            .set_ramdisk(ramdisk)
            .create_disk_image(&uefi_path)
            .unwrap();

        if !manifest.is_empty() {
            manifest.push(';');
        }
        manifest.push_str(&format!(
            "{}:{},{}",
            name,
            bios_path.display(),
            uefi_path.display()
        ));
    }

    if let Some((_, kernel)) = bins.iter().find(|(name, _)| name == "galexy-os") {
        let mut variants: Vec<(&str, &Path)> = vec![
            ("galexy-os-crashseam", &crash_ramdisk),
            ("galexy-os-gxld", &gxld_ramdisk),
        ];
        let login_rustc = out_dir.join("ramdisk-rustc-login.tar");
        if have_rustc {
            append_ramdisk(
                &ramdisk_path,
                &rustc_ramdisk,
                &login_rustc,
                keep_rustc_login,
                5,
            );
            variants.push(("galexy-os-rustc", &login_rustc));
        }
        // Milestone 95: `python` stays off the shared ramdisk. The login
        // image exists only when the ELF was present at this build.
        let login_python = out_dir.join("ramdisk-python-login.tar");
        if have_python {
            append_ramdisk(
                &ramdisk_path,
                &python_ramdisk,
                &login_python,
                |name| name == "python",
                1,
            );
            variants.push(("galexy-os-python", &login_python));
        }
        // `cargo run` boots this image when either ELF was built. Test
        // kernels keep the small ramdisk. CI, which has neither ELF,
        // still builds `galexy-os` and skips the desk.
        let desk = out_dir.join("ramdisk-desk.tar");
        let have_desk = if have_rustc && have_python {
            let mid = out_dir.join("ramdisk-desk-rustc.tar");
            append_ramdisk(&ramdisk_path, &rustc_ramdisk, &mid, keep_rustc_login, 5);
            append_ramdisk(&mid, &python_ramdisk, &desk, |name| name == "python", 1);
            true
        } else if have_rustc {
            append_ramdisk(&ramdisk_path, &rustc_ramdisk, &desk, keep_rustc_login, 5);
            true
        } else if have_python {
            append_ramdisk(
                &ramdisk_path,
                &python_ramdisk,
                &desk,
                |name| name == "python",
                1,
            );
            true
        } else {
            false
        };
        println!(
            "cargo:rustc-env=GALEXY_HAVE_DESK={}",
            if have_desk { "1" } else { "0" }
        );
        if have_desk {
            variants.push(("galexy-os-desk", &desk));
        }
        for (variant, ramdisk) in variants {
            let bios_path = out_dir.join(format!("{variant}-bios.img"));
            let uefi_path = out_dir.join(format!("{variant}-uefi.img"));
            bootloader::BiosBoot::new(kernel)
                .set_ramdisk(ramdisk)
                .create_disk_image(&bios_path)
                .unwrap();
            bootloader::UefiBoot::new(kernel)
                .set_ramdisk(ramdisk)
                .create_disk_image(&uefi_path)
                .unwrap();
            if !manifest.is_empty() {
                manifest.push(';');
            }
            manifest.push_str(&format!(
                "{variant}:{},{}",
                bios_path.display(),
                uefi_path.display()
            ));
        }
    } else {
        println!("cargo:rustc-env=GALEXY_HAVE_DESK=0");
    }

    println!("cargo:rustc-env=GALEXY_IMAGES={}", manifest);
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gxld/src")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../galexy-os/src")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../userspace/shell/src")
            .display()
    );
}

/// Packs `target/galexy-rustc` into a tar the `test-rustc` image boots.
///
/// Entries are the compiler ELF (`rustc`), the target JSON, and the
/// sysroot `.rlib` / `.rmeta` files as `srlib/<file>`. Metadata is
/// separate because bootstrap builds with `-Zembed-metadata=no`.
/// Returns false when the ELF is absent so CI can build the image
/// without a stage-1 compiler.
fn write_rustc_ramdisk(dest: &Path) -> bool {
    let root = repo_root().join("target/galexy-rustc");
    let bin = root.join("rustc");
    println!("cargo:rerun-if-changed={}", bin.display());
    if !bin.is_file() {
        return false;
    }
    let elf = std::fs::read(&bin).unwrap_or_else(|e| panic!("read {}: {e}", bin.display()));
    assert!(elf.starts_with(b"\x7fELF"), "galexy rustc is not an ELF");
    let spec = repo_root().join("targets/x86_64-unknown-galexy.json");
    let spec_bytes =
        std::fs::read(&spec).unwrap_or_else(|e| panic!("read {}: {e}", spec.display()));
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    entries.push(("rustc".into(), elf));
    entries.push(("x86_64-unknown-galexy.json".into(), spec_bytes));
    let rustlib = root.join("sysroot/lib/rustlib");
    println!("cargo:rerun-if-changed={}", rustlib.display());
    let mut rlibs = 0usize;
    if let Ok(triples) = std::fs::read_dir(&rustlib) {
        for triple in triples.flatten() {
            let triple_name = triple.file_name();
            let Some(triple_name) = triple_name.to_str() else {
                continue;
            };
            if !triple_name.starts_with("x86_64-unknown-galexy") {
                continue;
            }
            let Ok(dir) = std::fs::read_dir(triple.path().join("lib")) else {
                continue;
            };
            for entry in dir.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str());
                if ext != Some("rlib") && ext != Some("rmeta") {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let bytes =
                    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                entries.push((format!("srlib/{name}"), bytes));
                rlibs += 1;
            }
        }
    }
    assert!(
        rlibs > 0,
        "sysroot rlibs missing under {}",
        rustlib.display()
    );
    let gxld = build_guest_gxld(dest.parent().unwrap());
    entries.push(("gxld".into(), gxld));
    let refs = link_reference_hellos(dest.parent().unwrap());
    entries.extend(refs);
    let mut tar = tar::Builder::new(std::fs::File::create(dest).unwrap());
    for (name, bytes) in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes as &[u8]).unwrap();
    }
    tar.finish().unwrap();
    true
}

/// Packs `target/galexy-python/python` for `test-python` only.
///
/// Returns false when the ELF is absent so CI can build the image
/// without fetching RustPython.
fn write_python_ramdisk(dest: &Path) -> bool {
    let bin = repo_root().join("target/galexy-python/python");
    println!("cargo:rerun-if-changed={}", bin.display());
    if !bin.is_file() {
        return false;
    }
    let elf = std::fs::read(&bin).unwrap_or_else(|e| panic!("read {}: {e}", bin.display()));
    assert!(elf.starts_with(b"\x7fELF"), "galexy python is not an ELF");
    let mut tar = tar::Builder::new(std::fs::File::create(dest).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(elf.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, "python", elf.as_slice())
        .unwrap();
    tar.finish().unwrap();
    true
}

/// Ring-3 `gxld` the on-OS compiler spawns. Same std overlay as `stdmin`.
fn build_guest_gxld(out_dir: &Path) -> Vec<u8> {
    let sysroot = galexy_std_sysroot(out_dir);
    let std_target = out_dir.join("std-target");
    let manifest = repo_root().join("crates/gxld/Cargo.toml");
    let release = is_release();
    let mut cmd = nested_cargo();
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&std_target)
        .arg("--bin")
        .arg("gxld");
    galexy_cargo_args(&mut cmd, "std,panic_abort");
    if release {
        cmd.arg("--release");
    }
    let _ = std::fs::remove_file(std_target.join(".rustc_info.json"));
    cmd.env(
        "RUSTC_WRAPPER",
        repo_root().join("scripts/galexy-rustc-wrapper.py"),
    );
    cmd.env("GALEXY_SYSROOT", &sysroot);
    cmd.env_remove("RUSTFLAGS");
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("spawn cargo for guest gxld: {e}"));
    if !status.success() {
        panic!("guest gxld build failed");
    }
    let profile = if release { "release" } else { "debug" };
    let elf = std_target
        .join(galexy_target_dir_name())
        .join(profile)
        .join("gxld");
    let bytes = std::fs::read(&elf).unwrap_or_else(|e| panic!("read {}: {e}", elf.display()));
    assert!(bytes.starts_with(b"\x7fELF"), "guest gxld is not an ELF");
    bytes
}

/// Host cg_clif links of the same hello, one with `rust-lld` and one with
/// `gxld`. The guest boots both before the on-OS compile.
fn link_reference_hellos(out_dir: &Path) -> Vec<(String, Vec<u8>)> {
    let gxld_target = out_dir.join("gxld-target");
    let gxld_manifest = repo_root().join("crates/gxld/Cargo.toml");
    let mut cmd = nested_cargo();
    cmd.arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(&gxld_manifest)
        .arg("--target-dir")
        .arg(&gxld_target);
    let status = cmd.status().expect("spawn cargo for host gxld");
    if !status.success() {
        panic!("host gxld build failed");
    }
    let gxld = gxld_target.join("release").join("gxld");
    let libs =
        repo_root().join("target/galexy-rustc/sysroot/lib/rustlib/x86_64-unknown-galexy/lib");
    let have_std = std::fs::read_dir(&libs).ok().is_some_and(|rd| {
        rd.filter_map(|e| e.ok()).any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("libstd-") && name.ends_with(".rlib")
        })
    });
    assert!(
        have_std,
        "std rlib missing under {} (scripts/build-galexy-rustc.sh)",
        libs.display()
    );
    let json = galexy_target();
    let src_dir = out_dir.join("rustc-hello-src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let hello = src_dir.join("hello.rs");
    std::fs::write(
        &hello,
        "fn main() {\n    println!(\"hello from rustc on galexy\");\n}\n",
    )
    .unwrap();
    let lld = link_one(&hello, &src_dir.join("hello-lld"), &libs, &json, None);
    let linked = link_one(
        &hello,
        &src_dir.join("hello-gxld"),
        &libs,
        &json,
        Some(gxld.as_path()),
    );
    vec![("hello-lld".into(), lld), ("hello-gxld".into(), linked)]
}

fn link_one(src: &Path, out: &Path, libs: &Path, json: &Path, gxld: Option<&Path>) -> Vec<u8> {
    let mut cmd = Command::new("rustc");
    cmd.arg("-Zunstable-options")
        .arg("-Zcodegen-backend=cranelift")
        .arg("--target")
        .arg(json)
        .arg("-L")
        .arg(libs)
        .arg("--edition")
        .arg("2021")
        .arg("-C")
        .arg(format!(
            "link-arg=--image-base={:#x}",
            galexy_abi::USER_IMAGE_BASE
        ))
        .arg("-C")
        .arg("link-arg=--no-pie")
        .arg("-o")
        .arg(out)
        .arg(src);
    if let Some(gxld) = gxld {
        cmd.arg("-C")
            .arg(format!("linker={}", gxld.display()))
            .arg("-C")
            .arg("linker-flavor=ld");
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("spawn rustc for {}: {e}", out.display()));
    if !status.success() {
        panic!("reference link failed: {}", out.display());
    }
    let bytes = std::fs::read(out).unwrap_or_else(|e| panic!("read {}: {e}", out.display()));
    assert!(
        bytes.starts_with(b"\x7fELF"),
        "{} is not an ELF",
        out.display()
    );
    bytes
}

fn keep_rustc_login(name: &str) -> bool {
    name == "rustc"
        || name == "gxld"
        || name == "x86_64-unknown-galexy.json"
        || name.starts_with("srlib/")
}

/// `base` plus the entries of `extra` that `keep` accepts. A name that
/// already exists in `base` is a build bug: the login image must not
/// silently replace `hello` or `shell`. `min_added` is how many extra
/// names this image must gain (`python` is one; the compiler image is
/// `rustc`, `gxld`, the target JSON, and the sysroot).
fn append_ramdisk(
    base: &Path,
    extra: &Path,
    dest: &Path,
    keep: impl Fn(&str) -> bool,
    min_added: usize,
) {
    let mut entries = read_tar(base);
    let mut names: std::collections::BTreeSet<String> =
        entries.iter().map(|(name, _)| name.clone()).collect();
    let mut added = 0usize;
    for (name, body) in read_tar(extra) {
        if !keep(&name) {
            continue;
        }
        assert!(
            names.insert(name.clone()),
            "ramdisk name collision while building {}: {name}",
            dest.display()
        );
        entries.push((name, body));
        added += 1;
    }
    assert!(
        added >= min_added,
        "login ramdisk {} added {added} entries, wanted at least {min_added}",
        dest.display()
    );
    let mut tar = tar::Builder::new(std::fs::File::create(dest).unwrap());
    for (name, body) in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, body as &[u8]).unwrap();
    }
    tar.finish().unwrap();
}

fn read_tar(path: &Path) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).unwrap();
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut entries = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut body).unwrap();
        entries.push((name, body));
    }
    entries
}

/// Re-pack `source` as `dest`, letting `swap(name, body)` replace entries.
fn write_ramdisk_variant(
    source: &Path,
    dest: &Path,
    mut swap: impl FnMut(&str, Vec<u8>) -> Vec<u8>,
) {
    let bytes = std::fs::read(source).unwrap();
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut body).unwrap();
        let body = swap(&name, body);
        entries.push((name, body));
    }
    let mut tar = tar::Builder::new(std::fs::File::create(dest).unwrap());
    for (name, body) in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, body as &[u8]).unwrap();
    }
    tar.finish().unwrap();
}

fn hex_digest(digest: &[u8; 32]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Regular-file entries in a USTAR archive (header walk, no extraction).
fn count_tar_files(tar: &[u8]) -> usize {
    let mut count = 0usize;
    let mut off = 0usize;
    while off + 512 <= tar.len() {
        let header = &tar[off..off + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size_field = &header[124..136];
        let size_str = std::str::from_utf8(size_field)
            .unwrap_or("")
            .trim_end_matches(['\0', ' '])
            .trim_start_matches(' ');
        let size = usize::from_str_radix(size_str.trim_end_matches('\0'), 8).unwrap_or(0);
        if header[156] == b'0' || header[156] == 0 {
            count += 1;
        }
        off += 512 + size.div_ceil(512) * 512;
    }
    count
}

/// A nested `cargo` with this build script's package-specific environment
/// scrubbed, so it neither inherits our features nor fights our lock.
fn nested_cargo() -> Command {
    let mut cmd = Command::new(env!("CARGO"));
    scrub_cargo_env(&mut cmd);
    cmd
}

fn scrub_cargo_env(cmd: &mut Command) {
    for (key, _) in std::env::vars() {
        if key.starts_with("CARGO_FEATURE_")
            || key.starts_with("CARGO_BIN_")
            || key.starts_with("CARGO_PKG_")
            || key.starts_with("CARGO_ENCODED_")
            || key == "CARGO_MANIFEST_DIR"
            || key == "CARGO_MANIFEST_PATH"
            || key == "CARGO_CRATE_NAME"
            || key == "CARGO_PRIMARY_PACKAGE"
            || key.starts_with("DEP_")
        {
            cmd.env_remove(&key);
        }
    }
    cmd.env_remove("CARGO_TARGET_DIR");
}

/// Build the login compilers when this process is `cargo run` and the
/// ELFs are not already in `target/`. Tests and CI take the other branch
/// and keep the small image.
fn ensure_login_compilers() {
    println!("cargo:rerun-if-env-changed=GALEXY_SKIP_COMPILERS");
    println!("cargo:rerun-if-env-changed=GALEXY_BUILD_COMPILERS");
    let skip = std::env::var_os("GALEXY_SKIP_COMPILERS").is_some();
    let force = std::env::var_os("GALEXY_BUILD_COMPILERS").is_some();
    let sub = cargo_subcommand();
    if !login_compilers::want_login_compilers(skip, force, sub.as_deref()) {
        return;
    }
    eprintln!("[runner] cargo run will build any missing login compiler (python, rustc)");
    let root = repo_root();
    if !root.join("target/galexy-rustc/rustc").is_file() {
        eprintln!("[runner] building rustc for cargo run");
        run_host_script("scripts/build-galexy-rustc.sh");
    }
    if !galexy_toolchain_linked() {
        eprintln!("[runner] linking the galexy sysroot");
        run_xtask_sysroot();
    }
    if !root.join("target/galexy-python/python").is_file() {
        eprintln!("[runner] building python for cargo run");
        run_host_script("scripts/build-galexy-python.sh");
    }
}

fn cargo_subcommand() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let ppid = status.lines().find_map(|line| {
        let rest = line.strip_prefix("PPid:")?;
        Some(rest.trim().to_string())
    })?;
    let raw = std::fs::read(format!("/proc/{ppid}/cmdline")).ok()?;
    let args: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
    let sub = login_compilers::positional_subcommand(&args)?;
    Some(String::from_utf8_lossy(sub).into_owned())
}

fn galexy_toolchain_linked() -> bool {
    let Ok(out) = Command::new("rustup").args(["toolchain", "list"]).output() else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.split_whitespace().next() == Some("galexy"))
}

fn run_host_script(rel: &str) {
    let mut cmd = Command::new("bash");
    cmd.arg(repo_root().join(rel));
    cmd.current_dir(repo_root());
    scrub_cargo_env(&mut cmd);
    // The python script's load check is `cargo test` in this workspace,
    // which would wait on the lock `cargo run` already holds.
    cmd.env("GALEXY_SKIP_PYTHON_CHECK", "1");
    let status = cmd.status().unwrap_or_else(|e| panic!("spawn {rel}: {e}"));
    if !status.success() {
        panic!("{rel} failed ({status})");
    }
}

fn run_xtask_sysroot() {
    let mut cmd = nested_cargo();
    cmd.current_dir(repo_root());
    cmd.arg("run")
        .arg("--quiet")
        .arg("-p")
        .arg("xtask")
        .arg("--target-dir")
        .arg(repo_root().join("target/xtask-login"))
        .arg("--")
        .arg("sysroot");
    let status = cmd.status().expect("spawn cargo xtask sysroot");
    if !status.success() {
        panic!("cargo xtask sysroot failed ({status})");
    }
}

fn is_release() -> bool {
    std::env::var("PROFILE").as_deref() == Ok("release")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn galexy_target() -> PathBuf {
    repo_root().join("targets/x86_64-unknown-galexy.json")
}

/// Directory cargo uses for a JSON target: the file stem.
fn galexy_target_dir_name() -> &'static str {
    "x86_64-unknown-galexy"
}

fn watch_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                watch_tree(&path);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

fn watch_userspace() {
    watch_tree(&repo_root().join("crates/userspace"));
    // The std PAL and the ABI it is generated from feed the std overlay.
    watch_tree(&repo_root().join("std-pal"));
    watch_tree(&repo_root().join("crates/galexy-abi/src"));
    println!("cargo:rerun-if-changed={}", galexy_target().display());
    println!(
        "cargo:rerun-if-changed={}",
        repo_root().join("scripts/galexy-std-sysroot.py").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root()
            .join("scripts/galexy-rustc-wrapper.py")
            .display()
    );
}

/// `-Zbuild-std` for the galexy JSON target. `core,alloc` for `no_std`
/// programs; `std,panic_abort` for `stdmin`.
fn galexy_cargo_args(cmd: &mut Command, build_std: &str) {
    cmd.arg("--target")
        .arg(galexy_target())
        .arg(format!("-Zbuild-std={build_std}"))
        .arg("-Zbuild-std-features=compiler-builtins-mem")
        .arg("-Zjson-target-spec");
}

fn galexy_std_sysroot(out_dir: &Path) -> PathBuf {
    let script = repo_root().join("scripts/galexy-std-sysroot.py");
    let overlay = out_dir.join("galexy-std-overlay");
    let output = Command::new("python3")
        .arg(&script)
        .arg("--out")
        .arg(&overlay)
        .output()
        .unwrap_or_else(|e| panic!("spawn galexy-std-sysroot: {e}"));
    if !output.status.success() {
        panic!(
            "galexy-std-sysroot failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let path = String::from_utf8(output.stdout).expect("sysroot path is utf-8");
    PathBuf::from(path.trim())
}

fn read_elf_dir(bin_dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut bins = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(bin_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", bin_dir.display()))
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() || path.extension().is_some() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if bytes.starts_with(b"\x7fELF") {
            bins.insert(name, bytes);
        }
    }
    bins
}

/// `no_std` programs built with `-Zbuild-std=core,alloc`.
const NOSTD_PACKAGES: [&str; 6] = ["hello", "init", "shell", "util", "threadtest", "gxldos"];
/// `std` programs built with `-Zbuild-std=std,panic_abort` on the Galexy
/// PAL overlay. They share one target dir (one std build), separate from
/// the `no_std` one.
const STD_PACKAGES: [&str; 3] = ["stdmin", "stdtest", "bigtest"];

/// [`NOSTD_PACKAGES`] plus [`STD_PACKAGES`], as ramdisk entries.
fn build_userspace(out_dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let target_dir = out_dir.join("userspace-target");
    let workspace = repo_root().join("Cargo.toml");
    let release = is_release();
    let mut cmd = nested_cargo();
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&workspace)
        .arg("--target-dir")
        .arg(&target_dir);
    galexy_cargo_args(&mut cmd, "core,alloc");
    for pkg in NOSTD_PACKAGES {
        cmd.arg("-p").arg(pkg);
    }
    if release {
        cmd.arg("--release");
    }
    cmd.env_remove("RUSTC_WRAPPER");
    cmd.env_remove("GALEXY_SYSROOT");
    let status = cmd.status().expect("spawn cargo for galexy userspace");
    if !status.success() {
        panic!("galexy userspace build failed");
    }

    let profile = if release { "release" } else { "debug" };
    let mut bins = read_elf_dir(&target_dir.join(galexy_target_dir_name()).join(profile));

    let sysroot = galexy_std_sysroot(out_dir);
    let std_target = out_dir.join("std-target");
    for pkg in STD_PACKAGES {
        let manifest = repo_root().join(format!("crates/userspace/{pkg}/Cargo.toml"));
        let mut cmd = nested_cargo();
        cmd.arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&std_target);
        galexy_cargo_args(&mut cmd, "std,panic_abort");
        if release {
            cmd.arg("--release");
        }
        // Cargo caches `rustc --print sysroot` per (rustc, wrapper) in this
        // file; it does not key on `GALEXY_SYSROOT`, so a regenerated overlay
        // (new content hash, old one removed) would otherwise be invisible.
        let _ = std::fs::remove_file(std_target.join(".rustc_info.json"));
        cmd.env(
            "RUSTC_WRAPPER",
            repo_root().join("scripts/galexy-rustc-wrapper.py"),
        );
        cmd.env("GALEXY_SYSROOT", &sysroot);
        let status = cmd
            .status()
            .unwrap_or_else(|e| panic!("spawn cargo for {pkg}: {e}"));
        if !status.success() {
            panic!("{pkg} build failed");
        }
        let elf = std_target
            .join(galexy_target_dir_name())
            .join(profile)
            .join(pkg);
        let bytes = std::fs::read(&elf).unwrap_or_else(|e| panic!("read {}: {e}", elf.display()));
        assert!(bytes.starts_with(b"\x7fELF"), "{pkg} is not an ELF");
        bins.insert(pkg.into(), bytes);
    }
    // Milestone 71: the sysroot the on-OS compiler will read, under `rust/`.
    pack_sysroot(&std_target, profile, &mut bins);
    let (obj_a, obj_b) = gxld::sample_objects();
    bins.insert("a.o".into(), obj_a);
    bins.insert("b.o".into(), obj_b);
    bins
}

/// Copies std `.rlib` files into the ramdisk map as `rust/<file>`.
///
/// `-Zbuild-std` with the PAL overlay writes them under
/// `build/<crate>/<hash>/out`, not `deps/`. Only the newest file for
/// each crate is packed. `.rmeta` stays out: a debug `libcore.rmeta`
/// is tens of megabytes, and every BIOS/UEFI image embeds the ramdisk.
fn pack_sysroot(
    std_target: &Path,
    profile: &str,
    bins: &mut std::collections::BTreeMap<String, Vec<u8>>,
) {
    let profile_dir = std_target.join(galexy_target_dir_name()).join(profile);
    let mut best: std::collections::BTreeMap<&str, (std::time::SystemTime, std::path::PathBuf)> =
        std::collections::BTreeMap::new();
    let mut consider = |path: &Path| {
        if !path.is_file() {
            return;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("rlib") {
            return;
        }
        let Some(stem) = is_sysroot_artifact(name) else {
            return;
        };
        let mtime = path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match best.get(stem) {
            Some((prev, _)) if *prev >= mtime => {}
            _ => {
                best.insert(stem, (mtime, path.to_path_buf()));
            }
        }
    };
    if let Ok(entries) = std::fs::read_dir(profile_dir.join("deps")) {
        for entry in entries.flatten() {
            consider(&entry.path());
        }
    }
    if let Ok(crates) = std::fs::read_dir(profile_dir.join("build")) {
        for crate_dir in crates.flatten() {
            let Ok(hashes) = std::fs::read_dir(crate_dir.path()) else {
                continue;
            };
            for hash in hashes.flatten() {
                let Ok(files) = std::fs::read_dir(hash.path().join("out")) else {
                    continue;
                };
                for file in files.flatten() {
                    consider(&file.path());
                }
            }
        }
    }
    for (_mtime, path) in best.values() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap();
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        bins.insert(format!("rust/{name}"), bytes);
    }
    if !best.contains_key("libstd-") {
        panic!(
            "sysroot libstd rlib missing under {}",
            profile_dir.display()
        );
    }
}

fn is_sysroot_artifact(name: &str) -> Option<&'static str> {
    const STEMS: &[&str] = &[
        "libstd-",
        "libcore-",
        "liballoc-",
        "libpanic_abort-",
        "libpanic_unwind-",
        "libcompiler_builtins-",
        "libstd_detect-",
        "librustc_std_workspace_core-",
        "librustc_std_workspace_alloc-",
        "librustc_std_workspace_std-",
        "libhashbrown-",
        "libcfg_if-",
        "libunwind-",
    ];
    STEMS.iter().copied().find(|stem| name.starts_with(stem))
}

/// Shell ELF with the `crash` test seam. A separate target dir so this
/// build does not fight the parent cargo lock or feature unification.
fn build_crash_shell(out_dir: &Path) -> Vec<u8> {
    let target_dir = out_dir.join("crash-shell-target");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace/shell/Cargo.toml");
    let release = is_release();
    let mut cmd = nested_cargo();
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--features")
        .arg("crash-seam")
        .arg("--target-dir")
        .arg(&target_dir);
    galexy_cargo_args(&mut cmd, "core,alloc");
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().expect("spawn cargo for crash-seam shell");
    if !status.success() {
        panic!("crash-seam shell build failed");
    }
    let profile = if release { "release" } else { "debug" };
    let elf = target_dir
        .join(galexy_target_dir_name())
        .join(profile)
        .join("shell");
    std::fs::read(&elf).unwrap_or_else(|e| panic!("read {}: {e}", elf.display()))
}

/// Build the host `gxld` binary, then every userspace program with
/// `-Clinker=gxld -Clinker-flavor=ld`: rustc hands `gxld` the exact
/// objects and rlibs it hands `rust-lld`. Returns `bin name → ELF`.
fn build_gxld_userspace(out_dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let gxld_target = out_dir.join("gxld-target");
    let gxld_manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gxld/Cargo.toml");
    let mut cmd = nested_cargo();
    cmd.arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(&gxld_manifest)
        .arg("--target-dir")
        .arg(&gxld_target);
    let status = cmd.status().expect("spawn cargo for gxld");
    if !status.success() {
        panic!("gxld build failed");
    }
    let gxld = gxld_target.join("release").join("gxld");
    assert!(gxld.is_file(), "gxld binary missing at {}", gxld.display());

    let target_dir = out_dir.join("gxld-userspace");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let release = is_release();
    let mut cmd = nested_cargo();
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&workspace)
        .arg("--target-dir")
        .arg(&target_dir);
    galexy_cargo_args(&mut cmd, "core,alloc");
    for pkg in NOSTD_PACKAGES {
        cmd.arg("-p").arg(pkg);
    }
    if release {
        cmd.arg("--release");
    }
    cmd.env(
        "RUSTFLAGS",
        format!("-Clinker={} -Clinker-flavor=ld", gxld.display()),
    );
    let status = cmd.status().expect("spawn cargo for gxld-linked userspace");
    if !status.success() {
        panic!("gxld-linked userspace build failed");
    }
    let profile = if release { "release" } else { "debug" };
    read_elf_dir(&target_dir.join(galexy_target_dir_name()).join(profile))
}
