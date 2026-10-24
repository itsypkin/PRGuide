use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
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
    let temp = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(Timestamp::from_unix_millis(T0)));
    let mut store = Store::open_in(temp.path(), &PrRef::new("octo", "widget", 7).unwrap())
        .unwrap()
        .with_clock(clock.clone());
    let run = runs::start_run(&mut store, false).unwrap();
    (temp, clock, run.id)
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
        sender.send((result, warnings)).unwrap();
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
    fs::write(home.join("config.toml"), contents).unwrap();
}
fn error(result: Result<OpenOutput, (String, String)>) -> (String, String) {
    result.expect_err("expected an error")
}

#[test]
fn no_open_command_prints_the_viewer_command() {
    let (temp, clock, run) = setup();
    assert_eq!(run.as_str().len(), 14);
    assert!(run.as_str().starts_with("r_"));
    clock.advance(Duration::from_secs(10 * 60));
    let (output, warnings) = call_default(temp.path(), clock.clone(), run.clone(), None);
    assert!(warnings.is_empty());
    let output = output.unwrap();
    let snapshot = serde_json::to_string_pretty(&output)
        .unwrap()
        .replace(run.as_str(), "[RUN]");
    insta::assert_snapshot!("opened_false", snapshot);
    let mut store = Store::open_for_run(temp.path(), &run)
        .unwrap()
        .with_clock(clock.clone());
    let lease: String = store
        .tx(|tx| {
            tx.query_row(
                "SELECT lease_until FROM runs WHERE id = ?1",
                [run.as_str()],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
        })
        .unwrap();
    assert_eq!(lease, clock.now().saturating_add(LEASE).to_rfc3339());
}

#[test]
fn successful_command_prints_opened_true() {
    let (temp, clock, run) = setup();
    write_config(temp.path(), "open_command = 'exit 0'");
    let started = Instant::now();
    let (output, _) = call_default(temp.path(), clock, run, None);
    assert!(started.elapsed() < Duration::from_secs(2));
    insta::assert_snapshot!(
        "opened_true",
        serde_json::to_string_pretty(&output.unwrap()).unwrap()
    );
}

#[test]
fn configured_command_receives_view_command() {
    let (temp, clock, run) = setup();
    let root = fs::canonicalize(temp.path()).unwrap();
    let out = root.join("out");
    fs::create_dir(&out).unwrap();
    let exe = root.join("b i'n\"$x`y;z\nw").join("prguide");
    let script = format!(
        "printf '%s' \"$PRGUIDE_VIEW_CMD\" > \"{}/view\"; eval \"set -- $PRGUIDE_VIEW_CMD\"; i=0; for a in \"$@\"; do i=$((i+1)); printf '%s' \"$a\" > \"{}/arg$i\"; done",
        out.display(),
        out.display()
    );
    write_config(
        temp.path(),
        &format!("open_command = {}", toml::Value::String(script)),
    );
    assert!(
        call(
            temp.path().to_path_buf(),
            clock,
            run.clone(),
            None,
            exe.clone()
        )
        .0
        .unwrap()
        .opened
    );
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    assert_eq!(
        fs::read(out.join("view")).unwrap(),
        format!(
            "{} view --run {}",
            quote(&exe.to_string_lossy()),
            quote(run.as_str())
        )
        .as_bytes()
    );
    assert_eq!(
        fs::read(out.join("arg1")).unwrap(),
        exe.as_os_str().as_encoded_bytes()
    );
    assert_eq!(fs::read(out.join("arg2")).unwrap(), b"view");
    assert_eq!(fs::read(out.join("arg3")).unwrap(), b"--run");
    assert_eq!(fs::read(out.join("arg4")).unwrap(), run.as_str().as_bytes());
    assert!(!out.join("arg5").exists());
}

#[test]
fn env_var_overrides_config_and_empty_values_fall_back() {
    let (temp, clock, run) = setup();
    let marker = temp.path().join("marker");
    write_config(
        temp.path(),
        &format!(
            "open_command = {}",
            toml::Value::String(format!("touch {}", marker.display()))
        ),
    );
    assert!(
        call_default(
            temp.path(),
            clock.clone(),
            run.clone(),
            Some(OsString::from("exit 0"))
        )
        .0
        .unwrap()
        .opened
    );
    assert!(!marker.exists());
    for value in ["", " \t\n"] {
        assert!(
            call_default(
                temp.path(),
                clock.clone(),
                run.clone(),
                Some(OsString::from(value))
            )
            .0
            .unwrap()
            .opened
        );
        assert!(marker.exists());
        fs::remove_file(&marker).unwrap();
    }
}

#[test]
fn invalid_config_and_stopped_runs_fail_closed() {
    let (temp, clock, run) = setup();
    write_config(temp.path(), "open_command = ");
    let (code, message) = error(call_default(temp.path(), clock.clone(), run.clone(), None).0);
    assert_eq!(code, "config_invalid");
    assert!(message.contains("TOML parse error"));
    let mut store = Store::open_for_run(temp.path(), &run)
        .unwrap()
        .with_clock(clock.clone());
    runs::stop_run(&mut store, &run, StopReason::End).unwrap();
    let (code, _) = error(call_default(temp.path(), clock, run, None).0);
    assert_eq!(code, "run_stopped");
}

#[test]
fn failures_report_stderr_tail_and_signal() {
    for (command, required) in [
        ("echo boom >&2; exit 3", "exit code 3"),
        ("echo dying >&2; kill -TERM $$", "signal 15"),
        ("exit 7", "no output on stderr"),
    ] {
        let (temp, clock, run) = setup();
        write_config(
            temp.path(),
            &format!("open_command = {}", toml::Value::String(command.to_owned())),
        );
        let (code, message) = error(call_default(temp.path(), clock, run, None).0);
        assert_eq!(code, "open_failed");
        assert!(message.contains(required));
    }
}

#[test]
fn large_stderr_does_not_block() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'printf HEAD-MARK >&2; head -c 204800 /dev/zero | tr \\\\0 x >&2; printf TAIL-MARK >&2; exit 5'",
    );
    let started = Instant::now();
    let (_, message) = error(call_default(temp.path(), clock, run, None).0);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(
        message.contains("exit code 5")
            && message.contains("TAIL-MARK")
            && !message.contains("HEAD-MARK")
    );
}

#[test]
fn command_still_running_after_wait_is_detached() {
    let (temp, clock, run) = setup();
    let pid = temp.path().join("pid");
    write_config(
        temp.path(),
        &format!(
            "open_command = {}",
            toml::Value::String(format!("echo $$ > {}; exec sleep 20", pid.display()))
        ),
    );
    let started = Instant::now();
    assert!(
        call_default(temp.path(), clock, run, None)
            .0
            .unwrap()
            .opened
    );
    assert!(started.elapsed() >= Duration::from_millis(4900));
    let child = fs::read_to_string(&pid).unwrap().trim().to_owned();
    assert!(
        std::process::Command::new("kill")
            .args(["-0", &child])
            .status()
            .unwrap()
            .success()
    );
    std::process::Command::new("kill")
        .args(["-KILL", &child])
        .status()
        .unwrap();
}

#[test]
fn unknown_run_is_rejected() {
    let (temp, clock, _) = setup();
    let (code, message) =
        error(call_default(temp.path(), clock, RunId::new("r_000000000000"), None).0);
    assert_eq!(code, "run_unknown");
    assert!(message.contains("r_000000000000"));
}

#[test]
fn unknown_keys_warn_to_the_supplied_writer() {
    let (temp, clock, run) = setup();
    write_config(
        temp.path(),
        "open_command = 'exit 0'\nzeta = 1\n[alpha]\nk = 1",
    );
    let (_, warnings) = call_default(temp.path(), clock, run, None);
    let text = String::from_utf8(warnings).unwrap();
    assert!(
        text.contains("config_unknown_key") && text.contains("`alpha`") && text.contains("`zeta`")
    );
}

#[test]
fn binary_unusable_tmpdir_is_open_failed() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open_in(temp.path(), &PrRef::new("octo", "widget", 7).unwrap()).unwrap();
    let run = runs::start_run(&mut store, false).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_prguide"))
        .args(["open", "--run", run.id.as_str()])
        .env("PRGUIDE_HOME", temp.path())
        .env("TMPDIR", temp.path().join("missing"))
        .env("PRGUIDE_OPEN_CMD", "exit 0")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let body: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(body["code"], "open_failed");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("cannot run the open command: cannot create a file for its stderr")
    );
}

#[test]
fn src_names_no_multiplexer() {
    fn scan(root: &Path, path: &Path, visited: &mut Vec<String>, hits: &mut Vec<String>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = fs::symlink_metadata(&path).unwrap().file_type();
            if kind.is_dir() {
                scan(root, &path, visited, hits);
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                visited.push(relative.clone());
                let bytes = fs::read(path).unwrap();
                let lower: Vec<u8> = bytes
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

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut visited = Vec::new();
    let mut hits = Vec::new();
    scan(&root, &root, &mut visited, &mut hits);
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
