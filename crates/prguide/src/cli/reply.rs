//! Review reply command, filled by T4-1.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, clap::Args)]
#[command(group(clap::ArgGroup::new("content").required(true).args(["text", "file"])))]
pub struct ReplyArgs {
    pub thread: String,
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub caused_by: String,
    #[arg(long)]
    pub text: Option<String>,
    #[arg(long)]
    pub file: Option<PathBuf>,
}

pub fn run(_args: ReplyArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide reply is not implemented yet".into(),
    ))
}
