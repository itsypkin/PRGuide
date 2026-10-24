//! Event waiting command.

use std::{num::NonZeroU32, time::Duration};

use crate::{
    Error,
    cli::output,
    store::{Store, events, prguide_home, runs::RunId},
};

#[derive(Debug, clap::Args)]
pub struct WaitArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub timeout: Option<String>,
    #[arg(long)]
    pub max: Option<u32>,
}

fn timeout_error(input: &str) -> Error {
    Error::Usage(format!(
        "invalid --timeout {input:?}: use a whole number followed by s, m or h, like 30s or 4m"
    ))
}

fn parse_timeout(input: &str) -> Result<Duration, Error> {
    let Some(unit) = input.chars().last() else {
        return Err(timeout_error(input));
    };
    let digits = &input[..input.len().saturating_sub(unit.len_utf8())];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(timeout_error(input));
    }
    let value = digits.parse::<u64>().map_err(|_| timeout_error(input))?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return Err(timeout_error(input)),
    };
    let seconds = value
        .checked_mul(multiplier)
        .ok_or_else(|| timeout_error(input))?;
    Ok(Duration::from_secs(seconds))
}

pub fn run(args: WaitArgs) -> Result<(), Error> {
    let timeout = args.timeout.as_deref().map(parse_timeout).transpose()?;
    let max = NonZeroU32::new(args.max.unwrap_or(events::DEFAULT_MAX))
        .ok_or_else(|| Error::Usage("--max must be at least 1".into()))?;
    let run_id = RunId::new(args.run);
    let mut store = Store::open_for_run(&prguide_home()?, &run_id)?;
    let outcome = events::wait(&mut store, &run_id, timeout, max)?;
    output::print_json(&outcome)
}

#[cfg(test)]
mod tests {
    use super::parse_timeout;
    use crate::Error;
    use std::time::Duration;

    #[test]
    fn timeout_grammar_accepts_whole_units() {
        for (input, want) in [
            ("0s", 0),
            ("30s", 30),
            ("4m", 240),
            ("2h", 7200),
            ("04m", 240),
            ("18446744073709551615s", u64::MAX),
        ] {
            assert_eq!(
                parse_timeout(input).expect("valid"),
                Duration::from_secs(want)
            );
        }
    }

    #[test]
    fn timeout_grammar_rejects_everything_else() {
        for input in [
            "",
            "4",
            "m",
            "4x",
            "-1s",
            "+4m",
            " 4m",
            "4m ",
            "4 m",
            "1.5m",
            "4M",
            "4S",
            "4m30s",
            "4ms",
            "٤m",
            "18446744073709551616s",
            "307445734561825861m",
        ] {
            let Error::Usage(message) = parse_timeout(input).expect_err("invalid") else {
                panic!("usage")
            };
            assert!(message.contains("invalid --timeout"));
            assert!(message.contains(input));
        }
    }
}
