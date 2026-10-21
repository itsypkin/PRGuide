//! Viewer commands, filled by T6-1.

use std::ffi::OsString;

use clap::Parser;

use crate::{Error, cli::output, viewer};

#[derive(Debug, clap::Args)]
pub struct ViewArgs {
    #[arg(long)]
    pub run: Option<String>,
}

pub fn run(args: ViewArgs) -> Result<(), Error> {
    viewer::run(viewer::ViewerArgs {
        pr: None,
        run: args.run,
    })
}

#[derive(Debug, Parser)]
#[command(name = "prg")]
pub struct PrgArgs {
    pub n: Option<u64>,
}

pub fn run_prg(args: Vec<OsString>) {
    let args = match PrgArgs::try_parse_from(args) {
        Ok(args) => args,
        Err(error) => output::clap_error(error),
    };
    if let Err(error) = viewer::run(viewer::ViewerArgs {
        pr: args.n,
        run: None,
    }) {
        output::fail(error);
    }
}
