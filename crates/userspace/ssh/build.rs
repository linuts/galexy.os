fn main() {
    println!(
        "cargo:rustc-link-arg=--image-base={:#x}",
        galexy_abi::USER_IMAGE_BASE
    );
    println!("cargo:rustc-link-arg=--no-pie");
}
