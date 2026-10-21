fn main() {
    println!("cargo:rerun-if-env-changed=XIP_MODULE_STATIC_ADDR");
    println!("cargo:rustc-check-cfg=cfg(xip_psa_esp32)");
    println!("cargo:rustc-check-cfg=cfg(xip_psa_esp32s3)");
    // Native PSA context layout follows the exact SDK target, not just Xtensa:
    // ESP32 and ESP32-S3 have different enabled software hash implementations.
    if std::env::var("TARGET").as_deref() == Ok("xtensa-esp32-espidf") {
        println!("cargo:rustc-cfg=xip_psa_esp32");
    }
    if std::env::var("TARGET").as_deref() == Ok("xtensa-esp32s3-espidf") {
        println!("cargo:rustc-cfg=xip_psa_esp32s3");
    }
}
