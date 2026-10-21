//! Event acknowledgement command, filled by T1-3.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct AckArgs {
    pub event_id: String,
    #[arg(long)]
    pub run: String,
}

pub fn run(_args: AckArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide ack is not implemented yet".into(),
    ))
}
