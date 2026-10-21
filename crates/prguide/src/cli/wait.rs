//! Event waiting command, filled by T1-3.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct WaitArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub timeout: Option<String>,
    #[arg(long)]
    pub max: Option<u32>,
}

pub fn run(_args: WaitArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide wait is not implemented yet".into(),
    ))
}
