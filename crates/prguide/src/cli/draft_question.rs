//! Draft-question command, filled by T4-3.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct DraftQuestionArgs {
    pub thread: String,
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub caused_by: String,
    #[arg(long)]
    pub file: PathBuf,
}

pub fn run(_args: DraftQuestionArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide draft-question is not implemented yet".into(),
    ))
}
