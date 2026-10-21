//! Event display command, filled by T4-1.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    pub event_id: String,
    #[arg(long)]
    pub run: String,
}

pub fn run(_args: ShowArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide show is not implemented yet".into(),
    ))
}
