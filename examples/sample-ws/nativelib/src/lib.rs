extern "C" {
    fn rbe_native_answer() -> i32;
}

pub fn answer() -> i32 {
    unsafe { rbe_native_answer() }
}
