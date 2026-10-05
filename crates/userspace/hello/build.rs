fn main() {
    println!("cargo:rustc-link-arg=--image-base={:#x}", galexy_abi::USER_IMAGE_BASE);
    // Static, non-PIE: the loader maps at the phdrs' vaddrs verbatim.
    println!("cargo:rustc-link-arg=--no-pie");
}
