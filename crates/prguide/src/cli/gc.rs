//! Garbage collection command, filled by T8-1.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct GcArgs {
    #[arg(long)]
    pub closed_older_than: Option<String>,
    #[arg(long)]
    pub pr: Option<u64>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
}

pub fn run(_args: GcArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide gc is not implemented yet".into(),
    ))
}
