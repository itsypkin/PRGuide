//! Command-line parser and command dispatch.

use std::ffi::OsString;

use clap::{Parser, Subcommand};

pub mod ack;
pub mod check;
pub mod deepdive;
pub mod draft_question;
pub mod end;
pub mod gc;
pub mod guide_commit;
pub mod guide_new;
pub mod link_prg;
pub mod open;
pub mod output;
pub mod reply;
pub mod show;
pub mod skill;
pub mod start;
pub mod status;
pub mod threads;
pub mod view;
pub mod wait;

#[derive(Debug, Parser)]
#[command(name = "prguide", version)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start(start::StartArgs),
    #[command(subcommand)]
    Guide(GuideCommand),
    Check(check::CheckArgs),
    Open(open::OpenArgs),
    Wait(wait::WaitArgs),
    Show(show::ShowArgs),
    Reply(reply::ReplyArgs),
    Deepdive(deepdive::DeepdiveArgs),
    DraftQuestion(draft_question::DraftQuestionArgs),
    Ack(ack::AckArgs),
    Threads(threads::ThreadsArgs),
    Status(status::StatusArgs),
    End(end::EndArgs),
    #[command(subcommand)]
    Skill(SkillCommand),
    Gc(gc::GcArgs),
    LinkPrg(link_prg::LinkPrgArgs),
    View(view::ViewArgs),
}

#[derive(Debug, Subcommand)]
enum GuideCommand {
    New(guide_new::GuideNewArgs),
    Commit(guide_commit::GuideCommitArgs),
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    Install(skill::SkillInstallArgs),
}

pub fn run_prguide(args: Vec<OsString>) {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => output::clap_error(error),
    };
    let result = match cli.command {
        Command::Start(args) => start::run(args),
        Command::Guide(GuideCommand::New(args)) => guide_new::run(args),
        Command::Guide(GuideCommand::Commit(args)) => guide_commit::run(args),
        Command::Check(args) => check::run(args),
        Command::Open(args) => open::run(args),
        Command::Wait(args) => wait::run(args),
        Command::Show(args) => show::run(args),
        Command::Reply(args) => reply::run(args),
        Command::Deepdive(args) => deepdive::run(args),
        Command::DraftQuestion(args) => draft_question::run(args),
        Command::Ack(args) => ack::run(args),
        Command::Threads(args) => threads::run(args),
        Command::Status(args) => status::run(args),
        Command::End(args) => end::run(args),
        Command::Skill(SkillCommand::Install(args)) => skill::run(args),
        Command::Gc(args) => gc::run(args),
        Command::LinkPrg(args) => link_prg::run(args),
        Command::View(args) => view::run(args),
    };
    if let Err(error) = result {
        output::fail(error);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, view::PrgArgs};
    use clap::CommandFactory;

    #[test]
    fn command_definitions_are_valid() {
        Cli::command().debug_assert();
        PrgArgs::command().debug_assert();
    }
}
