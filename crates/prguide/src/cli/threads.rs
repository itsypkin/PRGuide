//! Thread listing command, filled by T4-1.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct ThreadsArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub github: bool,
}

pub fn run(_args: ThreadsArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide threads is not implemented yet".into(),
    ))
}
