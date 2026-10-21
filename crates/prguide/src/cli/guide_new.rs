//! Guide creation command, filled by T3-2.

use crate::Error;

#[derive(Debug, clap::Args)]
pub struct GuideNewArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub commits: Option<String>,
}

pub fn run(_args: GuideNewArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide guide new is not implemented yet".into(),
    ))
}
