//! prg link command, filled by T8-2.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct LinkPrgArgs {
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

pub fn run(_args: LinkPrgArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide link-prg is not implemented yet".into(),
    ))
}
