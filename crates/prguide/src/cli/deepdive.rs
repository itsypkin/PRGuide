//! Deep-dive command, filled by T4-2.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct DeepdiveArgs {
    pub slide: String,
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub caused_by: String,
    #[arg(long)]
    pub file: PathBuf,
}

pub fn run(_args: DeepdiveArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide deepdive is not implemented yet".into(),
    ))
}
