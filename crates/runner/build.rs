use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
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
        // Program bins from the userspace tree: one package dir per
        // program (dir name == package name == bin name), built via the
        // artifact dep env vars (same mechanism the kernel bins use).
        let userspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace");
        if let Ok(pkg_dirs) = std::fs::read_dir(&userspace_root) {
            for pkg in pkg_dirs.flatten() {
                if !pkg.path().join("src").is_dir() {
                    continue; // the galexy-rt LIB is not a program
                }
                let pkg_name = pkg
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let env_crate = pkg_name.replace('-', "_").to_uppercase();
                let mut bin_files: Vec<(String, String)> = Vec::new(); // (stem, env var)
                if let Ok(bin_dir) = std::fs::read_dir(pkg.path().join("src/bin")) {
                    for entry in bin_dir.flatten() {
                        let stem = entry
                            .path()
                            .file_stem()
                            .unwrap()
                            .to_string_lossy()
                            .to_string();
                        let var = format!("CARGO_BIN_FILE_{}_{}", env_crate, stem);
                        bin_files.push((stem, var));
                    }
                }
                let main_rs = pkg.path().join("src/main.rs");
                if main_rs.is_file() {
                    bin_files.push((
                        pkg_name.clone(),
                        format!("CARGO_BIN_FILE_{}_{}", env_crate, pkg_name),
                    ));
                }
                for (stem, var) in bin_files {
                    if let Some(path) = std::env::var_os(&var) {
                        let bytes = std::fs::read(&path)
                            .unwrap_or_else(|e| panic!("ramdisk: read {path:?}: {e}"));
                        entries.push((stem, bytes));
                    }
                }
            }
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
        let bytes = std::fs::read(&ramdisk_path).unwrap();
        // Rebuild from the production entry list is simpler: read names we
        // already packed by reconstructing. The crash image only swaps `shell`.
        let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut body).unwrap();
            if name == "shell" {
                entries.push((name, crash_shell.clone()));
            } else {
                entries.push((name, body));
            }
        }
        let mut tar = tar::Builder::new(std::fs::File::create(&crash_ramdisk).unwrap());
        for (name, body) in &entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, body as &[u8]).unwrap();
        }
        tar.finish().unwrap();
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
        let bios_path = out_dir.join("galexy-os-crashseam-bios.img");
        let uefi_path = out_dir.join("galexy-os-crashseam-uefi.img");
        bootloader::BiosBoot::new(kernel)
            .set_ramdisk(&crash_ramdisk)
            .create_disk_image(&bios_path)
            .unwrap();
        bootloader::UefiBoot::new(kernel)
            .set_ramdisk(&crash_ramdisk)
            .create_disk_image(&uefi_path)
            .unwrap();
        if !manifest.is_empty() {
            manifest.push(';');
        }
        manifest.push_str(&format!(
            "galexy-os-crashseam:{},{}",
            bios_path.display(),
            uefi_path.display()
        ));
    }

    println!("cargo:rustc-env=GALEXY_IMAGES={}", manifest);
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

/// Shell ELF with the `crash` test seam. A separate target dir so this
/// build does not fight the parent cargo lock or feature unification.
fn build_crash_shell(out_dir: &Path) -> Vec<u8> {
    let target_dir = out_dir.join("crash-shell-target");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../userspace/shell/Cargo.toml");
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let mut cmd = Command::new(env!("CARGO"));
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target")
        .arg("x86_64-unknown-none")
        .arg("--features")
        .arg("crash-seam")
        .arg("--target-dir")
        .arg(&target_dir);
    if release {
        cmd.arg("--release");
    }
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
    let status = cmd.status().expect("spawn cargo for crash-seam shell");
    if !status.success() {
        panic!("crash-seam shell build failed");
    }
    let profile = if release { "release" } else { "debug" };
    let elf = target_dir
        .join("x86_64-unknown-none")
        .join(profile)
        .join("shell");
    std::fs::read(&elf).unwrap_or_else(|e| panic!("read {}: {e}", elf.display()))
}
