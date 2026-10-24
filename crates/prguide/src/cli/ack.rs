//! Event acknowledgement command.

use serde::Serialize;

use crate::{
    Error,
    cli::output,
    store::{Store, events, prguide_home, runs::RunId},
};

#[derive(Debug, clap::Args)]
pub struct AckArgs {
    pub event_id: String,
    #[arg(long)]
    pub run: String,
}

#[derive(Serialize)]
struct AckOutput<'a> {
    event: &'a str,
    result_kind: events::ResultKind,
    result_id: String,
}

pub fn run(args: AckArgs) -> Result<(), Error> {
    let run_id = RunId::new(args.run);
    let mut store = Store::open_for_run(&prguide_home()?, &run_id)?;
    let result = events::ack(&mut store, &run_id, &args.event_id)?;
    output::print_json(&AckOutput {
        event: &args.event_id,
        result_kind: result.kind,
        result_id: result.id,
    })
}
