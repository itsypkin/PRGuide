use std::{
    ffi::OsString,
    fs,
    io::Write,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use prguide::{
    cli::open::{OpenArgs, OpenEnv, OpenOutput, open},
    store::{
        Clock, FakeClock, PrRef, Store, StoreError, Timestamp,
        runs::{self, LEASE, RunId, StopReason},
    },
};
use tempfile::TempDir;

const T0: i64 = 1_792_713_600_000;

fn setup() -> (TempDir, Arc<FakeClock>, RunId) {
    let temp = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    let mut store = Store::open_in(temp.path(), &PrRef::new("octo", "widget", 7).expect("pr"))
        .expect("store")
        .with_clock(clock.clone());
    let run = runs::start_run(&mut store, false).expect("run");
    (temp, clock, run.id)
}

fn setup_binary() -> (TempDir, RunId) {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store =
        Store::open_in(temp.path(), &PrRef::new("octo", "widget", 7).expect("pr")).expect("store");
    let run = runs::start_run(&mut store, false).expect("run");
    (temp, run.id)
}

fn call(
    home: PathBuf,
    clock: Arc<FakeClock>,
    run: RunId,
    command: Option<OsString>,
    exe: PathBuf,
) -> (Result<OpenOutput, (String, String)>, Vec<u8>) {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut warnings = Vec::new();
        let mut env = OpenEnv {
            home,
            open_cmd_env: command,
            exe,
            clock,
            warnings: &mut warnings,
        };
        let result = open(
            &mut env,
            &OpenArgs {
                run: run.to_string(),
            },
        )
        .map_err(|error| (error.code().to_owned(), error.to_string()));
        sender.send((result, warnings)).expect("send");
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("open completed")
}

fn call_default(
    home: &Path,
    clock: Arc<FakeClock>,
    run: RunId,
    command: Option<OsString>,
) -> (Result<OpenOutput, (String, String)>, Vec<u8>) {
    call(
        home.to_path_buf(),
        clock,
        run,
        command,
        PathBuf::from("/opt/prguide bin/prguide"),
    )
}

fn write_config(home: &Path, contents: &str) {
    fs::write(home.join("config.toml"), contents).expect("config");
}
fn error(result: Result<OpenOutput, (String, String)>) -> (String, String) {
    result.expect_err("expected error")
}
fn command_value(command: String) -> String {
    format!("open_command = {}", toml::Value::String(command))
}
fn assert_run_id(run: &RunId) {
    assert_eq!(run.as_str().len(), 14);
    assert!(run.as_str().starts_with("r_"));
    assert!(
        run.as_str()[2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
    );
}
fn run_lease(home: &Path, run: &RunId, clock: Arc<FakeClock>) -> (String, String) {
    let mut store = Store::open_for_run(home, run)
        .expect("store")
        .with_clock(clock);
    store
        .tx(|tx| {
            tx.query_row(
                "SELECT state, lease_until FROM runs WHERE id = ?1",
                [run.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StoreError::from)
        })
        .expect("row")
}
fn binary(home: &Path, run: &RunId, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prguide"));
    command
        .args(["open", "--run", run.as_str()])
        .env("PRGUIDE_HOME", home)
        .env_remove("PRGUIDE_OPEN_CMD")
        .env("HOME", cwd)
        .current_dir(cwd);
    command
}
fn output_json(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("json stdout")
}
fn error_json(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stderr).expect("json stderr")
}
fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}
fn pgid(pid: &str) -> String {
    String::from_utf8(
        Command::new("ps")
            .args(["-o", "pgid=", "-p", pid])
            .output()
            .expect("ps")
            .stdout,
    )
    .expect("pgid")
    .trim()
    .to_owned()
}
fn state(pid: &str) -> String {
    String::from_utf8(
        Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .expect("ps")
            .stdout,
    )
    .expect("state")
    .trim()
    .to_owned()
}
fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .expect("kill")
        .success()
        && !state(pid).starts_with('Z')
}
struct KillOnDrop(Option<String>);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
        }
    }
}
fn wait_for(path: &Path, limit: Duration) {
    let start = Instant::now();
    while !path.exists() && start.elapsed() < limit {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "{} was not created", path.display());
}

#[test]
fn no_open_command_prints_the_viewer_command() {
    let (temp, clock, run) = setup();
    assert_run_id(&run);
    clock.advance(Duration::from_secs(600));
    let (output, warnings) = call_default(temp.path(), clock.clone(), run.clone(), None);
    assert!(warnings.is_empty());
    let snapshot = serde_json::to_string_pretty(&output.expect("ok"))
        .expect("json")
        .replace(run.as_str(), "[RUN]");
    insta::assert_snapshot!("opened_false", snapshot);
    assert_eq!(
        run_lease(temp.path(), &run, clock.clone()).1,
        clock.now().saturating_add(LEASE).to_rfc3339()
    );
}

#[test]
fn successful_command_prints_opened_true() {
    let (temp, clock, run) = setup();
    write_config(temp.path(), "open_command = 'exit 0'");
    let start = Instant::now();
    let (output, _) = call_default(temp.path(), clock, run, None);
    assert!(start.elapsed() < Duration::from_secs(2));
    insta::assert_snapshot!(
        "opened_true",
        serde_json::to_string_pretty(&output.expect("ok")).expect("json")
    );
}

#[test]
fn configured_command_receives_view_command() {
    let (temp, clock, run) = setup();
    let root = fs::canonicalize(temp.path()).expect("canonical");
    let out = root.join("out");
    fs::create_dir(&out).expect("out");
    let exe = root.join("b i'n\"$x`y;z\nw").join("prguide");
    let script = format!(
        "printf '%s' \"$PRGUIDE_VIEW_CMD\" > \"{0}/view\"; eval \"set -- $PRGUIDE_VIEW_CMD\"; i=0; for a in \"$@\"; do i=$((i+1)); printf '%s' \"$a\" > \"{0}/arg$i\"; done",
        out.display()
    );
    write_config(temp.path(), &command_value(script));
    assert!(
        call(
            temp.path().to_path_buf(),
            clock,
            run.clone(),
            None,
            exe.clone()
        )
        .0
        .expect("ok")
        .opened
    );
    let expected = format!("{} view --run '{}'", shell_quote(&exe), run.as_str());
    assert_eq!(
        fs::read(out.join("view")).expect("view"),
        expected.as_bytes()
    );
    assert_eq!(
        fs::read(out.join("arg1")).expect("arg1"),
        exe.as_os_str().as_encoded_bytes()
    );
    assert_eq!(fs::read(out.join("arg2")).expect("arg2"), b"view");
    assert_eq!(fs::read(out.join("arg3")).expect("arg3"), b"--run");
    assert_eq!(
        fs::read(out.join("arg4")).expect("arg4"),
        run.as_str().as_bytes()
    );
    assert!(!out.join("arg5").exists());
}

#[test]
fn env_var_overrides_the_config_file() {
    let (temp, clock, run) = setup();
    let out = temp.path().join("out");
    fs::create_dir(&out).expect("out");
    write_config(
        temp.path(),
        &command_value(format!("touch {}/from-config", out.display())),
    );
    let env = OsString::from(format!("touch {}/from-env", out.display()));
    assert!(
        call_default(temp.path(), clock.clone(), run.clone(), Some(env))
            .0
            .expect("ok")
            .opened
    );
    assert!(out.join("from-env").exists());
    assert!(!out.join("from-config").exists());
    write_config(temp.path(), "open_command = ");
    let invalid_override = call_default(
        temp.path(),
        clock.clone(),
        run.clone(),
        Some(OsString::from(format!("touch {}/b", out.display()))),
    );
    assert!(invalid_override.0.expect("override invalid").opened);
    assert!(invalid_override.1.is_empty());
    assert!(out.join("b").exists());
    fs::remove_file(temp.path().join("config.toml")).expect("remove");
    fs::create_dir(temp.path().join("config.toml")).expect("directory");
    assert!(
        call_default(temp.path(), clock, run, Some(OsString::from("exit 0")))
            .0
            .expect("override unreadable")
            .opened
    );
}

#[test]
fn empty_env_var_falls_back_to_the_config_file() {
    let (temp, clock, run) = setup();
    let marker = temp.path().join("m");
    write_config(
        temp.path(),
        &command_value(format!("touch {}", marker.display())),
    );
    for value in ["", " \t\n"] {
        assert!(
            call_default(
                temp.path(),
                clock.clone(),
                run.clone(),
                Some(OsString::from(value))
            )
            .0
            .expect("ok")
            .opened
        );
        assert!(marker.exists());
        fs::remove_file(&marker).expect("remove");
    }
}

#[test]
fn empty_open_command_is_not_configured() {
    let (temp, clock, run) = setup();
    let base = serde_json::to_value(
        call_default(temp.path(), clock.clone(), run.clone(), None)
            .0
            .expect("base"),
    )
    .expect("value");
    for value in ["", "  \t"] {
        write_config(temp.path(), &command_value(value.to_owned()));
        let output = call_default(temp.path(), clock.clone(), run.clone(), None)
            .0
            .expect("ok");
        assert_eq!(serde_json::to_value(output).expect("value"), base);
    }
}

#[test]
fn failing_command_gives_open_failed() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'echo boom-on-stderr >&2; exit 3'",
    );
    let (code, message) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "open_failed");
    assert!(message.starts_with("the open command failed (exit code 3): "));
    assert!(message.contains("boom-on-stderr"));
}
#[test]
fn failing_command_without_stderr() {
    let (temp, clock, run) = setup();
    write_config(temp.path(), "open_command = 'exit 7'");
    let (code, message) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "open_failed");
    assert_eq!(
        message,
        "the open command failed (exit code 7) with no output on stderr"
    );
}
#[test]
fn command_killed_by_a_signal_gives_open_failed() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'echo dying >&2; kill -TERM $$'",
    );
    let (code, message) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "open_failed");
    assert!(message.contains("(signal 15)"));
    assert!(message.contains("dying"));
}
#[test]
fn open_failed_keeps_only_the_stderr_tail() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'printf HEAD-MARK >&2; head -c 10000 /dev/zero | tr \\0 x >&2; printf TAIL-MARK >&2; exit 2'",
    );
    let (_, message) = error(call_default(temp.path(), clock, run, None).0);
    assert!(message.contains("TAIL-MARK"));
    assert!(!message.contains("HEAD-MARK"));
    assert!(message.split(|c| c != 'x').map(str::len).max().unwrap_or(0) <= 4096);
}
#[test]
fn large_stderr_does_not_block_a_failing_command() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'head -c 204800 /dev/zero | tr \\0 x >&2; exit 5'",
    );
    let start = Instant::now();
    let (code, message) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "open_failed");
    assert!(message.contains("exit code 5"));
    assert!(start.elapsed() < Duration::from_secs(4));
}
#[test]
fn failure_within_the_wait_is_reported() {
    let (temp, clock, run) = setup();
    write_config(temp.path(), "open_command = 'sleep 3; exit 4'");
    let start = Instant::now();
    let (code, message) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "open_failed");
    assert!(message.contains("exit code 4"));
    assert!(start.elapsed() >= Duration::from_millis(2900));
}

#[test]
fn command_still_running_after_the_wait_is_detached() {
    let (temp, clock, run) = setup();
    let pid_path = temp.path().join("pid");
    write_config(
        temp.path(),
        &command_value(format!("echo $$ > {}; exec sleep 20", pid_path.display())),
    );
    let start = Instant::now();
    assert!(
        call_default(temp.path(), clock, run, None)
            .0
            .expect("ok")
            .opened
    );
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(4900) && elapsed < Duration::from_secs(8));
    let pid = fs::read_to_string(&pid_path)
        .expect("pid")
        .trim()
        .to_owned();
    let guard = KillOnDrop(Some(pid.clone()));
    assert!(alive(&pid));
    assert_eq!(pgid(&pid), pid);
    assert_ne!(pgid(&pid), pgid(&std::process::id().to_string()));
    Command::new("kill")
        .args(["-KILL", &pid])
        .status()
        .expect("kill");
    let start = Instant::now();
    while !state(&pid).is_empty() && start.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        state(&pid).is_empty(),
        "child was not reaped: {}",
        state(&pid)
    );
    drop(guard);
}

#[test]
fn stopped_run_gives_run_stopped_and_runs_nothing() {
    let (temp, clock, run) = setup();
    let marker = temp.path().join("m");
    write_config(
        temp.path(),
        &command_value(format!("touch {}", marker.display())),
    );
    let mut store = Store::open_for_run(temp.path(), &run)
        .expect("store")
        .with_clock(clock.clone());
    runs::stop_run(&mut store, &run, StopReason::End).expect("stop");
    let (code, message) = error(call_default(temp.path(), clock.clone(), run.clone(), None).0);
    assert_eq!(code, "run_stopped");
    assert!(message.contains(run.as_str()) && message.contains("has stopped"));
    assert!(!marker.exists());
    write_config(temp.path(), "open_command = ");
    assert_eq!(
        error(call_default(temp.path(), clock, run, None).0).0,
        "run_stopped"
    );
}
#[test]
fn expired_run_gives_run_stopped() {
    let (temp, clock, run) = setup();
    let marker = temp.path().join("m");
    write_config(
        temp.path(),
        &command_value(format!("touch {}", marker.display())),
    );
    clock.advance(LEASE + Duration::from_secs(1));
    assert_eq!(
        error(call_default(temp.path(), clock.clone(), run.clone(), None).0).0,
        "run_stopped"
    );
    assert_eq!(run_lease(temp.path(), &run, clock).0, "expired");
    assert!(!marker.exists());
}
#[test]
fn unknown_run_gives_run_unknown() {
    let (temp, clock, _) = setup();
    let marker = temp.path().join("m");
    write_config(
        temp.path(),
        &command_value(format!("touch {}", marker.display())),
    );
    let requested = RunId::new("r_000000000000");
    let (code, message) =
        error(call_default(temp.path(), clock.clone(), requested.clone(), None).0);
    assert_eq!(code, "run_unknown");
    assert!(message.contains(requested.as_str()));
    assert!(!marker.exists());
    let empty = tempfile::tempdir().expect("empty");
    assert_eq!(
        error(call_default(empty.path(), clock, requested, None).0).0,
        "run_unknown"
    );
}

#[test]
fn unknown_config_keys_warn_on_stderr() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'exit 0'\nzeta = 1\n[alpha]\nk = 1",
    );
    let (output, warnings) = call_default(temp.path(), clock, run, None);
    assert!(output.expect("ok").opened);
    let lines: Vec<serde_json::Value> = String::from_utf8(warnings)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("warning json"))
        .collect();
    assert_eq!(lines.len(), 2);
    for value in &lines {
        let object = value.as_object().expect("object");
        assert_eq!(object.len(), 2);
        assert_eq!(value["warning"], "config_unknown_key");
    }
    let path = temp.path().join("config.toml");
    assert_eq!(
        lines[0]["message"],
        format!("ignoring unknown key `alpha` in {}", path.display())
    );
    assert_eq!(
        lines[1]["message"],
        format!("ignoring unknown key `zeta` in {}", path.display())
    );
}
#[test]
fn malformed_config_is_config_invalid() {
    let (temp, clock, run) = setup();
    let path = temp.path().join("config.toml");
    write_config(temp.path(), "open_command = ");
    let (code, message) = error(call_default(temp.path(), clock.clone(), run.clone(), None).0);
    assert_eq!(code, "config_invalid");
    assert!(
        message.starts_with(&format!("invalid prguide config {}: ", path.display()))
            && message.contains("TOML parse error")
    );
    write_config(temp.path(), "open_command = 5");
    assert_eq!(
        error(call_default(temp.path(), clock.clone(), run.clone(), None).0).1,
        format!(
            "invalid prguide config {}: open_command must be a string",
            path.display()
        )
    );
    fs::write(&path, [0x6f, 0xff, 0x0a]).expect("bytes");
    assert!(
        error(call_default(temp.path(), clock, run, None).0)
            .1
            .ends_with("not UTF-8")
    );
}
#[test]
fn unreadable_config_is_config_unreadable() {
    let (temp, clock, run) = setup();
    fs::create_dir(temp.path().join("config.toml")).expect("dir");
    let (code, message) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "config_unreadable");
    assert!(message.starts_with(&format!(
        "cannot read the prguide config {}: ",
        temp.path().join("config.toml").display()
    )));
}

#[test]
fn binary_config_follows_prguide_home() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("h");
    let user = temp.path().join("u");
    let out = temp.path().join("out");
    fs::create_dir_all(user.join(".prguide")).expect("user");
    fs::create_dir_all(&out).expect("out");
    let mut store =
        Store::open_in(&home, &PrRef::new("octo", "widget", 7).expect("pr")).expect("store");
    let run = runs::start_run(&mut store, false).expect("run");
    drop(store);
    write_config(&home, &command_value(format!("touch {}/h", out.display())));
    write_config(
        &user.join(".prguide"),
        &command_value(format!("touch {}/u", out.display())),
    );
    let output = binary(&home, &run.id, temp.path())
        .env("HOME", &user)
        .output()
        .expect("binary");
    assert!(output.status.success());
    assert!(out.join("h").exists() && !out.join("u").exists());
    fs::remove_file(out.join("h")).expect("remove");
    fs::create_dir_all(user.join(".prguide/prs/github.com/octo/widget/7")).expect("copy path");
    fs::copy(
        home.join("prs/github.com/octo/widget/7/store.db"),
        user.join(".prguide/prs/github.com/octo/widget/7/store.db"),
    )
    .expect("copy");
    let output = Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(["open", "--run", run.id.as_str()])
        .env_remove("PRGUIDE_HOME")
        .env_remove("PRGUIDE_OPEN_CMD")
        .env("HOME", &user)
        .current_dir(temp.path())
        .output()
        .expect("binary");
    assert!(output.status.success());
    assert!(out.join("u").exists());
    fs::remove_file(out.join("u")).expect("remove");
    let output = Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(["open", "--run", run.id.as_str()])
        .env("PRGUIDE_HOME", "")
        .env_remove("PRGUIDE_OPEN_CMD")
        .env("HOME", &user)
        .current_dir(temp.path())
        .output()
        .expect("binary");
    assert!(output.status.success());
    assert!(out.join("u").exists() && !out.join("h").exists());
    let output = binary(&home, &run.id, temp.path())
        .env("PRGUIDE_HOME", "rel/dir")
        .output()
        .expect("binary");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(error_json(&output)["code"], "home_unresolved");
}

#[test]
fn binary_view_command_survives_a_hostile_binary_path() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    let root = fs::canonicalize(temp.path()).expect("canonical");
    let hostile = root.join("b i'n\"$x`y;z\nw");
    fs::create_dir(&hostile).expect("dir");
    let copied = hostile.join("prguide");
    fs::copy(env!("CARGO_BIN_EXE_prguide"), &copied).expect("copy");
    let out = root.join("out");
    fs::create_dir(&out).expect("out");
    let script = format!(
        "printf '%s' \"$PRGUIDE_VIEW_CMD\" > \"{0}/view\"; eval \"set -- $PRGUIDE_VIEW_CMD\"; i=0; for a in \"$@\"; do i=$((i+1)); printf '%s' \"$a\" > \"{0}/arg$i\"; done",
        out.display()
    );
    let output = Command::new(&copied)
        .args(["open", "--run", run.as_str()])
        .env("PRGUIDE_HOME", home.path())
        .env("HOME", &root)
        .env("PRGUIDE_OPEN_CMD", script)
        .current_dir(&root)
        .output()
        .expect("binary");
    assert!(output.status.success());
    assert_eq!(output_json(&output), serde_json::json!({"opened": true}));
    let expected = format!("{} view --run '{}'", shell_quote(&copied), run.as_str());
    assert_eq!(
        fs::read(out.join("view")).expect("view"),
        expected.as_bytes()
    );
    assert_eq!(
        fs::read(out.join("arg1")).expect("arg1"),
        copied.as_os_str().as_encoded_bytes()
    );
    assert_eq!(fs::read(out.join("arg2")).expect("arg2"), b"view");
    assert_eq!(fs::read(out.join("arg3")).expect("arg3"), b"--run");
    assert_eq!(
        fs::read(out.join("arg4")).expect("arg4"),
        run.as_str().as_bytes()
    );
    assert!(copied.is_absolute() && copied.is_file());
    let output = Command::new(&copied)
        .args(["open", "--run", run.as_str()])
        .env("PRGUIDE_HOME", home.path())
        .env("HOME", &root)
        .env_remove("PRGUIDE_OPEN_CMD")
        .current_dir(&root)
        .output()
        .expect("binary");
    assert!(output.status.success());
    assert_eq!(output_json(&output)["view_command"], expected);
    let uncopied_out = root.join("uncopied-out");
    fs::create_dir(&uncopied_out).expect("uncopied out");
    let uncopied_script = format!(
        "printf '%s' \"$PRGUIDE_VIEW_CMD\" > \"{0}/view\"; eval \"set -- $PRGUIDE_VIEW_CMD\"; i=0; for a in \"$@\"; do i=$((i+1)); printf '%s' \"$a\" > \"{0}/arg$i\"; done",
        uncopied_out.display()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(["open", "--run", run.as_str()])
        .env("PRGUIDE_HOME", home.path())
        .env("HOME", &root)
        .env("PRGUIDE_OPEN_CMD", uncopied_script)
        .current_dir(&root)
        .output()
        .expect("uncopied binary");
    assert!(output.status.success());
    let original_arg1 = fs::read_to_string(uncopied_out.join("arg1")).expect("uncopied arg1");
    assert!(Path::new(&original_arg1).is_absolute() && Path::new(&original_arg1).is_file());
}

#[test]
fn binary_stdout_is_only_the_json_line() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    write_config(
        home.path(),
        "open_command = 'echo NOISE; echo ERRNOISE >&2; exit 0'\nextra = 1",
    );
    let output = binary(home.path(), &run, temp.path())
        .output()
        .expect("binary");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{\"opened\":true}\n");
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert_eq!(stderr.lines().count(), 1);
    assert!(stderr.contains("config_unknown_key") && !stderr.contains("NOISE"));
    write_config(
        home.path(),
        "open_command = 'echo NOISE; echo ERRNOISE >&2; exit 3'",
    );
    let output = binary(home.path(), &run, temp.path())
        .output()
        .expect("binary");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let line = String::from_utf8(output.stderr).expect("stderr");
    let error: serde_json::Value =
        serde_json::from_str(line.lines().last().expect("error line")).expect("error json");
    assert_eq!(error["code"], "open_failed");
    assert!(
        error["message"]
            .as_str()
            .expect("message")
            .contains("ERRNOISE")
    );
}

#[test]
fn binary_child_gets_no_stdin() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    let file = temp.path().join("stdin");
    write_config(
        home.path(),
        &command_value(format!("cat > {}; exit 0", file.display())),
    );
    let start = Instant::now();
    let mut child = binary(home.path(), &run, temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("binary");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(b"LEAK\n").expect("write");
    assert!(child.wait().expect("wait").success());
    assert!(start.elapsed() < Duration::from_secs(4));
    drop(stdin);
    assert!(fs::read(file).expect("stdin").is_empty());
}
#[test]
fn binary_detached_child_does_not_hold_prguides_output() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    let pid_path = temp.path().join("pid");
    write_config(
        home.path(),
        &command_value(format!("echo $$ > {}; exec sleep 20", pid_path.display())),
    );
    let start = Instant::now();
    let output = binary(home.path(), &run, temp.path())
        .output()
        .expect("binary");
    assert!(
        start.elapsed() >= Duration::from_millis(4900) && start.elapsed() < Duration::from_secs(8)
    );
    assert_eq!(output.stdout, b"{\"opened\":true}\n");
    let pid = fs::read_to_string(pid_path).expect("pid").trim().to_owned();
    let _guard = KillOnDrop(Some(pid.clone()));
    assert!(alive(&pid));
}
#[test]
fn binary_detached_child_keeps_writing_after_prguide_exits() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    let pid_path = temp.path().join("pid");
    let count = temp.path().join("count");
    write_config(
        home.path(),
        &command_value(format!(
            "echo $$ > {0}; i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo \"line $i\" >&2; echo $i > {1}; sleep 0.05; done",
            pid_path.display(),
            count.display()
        )),
    );
    let mut child = binary(home.path(), &run, temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("binary");
    assert!(child.wait().expect("wait").success());
    let pid = fs::read_to_string(pid_path).expect("pid").trim().to_owned();
    let _guard = KillOnDrop(Some(pid.clone()));
    let first: i32 = fs::read_to_string(&count)
        .expect("count")
        .trim()
        .parse()
        .expect("number");
    thread::sleep(Duration::from_secs(1));
    let second: i32 = fs::read_to_string(&count)
        .expect("count")
        .trim()
        .parse()
        .expect("number");
    assert!(second > first && alive(&pid));
}
#[test]
fn binary_child_survives_a_kill_of_prguides_process_group() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    let pid_path = temp.path().join("pid");
    write_config(
        home.path(),
        &command_value(format!("echo $$ > {}; exec sleep 20", pid_path.display())),
    );
    let mut command = binary(home.path(), &run, temp.path());
    command.process_group(0);
    let mut child = command.spawn().expect("spawn");
    wait_for(&pid_path, Duration::from_secs(3));
    let pid = fs::read_to_string(&pid_path)
        .expect("pid")
        .trim()
        .to_owned();
    let _guard = KillOnDrop(Some(pid.clone()));
    assert_ne!(pgid(&pid), child.id().to_string());
    Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .status()
        .expect("kill group");
    assert_eq!(child.wait().expect("wait").signal(), Some(9));
    thread::sleep(Duration::from_millis(300));
    assert!(alive(&pid));
}
#[test]
fn binary_unusable_tmpdir_is_open_failed() {
    let (home, run) = setup_binary();
    let temp = tempfile::tempdir().expect("temp");
    let output = binary(home.path(), &run, temp.path())
        .env("TMPDIR", temp.path().join("missing"))
        .env("PRGUIDE_OPEN_CMD", "exit 0")
        .output()
        .expect("binary");
    assert_eq!(output.status.code(), Some(1));
    let body = error_json(&output);
    assert_eq!(body["code"], "open_failed");
    assert!(
        body["message"]
            .as_str()
            .expect("message")
            .starts_with("cannot run the open command: cannot create a file for its stderr")
    );
}

fn scan(root: &Path) -> (Vec<String>, Vec<String>) {
    fn walk(root: &Path, path: &Path, visited: &mut Vec<String>, hits: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .expect("dir")
            .map(|entry| entry.expect("entry"))
            .collect();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let kind = fs::symlink_metadata(&path).expect("metadata").file_type();
            if kind.is_dir() {
                walk(root, &path, visited, hits);
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("relative")
                    .to_string_lossy()
                    .replace('\\', "/");
                visited.push(relative.clone());
                let lower: Vec<u8> = fs::read(path)
                    .expect("read")
                    .into_iter()
                    .map(|byte| byte.to_ascii_lowercase())
                    .collect();
                for (line, text) in lower.split(|byte| *byte == b'\n').enumerate() {
                    for needle in [b"herdr".as_slice(), b"tmux", b"zellij", b"wezterm"] {
                        if text.windows(needle.len()).any(|window| window == needle) {
                            hits.push(format!(
                                "{}:{}: {}",
                                relative,
                                line + 1,
                                String::from_utf8_lossy(needle)
                            ));
                        }
                    }
                }
            }
        }
    }
    let mut visited = Vec::new();
    let mut hits = Vec::new();
    walk(root, root, &mut visited, &mut hits);
    (visited, hits)
}
#[test]
fn src_names_no_multiplexer() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let (visited, hits) = scan(&root);
    assert!(hits.is_empty(), "{hits:?}");
    assert!(visited.len() >= 60);
    for required in [
        "lib.rs",
        "cli/open.rs",
        "launcher.rs",
        "config.rs",
        "viewer/README.md",
    ] {
        assert!(visited.contains(&required.to_owned()), "missing {required}");
    }
}
#[test]
fn multiplexer_scan_catches_planted_files() {
    let temp = tempfile::tempdir().expect("temp");
    for (name, contents) in [
        ("a/b/c.rs", "// WezTerm"),
        ("x.md", "uses $TMUX_PANE"),
        ("y.txt", "HERDR_PANE_ID=1"),
        ("d/z.toml", "Zellij"),
        ("t.rs", "tmux"),
        ("clean.rs", "terminal multiplexer"),
    ] {
        let path = temp.path().join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, contents).expect("write");
    }
    let (_, mut hits) = scan(temp.path());
    hits.sort();
    assert_eq!(
        hits,
        vec![
            "a/b/c.rs:1: wezterm",
            "d/z.toml:1: zellij",
            "t.rs:1: tmux",
            "x.md:1: tmux",
            "y.txt:1: herdr",
        ]
    );
}
