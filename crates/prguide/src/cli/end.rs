//! Review-ending command, filled by T4-4.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct EndArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub clean: bool,
}

pub fn run(_args: EndArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide end is not implemented yet".into(),
    ))
}
