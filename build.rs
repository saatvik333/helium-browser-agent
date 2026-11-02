//! Stamps each build so a client never talks to a daemon started from a different binary.
//! Without `rerun-if-changed`, Cargo reruns this whenever any package file changes.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    println!("cargo:rustc-env=HBA_BUILD_ID={stamp:x}");
}
