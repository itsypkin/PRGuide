//! `prguide open`: runs the reviewer's open command, or prints the viewer command.

use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use serde::Serialize;

use crate::{
    Error,
    cli::output::print_json,
    config::{self, OPEN_CMD_ENV},
    launcher,
    store::{Clock, Store, SystemClock, prguide_home, runs::RunId},
};

#[derive(Debug, clap::Args)]
pub struct OpenArgs {
    #[arg(long)]
    pub run: String,
}

/// Everything `open` reads from its surroundings.
pub struct OpenEnv<'a> {
    pub home: PathBuf,
    pub open_cmd_env: Option<OsString>,
    pub exe: PathBuf,
    pub clock: Arc<dyn Clock>,
    pub warnings: &'a mut dyn Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenOutput {
    pub opened: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_command: Option<String>,
}

/// Runs `prguide open` against the real environment.
pub fn run(args: OpenArgs) -> Result<(), Error> {
    let exe = std::env::current_exe()
        .map_err(|error| Error::Internal(format!("cannot find the prguide binary: {error}")))?;
    let mut stderr = io::stderr().lock();
    let mut env = OpenEnv {
        home: prguide_home()?,
        open_cmd_env: std::env::var_os(OPEN_CMD_ENV),
        exe,
        clock: Arc::new(SystemClock),
        warnings: &mut stderr,
    };
    print_json(&open(&mut env, &args)?)
}

/// Runs the configured open command, or returns the viewer command to print.
pub fn open(env: &mut OpenEnv<'_>, args: &OpenArgs) -> Result<OpenOutput, Error> {
    let run_id = RunId::new(&args.run);
    let mut store = Store::open_for_run(&env.home, &run_id)?.with_clock(Arc::clone(&env.clock));
    crate::store::runs::require_active(&mut store, &run_id)?;
    let number = store.pr().number();
    drop(store);

    let (command, warnings) = config::open_command(&env.home, env.open_cmd_env.as_deref())?;
    for warning in warnings {
        let message = warning.to_string();
        let line = serde_json::json!({
            "warning": "config_unknown_key",
            "message": message,
        });
        let _ = writeln!(env.warnings, "{line}");
    }
    let view = launcher::view_command(&env.exe, &run_id);
    match command {
        None => Ok(OpenOutput {
            opened: false,
            command: Some(format!("prg {number}")),
            view_command: Some(view.to_string_lossy().into_owned()),
        }),
        Some(command) => {
            launcher::launch(&command, &view)?;
            Ok(OpenOutput {
                opened: true,
                command: None,
                view_command: None,
            })
        }
    }
}
