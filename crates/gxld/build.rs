fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("galexy") {
        println!(
            "cargo:rustc-link-arg=--image-base={:#x}",
            galexy_abi::USER_IMAGE_BASE
        );
        println!("cargo:rustc-link-arg=--no-pie");
    }
}
