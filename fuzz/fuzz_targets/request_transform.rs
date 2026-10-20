#![no_main]
#[path = "../../tests/support/invariants.rs"]
mod invariants;
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    invariants::transformation(data);
});
