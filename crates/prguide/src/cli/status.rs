//! `prguide status --run R`: the agent's heartbeat and the viewer's view of the agent.

use serde::Serialize;

use crate::{
    Error,
    cli::output,
    store::{
        Store, prguide_home,
        runs::{self, Run, RunId},
        status::{self, ReadyVersion},
    },
};

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub run: String,
}

#[derive(Debug, Serialize)]
pub struct StatusOutput {
    pub run: Run,
    pub guide_versions: Vec<ReadyVersion>,
    pub status: &'static str,
    pub pending: u64,
}

pub fn run(args: StatusArgs) -> Result<(), Error> {
    let run_id = RunId::new(args.run);
    let mut store = Store::open_for_run(&prguide_home()?, &run_id)?;
    let run = runs::require_active(&mut store, &run_id)?;
    let report = status::status_report(&mut store)?;
    output::print_json(&StatusOutput {
        run,
        guide_versions: report.guide_versions,
        status: report.agent.name(),
        pending: report.pending,
    })
}
