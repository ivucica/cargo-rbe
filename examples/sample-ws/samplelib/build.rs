fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/gen.rs"), "pub const GENERATED: u32 = 41;\n").unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
