//! Start command, filled by T2-4.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    pub pr: u64,
    #[arg(long)]
    pub takeover: bool,
}

pub fn run(_args: StartArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide start is not implemented yet".into(),
    ))
}
