mod support;

use assert_cmd::Command;
use serde_json::Value;

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_prguide"))
}

fn error_result(command: &mut Command) -> Value {
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1);
    let value: Value = serde_json::from_str(&stderr).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), 2);
    assert!(object.contains_key("code") && object.contains_key("message"));
    assert!(
        object["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    value
}

fn usage(command: &mut Command) -> Value {
    let value = error_result(command);
    assert_eq!(value["code"], "usage");
    value
}
fn not_implemented(command: &mut Command, message: &str) {
    let value = error_result(command);
    assert_eq!(value["code"], "not_implemented");
    assert!(value["message"].as_str().unwrap().contains(message));
}

#[test]
fn every_command_parses_required_and_optional_arguments() {
    let cases: &[(&[&str], &str)] = &[
        (&["guide", "new", "--run", "r_x"], "guide new"),
        (
            &["guide", "new", "--run", "r_x", "--commits", "a..b"],
            "guide new",
        ),
        (&["check", "d.md", "--run", "r_x"], "check"),
        (&["guide", "commit", "d.md", "--run", "r_x"], "guide commit"),
        (
            &[
                "guide",
                "commit",
                "d.md",
                "--run",
                "r_x",
                "--caused-by",
                "e_1",
            ],
            "guide commit",
        ),
        (&["show", "e_1", "--run", "r_x"], "show"),
        (
            &[
                "reply",
                "t_1",
                "--run",
                "r_x",
                "--caused-by",
                "e_1",
                "--text",
                "hi",
            ],
            "reply",
        ),
        (
            &[
                "reply",
                "t_1",
                "--run",
                "r_x",
                "--caused-by",
                "e_1",
                "--file",
                "m.md",
            ],
            "reply",
        ),
        (
            &[
                "deepdive",
                "s_1",
                "--run",
                "r_x",
                "--caused-by",
                "e_1",
                "--file",
                "p.md",
            ],
            "deepdive",
        ),
        (
            &[
                "draft-question",
                "t_1",
                "--run",
                "r_x",
                "--caused-by",
                "e_1",
                "--file",
                "q.md",
            ],
            "draft-question",
        ),
        (&["threads", "--run", "r_x"], "threads"),
        (&["threads", "--run", "r_x", "--github"], "threads"),
        (&["end", "--run", "r_x"], "end"),
        (&["end", "--run", "r_x", "--clean"], "end"),
        (&["skill", "install"], "skill install"),
        (
            &[
                "skill",
                "install",
                "--harness",
                "claude",
                "--dir",
                "/tmp/x",
                "--force",
            ],
            "skill install",
        ),
        (
            &[
                "skill",
                "install",
                "--harness",
                "agents",
                "--dir",
                "/tmp/x",
                "--force",
            ],
            "skill install",
        ),
        (&["gc"], "gc"),
        (
            &[
                "gc",
                "--closed-older-than",
                "30d",
                "--pr",
                "7",
                "--dry-run",
                "--force",
            ],
            "gc",
        ),
        (&["link-prg"], "link-prg"),
        (&["link-prg", "--dir", "/tmp/x"], "link-prg"),
    ];
    for (args, message) in cases {
        let mut command = command();
        command.args(*args);
        not_implemented(&mut command, message);
    }
    {
        let mut command = command();
        command.args(["view"]);
        not_implemented(&mut command, "viewer");
    }
    {
        let mut command = command();
        command.args(["view", "--run", "r_x"]);
        not_implemented(&mut command, "viewer");
    }
}

#[test]
fn invalid_invocations_report_usage_json() {
    let cases: &[&[&str]] = &[
        &["frobnicate"],
        &[],
        &["show", "--run", "r_x"],
        &[
            "reply",
            "t_1",
            "--run",
            "r_x",
            "--caused-by",
            "e_1",
            "--text",
            "hi",
            "--file",
            "m.md",
        ],
        &["reply", "t_1", "--run", "r_x", "--caused-by", "e_1"],
        &["skill", "install", "--harness", "bogus"],
        &["start", "abc"],
        &["guide"],
        &["skill"],
        &["gc", "--run", "r_x"],
    ];
    for args in cases {
        let mut command = command();
        command.args(*args);
        let value = usage(&mut command);
        if matches!(*args, [] | ["guide"] | ["skill"]) {
            assert!(
                value["message"]
                    .as_str()
                    .unwrap()
                    .contains("requires a subcommand")
            );
        }
    }
    let mut forced = command();
    forced.env("CLICOLOR_FORCE", "1").arg("frobnicate");
    assert!(
        !error_result(&mut forced)["message"]
            .as_str()
            .unwrap()
            .contains('\u{1b}')
    );
}

#[test]
fn help_and_version_succeed() {
    let mut help = command();
    let help = help.arg("--help").output().unwrap();
    assert!(help.status.success() && !help.stdout.is_empty());
    let mut version = command();
    let version = version.arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8(version.stdout).unwrap().contains("0.1.0"));
}

#[cfg(unix)]
mod unix {
    use super::{error_result, not_implemented, usage};
    use assert_cmd::Command;
    use std::os::unix::fs::symlink;
    use tempfile::{TempDir, tempdir};
    fn linked(name: &str) -> (TempDir, Command) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        symlink(env!("CARGO_BIN_EXE_prguide"), &path).unwrap();
        (dir, Command::new(&path))
    }

    #[test]
    fn prg_name_runs_viewer_and_only_exact_name_does() {
        let (_dir, mut command) = linked("prg");
        not_implemented(&mut command, "viewer");
        let (_dir, mut command) = linked("prg");
        command.arg("12");
        not_implemented(&mut command, "viewer");
        let (_dir, mut command) = linked("prg");
        command.arg("abc");
        usage(&mut command);
        let (_dir, mut command) = linked("prg");
        command.args(["1", "2"]);
        usage(&mut command);
        let (_dir, mut command) = linked("prgx");
        usage(&mut command);
        let (_dir, mut command) = linked("prgx");
        let home = tempdir().unwrap();
        command
            .args(["status", "--run", "r_x"])
            .env("PRGUIDE_HOME", home.path());
        let value = error_result(&mut command);
        assert_eq!(value["code"], "run_unknown");
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[test]
    fn prg_help_succeeds() {
        let (_dir, mut command) = linked("prg");
        let output = command.arg("--help").output().unwrap();
        assert!(output.status.success() && !output.stdout.is_empty());
    }
}
