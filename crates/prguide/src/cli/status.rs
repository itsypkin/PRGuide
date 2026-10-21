//! Status command, filled by T1-4.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub run: String,
}

pub fn run(_args: StatusArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide status is not implemented yet".into(),
    ))
}
