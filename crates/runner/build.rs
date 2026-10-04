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
    for (name, kernel) in &bins {
        let bios_path = out_dir.join(format!("{}-bios.img", name));
        let uefi_path = out_dir.join(format!("{}-uefi.img", name));

        bootloader::BiosBoot::new(kernel)
            .create_disk_image(&bios_path)
            .unwrap();
        bootloader::UefiBoot::new(kernel)
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
