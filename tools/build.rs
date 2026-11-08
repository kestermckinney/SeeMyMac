fn main() {
    // This is a build script that can be used to detect
    // if we're building with X11 support
    
    println!("cargo:rustc-check-cfg=cfg(with_x11)");
}