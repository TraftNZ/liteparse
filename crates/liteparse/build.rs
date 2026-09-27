// The static ONNX Runtime library used by oar-ocr needs C23 strtol symbols.
// glibc before 2.38 does not export them, so link a compatibility object there.

use std::env;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const GLIBC_ISOC23_MIN: (u32, u32) = (2, 38);

fn host_glibc_version(cc: &str) -> (u32, u32) {
    let mut child = Command::new(cc)
        .args(["-dM", "-E", "-x", "c", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to invoke C compiler `{cc}` for glibc probe: {e}"));
    child
        .stdin
        .take()
        .expect("probe stdin")
        .write_all(b"#include <features.h>\n")
        .expect("write glibc probe");
    let output = child.wait_with_output().expect("glibc probe output");
    if !output.status.success() {
        panic!("glibc probe with `{cc}` failed: {}", output.status);
    }
    let macros = String::from_utf8_lossy(&output.stdout);
    let macro_value = |name: &str| -> u32 {
        macros
            .lines()
            .find_map(|line| line.strip_prefix(&format!("#define {name} ")))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("glibc probe did not define {name}"))
    };
    (macro_value("__GLIBC__"), macro_value("__GLIBC_MINOR__"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=glibc_isoc23_shim.c");

    if env::var_os("CARGO_FEATURE_OAR_OCR").is_none()
        || env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "linux"
        || env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() != "gnu"
    {
        return;
    }

    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    if host_glibc_version(&cc) >= GLIBC_ISOC23_MIN {
        return;
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let obj_path = Path::new(&out_dir).join("glibc_isoc23_shim.o");
    let status = Command::new(&cc)
        .args([
            "-c",
            "-O2",
            "-fPIC",
            "glibc_isoc23_shim.c",
            "-o",
            obj_path.to_str().expect("non-utf8 OUT_DIR"),
        ])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke C compiler `{cc}` for glibc shim: {e}"));
    if !status.success() {
        panic!("compiling glibc_isoc23_shim.c with `{cc}` failed: {status}");
    }

    // A direct object argument ensures its weak symbols are retained.
    println!(
        "cargo:rustc-link-arg-bins={}",
        obj_path.to_str().expect("non-utf8 OUT_DIR")
    );
}
