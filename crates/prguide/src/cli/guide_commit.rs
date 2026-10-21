//! Guide commit command, filled by T3-4.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct GuideCommitArgs {
    pub draft: PathBuf,
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub caused_by: Option<String>,
}

pub fn run(_args: GuideCommitArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide guide commit is not implemented yet".into(),
    ))
}
