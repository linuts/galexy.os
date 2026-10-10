use std::path::{Path, PathBuf};
use std::process::Command;

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

        // Milestone 61: host `gxc` compiles the frozen gxr example into a
        // distinct ramdisk name so rustc-built `hello` stays untouched.
        {
            let gxr = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gxc/examples/hello.gxr");
            println!("cargo:rerun-if-changed={}", gxr.display());
            let src = std::fs::read_to_string(&gxr)
                .unwrap_or_else(|e| panic!("ramdisk: read {}: {e}", gxr.display()));
            let elf =
                gxc::compile_elf(&src).unwrap_or_else(|e| panic!("gxc compile hello.gxr: {e}"));
            entries.push(("hello-gxc".into(), elf));
        }

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

    for (name, kernel) in &bins {
        let bios_path = out_dir.join(format!("{}-bios.img", name));
        let uefi_path = out_dir.join(format!("{}-uefi.img", name));

        bootloader::BiosBoot::new(kernel)
            .set_ramdisk(&ramdisk_path)
            .create_disk_image(&bios_path)
            .unwrap();
        bootloader::UefiBoot::new(kernel)
            .set_ramdisk(&ramdisk_path)
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
        for (variant, ramdisk) in [
            ("galexy-os-crashseam", &crash_ramdisk),
            ("galexy-os-gxld", &gxld_ramdisk),
        ] {
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
    cmd
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

/// `hello`, `init`, `shell`, `util` with `-Zbuild-std=core,alloc`, plus
/// `stdmin` with `-Zbuild-std=std,panic_abort` on the unsupported PAL.
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
    for pkg in ["hello", "init", "shell", "util"] {
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

    let std_target = out_dir.join("stdmin-target");
    let manifest = repo_root().join("crates/userspace/stdmin/Cargo.toml");
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
    let sysroot = galexy_std_sysroot(out_dir);
    cmd.env(
        "RUSTC_WRAPPER",
        repo_root().join("scripts/galexy-rustc-wrapper.py"),
    );
    cmd.env("GALEXY_SYSROOT", &sysroot);
    let status = cmd.status().expect("spawn cargo for stdmin");
    if !status.success() {
        panic!("stdmin build failed");
    }

    let profile = if release { "release" } else { "debug" };
    let mut bins = read_elf_dir(&target_dir.join(galexy_target_dir_name()).join(profile));
    let stdmin = std_target
        .join(galexy_target_dir_name())
        .join(profile)
        .join("stdmin");
    let bytes = std::fs::read(&stdmin).unwrap_or_else(|e| panic!("read {}: {e}", stdmin.display()));
    assert!(bytes.starts_with(b"\x7fELF"), "stdmin is not an ELF");
    bins.insert("stdmin".into(), bytes);
    bins
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
    for pkg in ["hello", "init", "shell", "util"] {
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
