//! Skill installation command, filled by T4-6.

use std::path::PathBuf;

use crate::Error;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Harness {
    Claude,
    Agents,
}

#[derive(Debug, clap::Args)]
pub struct SkillInstallArgs {
    #[arg(long)]
    pub harness: Option<Harness>,
    #[arg(long)]
    pub dir: Option<PathBuf>,
    #[arg(long)]
    pub force: bool,
}

pub fn run(_args: SkillInstallArgs) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "prguide skill install is not implemented yet".into(),
    ))
}
