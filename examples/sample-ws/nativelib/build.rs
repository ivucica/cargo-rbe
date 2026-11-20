// Compiles a C file into a static library, like many -sys crates do (and like jemalloc does,
// at a much larger scale). Exercises: a registry build-dependency, running a C compiler, files
// written to OUT_DIR, and cargo:rustc-link-* directives that contain OUT_DIR paths.
fn main() {
    cc::Build::new().file("src/native.c").compile("rbe_native");
    println!("cargo:rerun-if-changed=src/native.c");
}
