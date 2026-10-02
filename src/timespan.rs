//! Parser for the subset of systemd time-span syntax the module's options use
//! (`"15min"`, `"1h 30min"`, `"90"`), so `pull.interval = "15min"` keeps meaning
//! what it always has.

use std::time::Duration;

use anyhow::{Result, bail};

pub fn parse(input: &str) -> Result<Duration> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty time span");
    }
    // A bare number is seconds, as in systemd.
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }

    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            bail!("invalid time span {input:?}: expected a number at {rest:?}");
        }
        let n: u64 = rest[..digits].parse()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        let part = match unit {
            "us" | "usec" => Duration::from_micros(n),
            "ms" | "msec" => Duration::from_millis(n),
            "" | "s" | "sec" | "second" | "seconds" => Duration::from_secs(n),
            "m" | "min" | "minute" | "minutes" => Duration::from_secs(n * 60),
            "h" | "hr" | "hour" | "hours" => Duration::from_secs(n * 3600),
            "d" | "day" | "days" => Duration::from_secs(n * 86_400),
            "w" | "week" | "weeks" => Duration::from_secs(n * 7 * 86_400),
            other => bail!("invalid time span {input:?}: unknown unit {other:?}"),
        };
        total += part;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_defaults() {
        assert_eq!(parse("15min").unwrap(), Duration::from_secs(900));
        assert_eq!(parse("5min").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn compound_and_bare() {
        assert_eq!(parse("1h 30min").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse("1h30min").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse("2d").unwrap(), Duration::from_secs(172_800));
        assert_eq!(parse("250ms").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "min", "5parsecs", "-5s", "1.5h"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
