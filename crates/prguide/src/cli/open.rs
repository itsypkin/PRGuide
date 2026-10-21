//! Browser opening command, filled by T4-5.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct OpenArgs {
    #[arg(long)]
    pub run: String,
}

pub fn run(_args: OpenArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide open is not implemented yet".into(),
    ))
}
