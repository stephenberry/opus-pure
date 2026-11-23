#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../tests/common/fuzz.rs"]
mod body;

fuzz_target!(|data: &[u8]| body::caf_read(data));
