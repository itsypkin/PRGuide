//! Guide checking command, filled by T3-3.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    pub draft: PathBuf,
    #[arg(long)]
    pub run: String,
}

pub fn run(_args: CheckArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide check is not implemented yet".into(),
    ))
}
