use std::path::{Path, PathBuf};

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

    println!("cargo:rustc-env=GALEXY_IMAGES={}", manifest);
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../galexy-os/src")
            .display()
    );
}
