//! Runs the reviewer's open command with `/bin/sh -c`.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::store::runs::RunId;

pub const OPEN_WAIT: Duration = Duration::from_secs(5);
pub const STDERR_TAIL: usize = 4096;
pub const VIEW_CMD_ENV: &str = "PRGUIDE_VIEW_CMD";

/// Single-quotes `value` for POSIX shells, preserving its bytes.
pub fn shell_quote(value: &OsStr) -> OsString {
    let bytes = value.as_bytes();
    let mut quoted = Vec::with_capacity(bytes.len() + 2);
    quoted.push(b'\'');
    for byte in bytes {
        if *byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    OsString::from_vec(quoted)
}

/// The command which starts the viewer for `run_id`.
pub fn view_command(exe: &Path, run_id: &RunId) -> OsString {
    let mut command = shell_quote(exe.as_os_str());
    command.push(" view --run ");
    command.push(shell_quote(OsStr::new(run_id.as_str())));
    command
}

#[derive(Debug, PartialEq, Eq)]
pub enum Launched {
    Exited,
    StillRunning,
}

pub fn launch(command: &OsStr, view_command: &OsStr) -> Result<Launched, LaunchError> {
    let mut capture = tempfile::tempfile().map_err(|error| LaunchError::Spawn {
        reason: format!("cannot create a file for its stderr: {error}"),
    })?;
    let stderr = capture.try_clone().map_err(|error| LaunchError::Spawn {
        reason: format!("cannot create a file for its stderr: {error}"),
    })?;
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .env(VIEW_CMD_ENV, view_command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .map_err(|error| LaunchError::Spawn {
            reason: error.to_string(),
        })?;
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|error| LaunchError::Spawn {
            reason: error.to_string(),
        })? {
            Some(status) if status.success() => return Ok(Launched::Exited),
            Some(status) => {
                return Err(LaunchError::OpenFailed {
                    status: status_text(status),
                    stderr: stderr_tail(&mut capture),
                });
            }
            None if start.elapsed() >= OPEN_WAIT => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(Launched::StillRunning);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn status_text(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exit code {code}")
    } else if let Some(signal) = status.signal() {
        format!("signal {signal}")
    } else {
        "unknown status".to_owned()
    }
}

fn stderr_tail(capture: &mut File) -> String {
    let len = match capture.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return String::new(),
    };
    let offset = len.saturating_sub(STDERR_TAIL as u64);
    if capture.seek(SeekFrom::Start(offset)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if capture.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("{}", open_failed_message(.status, .stderr))]
    OpenFailed { status: String, stderr: String },
    #[error("cannot run the open command: {reason}")]
    Spawn { reason: String },
}

fn open_failed_message(status: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        format!("the open command failed ({status}) with no output on stderr")
    } else {
        format!("the open command failed ({status}): {stderr}")
    }
}

impl LaunchError {
    pub fn code(&self) -> &'static str {
        "open_failed"
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::Path,
    };

    use crate::store::runs::RunId;

    use super::{LaunchError, shell_quote, view_command};

    #[test]
    fn shell_quote_single_quotes_everything() {
        assert_eq!(shell_quote("".as_ref()), "''");
        assert_eq!(shell_quote("abc".as_ref()), "'abc'");
        assert_eq!(shell_quote("a'b".as_ref()), "'a'\\''b'");
        assert_eq!(shell_quote("''".as_ref()), "''\\'''\\'''");
        assert_eq!(
            shell_quote(OsString::from_vec(vec![b'f', 0xff]).as_ref()).as_bytes(),
            &[b'\'', b'f', 0xff, b'\'']
        );
    }

    #[test]
    fn view_command_quotes_path_and_run_id() {
        assert_eq!(
            view_command(Path::new("/a b/prguide"), &RunId::new("r_1")),
            "'/a b/prguide' view --run 'r_1'"
        );
    }

    #[test]
    fn launch_error_codes_are_stable() {
        assert_eq!(
            LaunchError::OpenFailed {
                status: String::new(),
                stderr: String::new()
            }
            .code(),
            "open_failed"
        );
        assert_eq!(
            LaunchError::Spawn {
                reason: String::new()
            }
            .code(),
            "open_failed"
        );
    }
}
