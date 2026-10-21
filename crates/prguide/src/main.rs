//! Binary entry point for PRGuide.

use std::{ffi::OsString, path::Path};

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let is_prg = args
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .is_some_and(|name| name == "prg");

    if is_prg {
        prguide::cli::view::run_prg(args);
    } else {
        prguide::cli::run_prguide(args);
    }
}
