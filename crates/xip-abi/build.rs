fn main() {
    println!("cargo:rerun-if-env-changed=XIP_MODULE_STATIC_ADDR");
}
