fn main() {
    println!("cargo:rerun-if-changed=../sdkconfig.defaults");
    println!("cargo:rerun-if-changed=../components/xip_ble/profiles");
    embuild::espidf::sysenv::output();
}
