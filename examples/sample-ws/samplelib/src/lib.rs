include!(concat!(env!("OUT_DIR"), "/gen.rs"));

cfg_if::cfg_if! {
    if #[cfg(unix)] {
        pub fn describe() -> &'static str { "unix" }
    } else {
        pub fn describe() -> &'static str { "other" }
    }
}

pub fn value() -> u32 {
    GENERATED + 1
}

// Deliberately triggers a compiler warning: cache hits must replay it.
#[allow(dead_code)]
pub fn warning_demo() {
    let unused_variable = 1;
}
