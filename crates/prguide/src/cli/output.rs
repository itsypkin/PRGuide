//! JSON output and CLI error reporting conventions.

use std::io::{self, Write};

use clap::error::ErrorKind;
use serde::Serialize;

use crate::Error;

pub fn print_json<T: Serialize>(value: &T) -> Result<(), Error> {
    let text = serde_json::to_string(value).map_err(|error| Error::Internal(error.to_string()))?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}").map_err(|error| Error::Internal(error.to_string()))
}

pub fn fail(err: Error) -> ! {
    let body = serde_json::json!({"code": err.code(), "message": err.to_string()});
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{body}");
    std::process::exit(1)
}

pub fn clap_error(err: clap::Error) -> ! {
    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => err.exit(),
        _ => fail(Error::Usage(err.render().to_string().trim().to_string())),
    }
}
