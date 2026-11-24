fn main() {
    println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:xtp-probe-delayed.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
