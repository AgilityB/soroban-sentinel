//! Historical TTL trend analysis and threshold-crossing projection (`sentinel trend`).
//!
//! Ingests a local directory of saved JSON scan reports, orders them by
//! timestamp and ledger sequence, detects discontinuous positive jumps (TTL
//! bumps), fits ordinary least-squares linear regressions per storage entry,
//! and projects calendar dates (with 95% confidence intervals) when entries
//! will cross warning and critical thresholds.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sentinel_core::domain::compare::MAX_BASELINE_FILE_BYTES;
use sentinel_core::domain::ttl::{SECONDS_PER_DAY, TARGET_LEDGER_CLOSE_SECONDS};
use serde::{Deserialize, Serialize};

use crate::args::TrendArgs;
use crate::commands::{resolve_threshold_config, EXIT_INVALID_ARGS, EXIT_OK};

/// Origin of the wall-clock timestamps used for regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampSource {
    /// Explicit `timestamp` / `scanned_at` field inside the JSON reports.
    Report,
    /// Date/timestamp parsed from report filenames (e.g. `scan-2026-01-15.json`).
    Filename,
    /// Derived from `latest_ledger` and `ledger_velocity_seconds` anchored to the newest report.
    LedgerVelocity,
    /// Mix of explicit timestamps and ledger-derived timestamps.
    Mixed,
}

/// Deserialized JSON scan snapshot with optional explicit timestamp metadata.
#[derive(Debug, Clone, Deserialize)]
struct RawSnapshotReport {
    contract_id: String,
    network: String,
    latest_ledger: u32,
    #[serde(default = "default_ledger_velocity")]
    ledger_velocity_seconds: f64,
    #[serde(default, alias = "scanned_at", alias = "generated_at")]
    timestamp: Option<RawTimestamp>,
    #[serde(default)]
    entries: Vec<RawEntrySnapshot>,
}

fn default_ledger_velocity() -> f64 {
    TARGET_LEDGER_CLOSE_SECONDS
}

/// Flexible JSON timestamp supporting Unix epoch seconds or ISO-8601 UTC strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawTimestamp {
    UnixSeconds(i64),
    UnixFloat(f64),
    Iso8601(String),
}

/// Entry fields extracted from a JSON scan report.
#[derive(Debug, Clone, Deserialize)]
struct RawEntrySnapshot {
    identifier: String,
    #[serde(default)]
    size_bytes: usize,
    #[serde(default)]
    live_until_ledger: u32,
    remaining_ledgers: i64,
}

/// Normalized historical snapshot ready for chronological ordering and regression.
#[derive(Debug, Clone)]
pub struct IngestedSnapshot {
    pub file_name: String,
    pub contract_id: String,
    pub network: String,
    pub latest_ledger: u32,
    pub ledger_velocity_seconds: f64,
    pub timestamp_secs: i64,
    pub explicit_timestamp: bool,
    pub entries: Vec<SnapshotEntryPoint>,
}

/// Single storage entry measurement within an ingested snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotEntryPoint {
    pub identifier: String,
    pub size_bytes: usize,
    pub live_until_ledger: u32,
    pub remaining_ledgers: i64,
}

/// Result of ingesting and ordering snapshots from an input directory.
#[derive(Debug, Clone)]
pub struct IngestedSeries {
    pub contract_id: String,
    pub network: String,
    pub timestamp_source: TimestampSource,
    pub snapshots: Vec<IngestedSnapshot>,
}

/// Entry point for `sentinel trend`.
pub fn run_trend(args: TrendArgs) -> i32 {
    if let Err(err) = resolve_threshold_config(args.warning_threshold, args.critical_threshold) {
        eprintln!("Error: {err}");
        return EXIT_INVALID_ARGS;
    }

    let series = match ingest_snapshots(&args.input_dir, args.contract.as_deref()) {
        Ok(series) => series,
        Err(err) => {
            eprintln!("Error: {err}");
            return EXIT_INVALID_ARGS;
        }
    };

    if args.format == "json" {
        let payload = serde_json::json!({
            "contract_id": series.contract_id,
            "network": series.network,
            "snapshot_count": series.snapshots.len(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("trend summary is serializable")
        );
    } else {
        println!(
            "Ingested {} snapshots for {} on {}",
            series.snapshots.len(),
            series.contract_id,
            series.network
        );
    }

    EXIT_OK
}

struct PendingSnapshot {
    file_name: String,
    report: RawSnapshotReport,
    report_timestamp: Option<i64>,
    filename_timestamp: Option<i64>,
    mtime_secs: Option<i64>,
}

/// Reads all `.json` scan reports in `input_dir`, optionally filters by `contract_filter`,
/// resolves timestamps, and returns snapshots ordered by timestamp/ledger sequence.
pub fn ingest_snapshots(
    input_dir: &Path,
    contract_filter: Option<&str>,
) -> Result<IngestedSeries, String> {
    if !input_dir.exists() {
        return Err(format!(
            "input directory '{}' does not exist",
            input_dir.display()
        ));
    }
    if !input_dir.is_dir() {
        return Err(format!(
            "input path '{}' is not a directory",
            input_dir.display()
        ));
    }

    let mut json_paths: Vec<PathBuf> = Vec::new();
    let entries = fs::read_dir(input_dir)
        .map_err(|e| format!("failed to read directory '{}': {e}", input_dir.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| format!("failed to read entry in '{}': {e}", input_dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if is_json {
            json_paths.push(path);
        }
    }

    // Deterministic filesystem traversal order before semantic sorting.
    json_paths.sort();

    if json_paths.is_empty() {
        return Err(format!(
            "no JSON scan reports found in '{}'",
            input_dir.display()
        ));
    }

    let mut pending: Vec<PendingSnapshot> = Vec::with_capacity(json_paths.len());
    for path in json_paths {
        let metadata = fs::metadata(&path)
            .map_err(|e| format!("failed to stat '{}': {e}", path.display()))?;
        if metadata.len() > MAX_BASELINE_FILE_BYTES {
            return Err(format!(
                "report '{}' exceeds maximum allowed size of {} bytes",
                path.display(),
                MAX_BASELINE_FILE_BYTES
            ));
        }

        let contents = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
        let report: RawSnapshotReport = serde_json::from_str(&contents)
            .map_err(|e| format!("invalid JSON scan report '{}': {e}", path.display()))?;

        let report_timestamp = match &report.timestamp {
            Some(raw) => Some(parse_raw_timestamp(raw).map_err(|e| {
                format!("invalid timestamp in '{}': {e}", path.display())
            })?),
            None => None,
        };

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let filename_timestamp = extract_timestamp_from_filename(&file_name);
        let mtime_secs = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        pending.push(PendingSnapshot {
            file_name,
            report,
            report_timestamp,
            filename_timestamp,
            mtime_secs,
        });
    }

    if let Some(target_contract) = contract_filter {
        pending.retain(|item| item.report.contract_id == target_contract);
        if pending.is_empty() {
            return Err(format!(
                "no scan reports matching contract '{}' found in '{}'",
                target_contract,
                input_dir.display()
            ));
        }
    } else {
        let contracts: BTreeSet<&str> = pending
            .iter()
            .map(|item| item.report.contract_id.as_str())
            .collect();
        if contracts.len() > 1 {
            let list = contracts.into_iter().collect::<Vec<_>>().join(", ");
            return Err(format!(
                "multiple contracts found in '{}': {list}; specify --contract <ID> to select one",
                input_dir.display()
            ));
        }
    }

    if pending.len() < 2 {
        return Err(format!(
            "at least 2 historical scan reports are required for trend analysis (found {})",
            pending.len()
        ));
    }

    let contract_id = pending[0].report.contract_id.clone();
    let network = pending[0].report.network.clone();
    for item in &pending[1..] {
        if !item.report.network.eq_ignore_ascii_case(&network) {
            return Err(format!(
                "reports for contract '{}' span multiple networks: '{}' vs '{}'",
                contract_id, network, item.report.network
            ));
        }
    }

    // Resolve wall-clock timestamps.
    let all_have_report_ts = pending.iter().all(|p| p.report_timestamp.is_some());
    let all_have_filename_ts = pending
        .iter()
        .all(|p| p.report_timestamp.is_none() && p.filename_timestamp.is_some());
    let any_explicit_ts = pending
        .iter()
        .any(|p| p.report_timestamp.is_some() || p.filename_timestamp.is_some());

    // Anchor timestamp and ledger for ledger-velocity-derived timestamps.
    let anchor = pending
        .iter()
        .max_by_key(|p| {
            (
                p.report_timestamp.or(p.filename_timestamp).is_some(),
                p.report.latest_ledger,
            )
        })
        .expect("pending has at least 2 items");
    let anchor_ledger = anchor.report.latest_ledger;
    let anchor_ts = anchor
        .report_timestamp
        .or(anchor.filename_timestamp)
        .or(anchor.mtime_secs)
        .unwrap_or_else(unix_timestamp_now);

    let mut velocity_sum = 0.0;
    let mut velocity_count = 0usize;
    for item in &pending {
        let v = item.report.ledger_velocity_seconds;
        if v.is_finite() && (1.0..=60.0).contains(&v) {
            velocity_sum += v;
            velocity_count += 1;
        }
    }
    let avg_velocity = if velocity_count > 0 {
        velocity_sum / velocity_count as f64
    } else {
        TARGET_LEDGER_CLOSE_SECONDS
    };

    let timestamp_source = if all_have_report_ts {
        TimestampSource::Report
    } else if all_have_filename_ts {
        TimestampSource::Filename
    } else if any_explicit_ts {
        TimestampSource::Mixed
    } else {
        TimestampSource::LedgerVelocity
    };

    let mut snapshots: Vec<IngestedSnapshot> = pending
        .into_iter()
        .map(|item| {
            let explicit = item.report_timestamp.or(item.filename_timestamp);
            let (timestamp_secs, explicit_timestamp) = match explicit {
                Some(ts) => (ts, true),
                None => {
                    let ledger_delta =
                        i64::from(item.report.latest_ledger) - i64::from(anchor_ledger);
                    let offset_secs = (ledger_delta as f64 * avg_velocity).round() as i64;
                    (anchor_ts.saturating_add(offset_secs), false)
                }
            };
            let velocity = if item.report.ledger_velocity_seconds.is_finite()
                && (1.0..=60.0).contains(&item.report.ledger_velocity_seconds)
            {
                item.report.ledger_velocity_seconds
            } else {
                avg_velocity
            };
            IngestedSnapshot {
                file_name: item.file_name,
                contract_id: item.report.contract_id,
                network: item.report.network,
                latest_ledger: item.report.latest_ledger,
                ledger_velocity_seconds: velocity,
                timestamp_secs,
                explicit_timestamp,
                entries: item
                    .report
                    .entries
                    .into_iter()
                    .map(|e| SnapshotEntryPoint {
                        identifier: e.identifier,
                        size_bytes: e.size_bytes,
                        live_until_ledger: e.live_until_ledger,
                        remaining_ledgers: e.remaining_ledgers,
                    })
                    .collect(),
            }
        })
        .collect();

    // Order chronologically by timestamp, breaking ties by ledger sequence and filename.
    snapshots.sort_by(|a, b| {
        a.timestamp_secs
            .cmp(&b.timestamp_secs)
            .then_with(|| a.latest_ledger.cmp(&b.latest_ledger))
            .then_with(|| a.file_name.cmp(&b.file_name))
    });

    Ok(IngestedSeries {
        contract_id,
        network,
        timestamp_source,
        snapshots,
    })
}

fn parse_raw_timestamp(raw: &RawTimestamp) -> Result<i64, String> {
    match raw {
        RawTimestamp::UnixSeconds(s) => Ok(*s),
        RawTimestamp::UnixFloat(f) => {
            if !f.is_finite() {
                return Err("non-finite numeric timestamp".to_string());
            }
            Ok(f.round() as i64)
        }
        RawTimestamp::Iso8601(s) => parse_iso8601_utc(s),
    }
}

/// Parses an ISO-8601 date or UTC timestamp (`YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ`,
/// optional fractional seconds, optional `±HH:MM` offset) into Unix epoch seconds.
pub(crate) fn parse_iso8601_utc(input: &str) -> Result<i64, String> {
    let s = input.trim();
    if s.len() < 10 {
        return Err(format!("unrecognized timestamp '{input}'"));
    }

    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(format!("expected YYYY-MM-DD prefix in '{input}'"));
    }

    let year: i64 = s[0..4]
        .parse()
        .map_err(|_| format!("invalid year in '{input}'"))?;
    let month: i64 = s[5..7]
        .parse()
        .map_err(|_| format!("invalid month in '{input}'"))?;
    let day: i64 = s[8..10]
        .parse()
        .map_err(|_| format!("invalid day in '{input}'"))?;

    validate_calendar_date(year, month, day)?;
    let base_days = days_from_civil(year, month, day);

    if s.len() == 10 {
        return Ok(base_days * SECONDS_PER_DAY as i64);
    }

    let sep = bytes[10];
    if sep != b'T' && sep != b't' && sep != b' ' {
        return Err(format!("expected 'T' separator in '{input}'"));
    }
    let time_part = &s[11..];
    if time_part.len() < 8 {
        return Err(format!("incomplete time component in '{input}'"));
    }
    let tbytes = time_part.as_bytes();
    if (tbytes[2] != b':' && tbytes[2] != b'-') || (tbytes[5] != b':' && tbytes[5] != b'-') {
        return Err(format!("invalid time format in '{input}'"));
    }
    let hour: i64 = time_part[0..2]
        .parse()
        .map_err(|_| format!("invalid hour in '{input}'"))?;
    let minute: i64 = time_part[3..5]
        .parse()
        .map_err(|_| format!("invalid minute in '{input}'"))?;
    let second: i64 = time_part[6..8]
        .parse()
        .map_err(|_| format!("invalid second in '{input}'"))?;

    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return Err(format!("time out of range in '{input}'"));
    }

    let mut rest = &time_part[8..];
    // Skip optional fractional seconds (.123456)
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            return Err(format!("invalid fractional seconds in '{input}'"));
        }
        rest = &after_dot[digits..];
    }

    let tz_offset_secs: i64 = if rest.is_empty() || rest == "Z" || rest == "z" {
        0
    } else if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) {
        let sign = if rest.starts_with('+') { 1 } else { -1 };
        if rest.as_bytes()[3] != b':' {
            return Err(format!("invalid timezone offset in '{input}'"));
        }
        let tz_h: i64 = rest[1..3]
            .parse()
            .map_err(|_| format!("invalid timezone hour in '{input}'"))?;
        let tz_m: i64 = rest[4..6]
            .parse()
            .map_err(|_| format!("invalid timezone minute in '{input}'"))?;
        if !(0..=23).contains(&tz_h) || !(0..=59).contains(&tz_m) {
            return Err(format!("timezone offset out of range in '{input}'"));
        }
        sign * (tz_h * 3_600 + tz_m * 60)
    } else {
        return Err(format!("unsupported timezone suffix '{rest}' in '{input}'"));
    };

    Ok(base_days * 86_400 + hour * 3_600 + minute * 60 + second - tz_offset_secs)
}

/// Attempts to extract a `YYYY-MM-DD` or `YYYY-MM-DDTHH[-:]MM[-:]SSZ` date from a filename.
fn extract_timestamp_from_filename(file_name: &str) -> Option<i64> {
    let bytes = file_name.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    for start in 0..=bytes.len() - 10 {
        if bytes[start..start + 4].iter().all(|b| b.is_ascii_digit())
            && bytes[start + 4] == b'-'
            && bytes[start + 5..start + 7]
                .iter()
                .all(|b| b.is_ascii_digit())
            && bytes[start + 7] == b'-'
            && bytes[start + 8..start + 10]
                .iter()
                .all(|b| b.is_ascii_digit())
        {
            // Check for full datetime suffix `THH-MM-SSZ` or `THH:MM:SSZ`
            if start + 20 <= bytes.len()
                && (bytes[start + 10] == b'T' || bytes[start + 10] == b't')
                && (bytes[start + 19] == b'Z' || bytes[start + 19] == b'z')
            {
                if let Ok(ts) = parse_iso8601_utc(&file_name[start..start + 20]) {
                    return Some(ts);
                }
            }
            if let Ok(ts) = parse_iso8601_utc(&file_name[start..start + 10]) {
                return Some(ts);
            }
        }
    }
    None
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn validate_calendar_date(year: i64, month: i64, day: i64) -> Result<(), String> {
    if !(1..=12).contains(&month) {
        return Err(format!("month {month} out of range"));
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => unreachable!(),
    };
    if !(1..=max_day).contains(&day) {
        return Err(format!("day {day} out of range for {year:04}-{month:02}"));
    }
    Ok(())
}

/// Howard Hinnant's civil-to-days algorithm (inverse of `civil_date_from_days`).
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant's civil-from-days algorithm, valid for all Unix-era dates.
pub(crate) fn civil_date_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + if month <= 2 { 1 } else { 0 }, month, day)
}

/// Formats Unix seconds as UTC ISO-8601 calendar date `YYYY-MM-DD`.
pub(crate) fn format_date_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_date_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Formats Unix seconds as UTC ISO-8601 timestamp `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn format_timestamp_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let within_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        within_day / 3_600,
        (within_day % 3_600) / 60,
        within_day % 60
    )
}

fn unix_timestamp_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_test_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-trend-unit-{prefix}-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_iso8601_roundtrip_and_calendar_math() {
        assert_eq!(parse_iso8601_utc("1970-01-01").unwrap(), 0);
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_iso8601_utc("2026-01-15T12:30:45Z").unwrap(),
            days_from_civil(2026, 1, 15) * 86_400 + 12 * 3_600 + 30 * 60 + 45
        );
        assert_eq!(
            format_timestamp_utc(parse_iso8601_utc("2026-03-01T08:15:00Z").unwrap()),
            "2026-03-01T08:15:00Z"
        );
        assert_eq!(
            format_date_utc(parse_iso8601_utc("2024-02-29T23:59:59Z").unwrap()),
            "2024-02-29"
        );
        // Timezone offset (+02:00 means UTC is 2 hours earlier)
        assert_eq!(
            parse_iso8601_utc("2026-01-15T14:00:00+02:00").unwrap(),
            parse_iso8601_utc("2026-01-15T12:00:00Z").unwrap()
        );
        // Invalid leap day
        assert!(parse_iso8601_utc("2025-02-29").is_err());
    }

    #[test]
    fn test_extract_timestamp_from_filename() {
        assert_eq!(
            extract_timestamp_from_filename("scan-2026-01-15.json"),
            Some(parse_iso8601_utc("2026-01-15").unwrap())
        );
        assert_eq!(
            extract_timestamp_from_filename("contract_2026-01-15T06-30-00Z.json"),
            Some(parse_iso8601_utc("2026-01-15T06:30:00Z").unwrap())
        );
        assert_eq!(extract_timestamp_from_filename("day1.json"), None);
    }

    #[test]
    fn test_ingest_orders_reports_by_timestamp_and_ledger() {
        let dir = temp_test_dir("order");
        let contract = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

        // Intentionally write files with misleading alphabetical names
        let r3 = json!({
            "contract_id": contract,
            "network": "testnet",
            "latest_ledger": 5_034_560,
            "ledger_velocity_seconds": 5.0,
            "timestamp": "2026-01-03T00:00:00Z",
            "entries": [{ "identifier": "instance", "live_until_ledger": 5_300_000, "remaining_ledgers": 265_440 }]
        });
        let r1 = json!({
            "contract_id": contract,
            "network": "testnet",
            "latest_ledger": 5_000_000,
            "ledger_velocity_seconds": 5.0,
            "timestamp": "2026-01-01T00:00:00Z",
            "entries": [{ "identifier": "instance", "live_until_ledger": 5_300_000, "remaining_ledgers": 300_000 }]
        });
        let r2 = json!({
            "contract_id": contract,
            "network": "testnet",
            "latest_ledger": 5_017_280,
            "ledger_velocity_seconds": 5.0,
            "timestamp": "2026-01-02T00:00:00Z",
            "entries": [{ "identifier": "instance", "live_until_ledger": 5_300_000, "remaining_ledgers": 282_720 }]
        });

        fs::write(dir.join("a_latest.json"), serde_json::to_vec(&r3).unwrap()).unwrap();
        fs::write(dir.join("z_earliest.json"), serde_json::to_vec(&r1).unwrap()).unwrap();
        fs::write(dir.join("m_middle.json"), serde_json::to_vec(&r2).unwrap()).unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let series = ingest_snapshots(&dir, None).unwrap();
        assert_eq!(series.timestamp_source, TimestampSource::Report);
        assert_eq!(series.snapshots.len(), 3);
        assert_eq!(series.snapshots[0].latest_ledger, 5_000_000);
        assert_eq!(series.snapshots[1].latest_ledger, 5_017_280);
        assert_eq!(series.snapshots[2].latest_ledger, 5_034_560);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_ingest_derives_timestamps_from_ledger_velocity_when_omitted() {
        let dir = temp_test_dir("derived-ts");
        let contract = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM";

        for (name, ledger, rem) in [
            ("snap1.json", 5_000_000u32, 300_000i64),
            ("snap2.json", 5_017_280u32, 282_720i64),
            ("snap3.json", 5_034_560u32, 265_440i64),
        ] {
            let report = json!({
                "contract_id": contract,
                "network": "testnet",
                "latest_ledger": ledger,
                "ledger_velocity_seconds": 5.0,
                "entries": [{ "identifier": "instance", "live_until_ledger": 5_300_000, "remaining_ledgers": rem }]
            });
            fs::write(dir.join(name), serde_json::to_vec(&report).unwrap()).unwrap();
        }

        let series = ingest_snapshots(&dir, None).unwrap();
        assert_eq!(series.timestamp_source, TimestampSource::LedgerVelocity);
        assert_eq!(
            series.snapshots[1].timestamp_secs - series.snapshots[0].timestamp_secs,
            86_400
        );
        assert_eq!(
            series.snapshots[2].timestamp_secs - series.snapshots[1].timestamp_secs,
            86_400
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
