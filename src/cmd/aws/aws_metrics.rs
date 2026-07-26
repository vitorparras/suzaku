use crate::core::color::SuzakuColor::Red;
use crate::core::errorlog::log_error;
use crate::core::log_source::LogSource;
use crate::core::scan::{load_aws_events_from_file, process_events_from_dir};
use crate::core::timeline_writer::resolve_output_targets;
use crate::core::util::{
    error_msg, fatal_error, get_writer, output_path_info, p, sanitize_csv_field, upsert_count_entry,
};
use crate::option::cli::{MetricsOptions, OutputFormat};
use crate::option::geoip::GeoIPSearch;
use crate::option::timefiler::filter_by_time;
use comfy_table::{Cell, CellAlignment, Table};
use duckdb::Connection;
use serde::Serialize;
use serde_json::Value;
use sigma_rust::{Event, event_from_json};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Value shown for an event that has no value for the requested field. CloudTrail routinely
/// omits fields (`userIdentity.accessKeyId` is absent for console sessions, `errorCode` for
/// successful calls), and those events are part of the picture: counting them under `-`
/// keeps every field's total equal to the number of events scanned, so the percentages mean
/// "share of all events" rather than "share of the events that happened to have this field".
/// This matches how `aws-ct-summary` renders a missing field.
const NO_VALUE: &str = "-";

/// Prefix of AWS STS temporary access key IDs (as opposed to `AKIA` long-term keys).
const STS_KEY_PREFIX: &str = "ASIA";

// ---------------------------------------------------------------------------
// 集計用内部構造体
// ---------------------------------------------------------------------------

/// `value -> (count, first_seen, last_seen)` for a single field.
type CountMap = HashMap<String, (usize, String, String)>;

/// Per-field counters. Every requested field is tallied in the same pass over the logs, so
/// aggregating five fields costs one scan instead of five.
#[derive(Default)]
struct FieldMetrics {
    counts: CountMap,
    total: usize,
}

struct Metrics {
    fields: Vec<String>,
    per_field: Vec<FieldMetrics>,
    include_sts: bool,
}

impl Metrics {
    fn new(fields: &[String], include_sts: bool) -> Self {
        Metrics {
            fields: fields.to_vec(),
            per_field: fields.iter().map(|_| FieldMetrics::default()).collect(),
            include_sts,
        }
    }

    fn add_event(&mut self, event: &Event) {
        let event_time = match event.get("eventTime") {
            Some(time) => time.value_to_string(),
            None => NO_VALUE.to_string(),
        };
        for (i, field) in self.fields.iter().enumerate() {
            let value = match event.get(field) {
                Some(value) => value.value_to_string(),
                None => NO_VALUE.to_string(),
            };
            // Temporary STS keys are noise for most investigations (a new key per session
            // blows up the value count), so they are dropped unless -s is given. Same rule
            // and same flag as aws-ct-summary.
            if !self.include_sts
                && field.ends_with("accessKeyId")
                && value.starts_with(STS_KEY_PREFIX)
            {
                continue;
            }
            let metrics = &mut self.per_field[i];
            metrics.total += 1;
            upsert_count_entry(&mut metrics.counts, value, &event_time);
        }
    }

    fn is_empty(&self) -> bool {
        self.per_field.iter().all(|m| m.total == 0)
    }

    /// Fields that no event had a value for, i.e. whose only tallied value is `-`. Almost always
    /// a misspelled or mis-cased `-F` (CloudTrail's field is `sourceIPAddress`, not
    /// `sourceIPaddress`), which would otherwise be indistinguishable from real data: counting
    /// missing values as `-` means a typo produces a plausible-looking 100% `-` table.
    fn fields_with_no_values(&self) -> Vec<&str> {
        self.fields
            .iter()
            .zip(self.per_field.iter())
            .filter(|(_, data)| data.counts.keys().all(|value| value == NO_VALUE))
            .map(|(field, _)| field.as_str())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 出力用データ構造
// ---------------------------------------------------------------------------

/// One output row: a single value of a single field. `percent` is that value's share of all
/// events counted for the field.
#[derive(Serialize, Debug, PartialEq)]
pub struct MetricRecord {
    pub field: String,
    pub value: String,
    pub count: usize,
    pub percent: f64,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_asn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_country: Option<String>,
}

impl MetricRecord {
    fn percent_str(&self) -> String {
        format!("{:.2}%", self.percent)
    }

    /// The three GeoIP cells, or `-` when this value could not be enriched. Only called for
    /// the CSV and stdout outputs, which stay rectangular; JSON omits the fields entirely.
    fn geo_cells(&self) -> [String; 3] {
        let cell = |v: &Option<String>| v.clone().unwrap_or_else(|| NO_VALUE.to_string());
        [
            cell(&self.src_asn),
            cell(&self.src_city),
            cell(&self.src_country),
        ]
    }
}

// ---------------------------------------------------------------------------
// パブリックエントリポイント
// ---------------------------------------------------------------------------

pub fn aws_metrics(opt: &MetricsOptions, no_color: bool) {
    let directory = &opt.input_opt.directory;
    let file = &opt.input_opt.filepath;

    let mut geo_search = None;
    if let Some(path) = opt.geo_ip.as_ref() {
        match GeoIPSearch::new(path) {
            Ok(geo) => geo_search = Some(geo),
            Err(_) => {
                p(
                    Red.rdg(no_color),
                    "Could not find the appropriate MaxMind GeoIP .mmdb database files.\n",
                    true,
                );
                return;
            }
        }
    }

    let mut metrics = Metrics::new(&opt.field_names, opt.include_sts);
    let mut stats_func = |json_values: &[Value]| {
        for json_value in json_values {
            if !filter_by_time(&opt.input_opt.time_opt, json_value, "eventTime") {
                continue;
            }
            let event: Event = match event_from_json(json_value.to_string().as_str()) {
                Ok(event) => event,
                Err(_) => continue,
            };
            metrics.add_event(&event);
        }
    };

    if let Some(d) = directory {
        if let Err(e) = process_events_from_dir(
            stats_func,
            d,
            true,
            no_color,
            &LogSource::Aws,
            &opt.input_opt.file_date_opt,
        ) {
            log_error(&format!("Failed to scan directory {}: {e}", d.display()));
        }
    } else if let Some(f) = file {
        match load_aws_events_from_file(f) {
            Ok(events) => stats_func(&events),
            Err(_) => return,
        }
    }

    output_metrics(&metrics, opt, geo_search.as_mut(), no_color);
}

// ---------------------------------------------------------------------------
// レコード生成
// ---------------------------------------------------------------------------

/// Turn the counters into output rows, ordered by count (most frequent first) and GeoIP-enriched
/// when `-G` is given.
fn build_records(metrics: &Metrics, geo_search: Option<&mut GeoIPSearch>) -> Vec<MetricRecord> {
    let mut geo_search = geo_search;
    let mut records = Vec::new();

    for (field, data) in metrics.fields.iter().zip(metrics.per_field.iter()) {
        let mut entries: Vec<(&String, &(usize, String, String))> = data.counts.iter().collect();
        // Ties are broken by value so the output is stable across runs: a HashMap iterates in
        // an arbitrary order, which would otherwise shuffle equal-count rows between runs and
        // make a diff of two result files unreadable.
        entries.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));

        for (value, (count, first, last)) in entries {
            let percent = if data.total == 0 {
                0.0
            } else {
                (*count as f64 / data.total as f64) * 100.0
            };
            let (src_asn, src_city, src_country) = match geo_search.as_mut() {
                // Only values that parse as an IP address are enriched. Anything else — an
                // event name, or the `cloudtrail.amazonaws.com` that CloudTrail writes into
                // sourceIPAddress for AWS-service calls — is reported as `-`.
                Some(geo) => match geo.convert(value.as_str()) {
                    Some(ip) => (
                        Some(geo.get_asn(ip)),
                        Some(geo.get_city(ip)),
                        Some(geo.get_country(ip)),
                    ),
                    None => (
                        Some(NO_VALUE.to_string()),
                        Some(NO_VALUE.to_string()),
                        Some(NO_VALUE.to_string()),
                    ),
                },
                None => (None, None, None),
            };
            records.push(MetricRecord {
                field: field.clone(),
                value: value.clone(),
                count: *count,
                percent: (percent * 100.0).round() / 100.0,
                first_seen: render_time(first),
                last_seen: render_time(last),
                src_asn,
                src_city,
                src_country,
            });
        }
    }
    records
}

/// Render a CloudTrail RFC 3339 timestamp the way the rest of Suzaku does: `2024-01-02 03:04:05`.
fn render_time(time: &str) -> String {
    time.replace('T', " ").replace('Z', "")
}

// ---------------------------------------------------------------------------
// 出力処理
// ---------------------------------------------------------------------------

fn output_metrics(
    metrics: &Metrics,
    opt: &MetricsOptions,
    geo_search: Option<&mut GeoIPSearch>,
    no_color: bool,
) {
    if metrics.is_empty() {
        error_msg(no_color, "No results found.");
        return;
    }

    // The field is spelled correctly (`-F` is validated up front) but these logs do not carry
    // it — `errorCode` in an export with no failed calls, say. Worth saying out loud, because
    // the resulting single `-` row at 100% otherwise reads like a real result.
    for field in metrics.fields_with_no_values() {
        error_msg(
            no_color,
            &format!(
                "No event had a value for the field \"{field}\", so every event is counted as \"-\"."
            ),
        );
    }

    let geo_enabled = geo_search.is_some();

    let Some(output) = opt.output.as_ref() else {
        let records = build_records(metrics, geo_search);
        print_tables(metrics, &records, geo_enabled);
        return;
    };

    // One <base>.<ext> per requested format, deduped — the same resolution the timeline and
    // summary commands use, so -o/-t behave identically across the tool.
    let targets = resolve_output_targets(output, &opt.output_types);
    let path_for = |format: OutputFormat| -> Option<PathBuf> {
        targets
            .iter()
            .find(|(fmt, _)| *fmt == format)
            .map(|(_, path)| path.clone())
    };

    // Checked before the records are built so an existing file fails immediately, without
    // first paying for the GeoIP lookups.
    if !opt.clobber
        && let Some(path) = targets
            .iter()
            .map(|(_, path)| path)
            .find(|path| path.exists())
    {
        error_msg(
            no_color,
            &format!(
                "The file {} already exists. Use --clobber to overwrite.",
                path.display()
            ),
        );
        return;
    }

    let records = build_records(metrics, geo_search);
    let mut output_paths: Vec<PathBuf> = Vec::new();

    if let Some(csv_path) = path_for(OutputFormat::Csv) {
        write_csv(&csv_path, &records, geo_enabled, no_color);
        output_paths.push(csv_path);
    }

    if let Some(json_path) = path_for(OutputFormat::Json) {
        let file = File::create(&json_path).unwrap_or_else(|e| {
            fatal_error(
                no_color,
                &format!("Cannot write to output file {}: {e}", json_path.display()),
            )
        });
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &records).unwrap();
        writer.flush().unwrap();
        output_paths.push(json_path);
    }

    if let Some(jsonl_path) = path_for(OutputFormat::Jsonl) {
        let file = File::create(&jsonl_path).unwrap_or_else(|e| {
            fatal_error(
                no_color,
                &format!("Cannot write to output file {}: {e}", jsonl_path.display()),
            )
        });
        let mut writer = BufWriter::new(file);
        for record in &records {
            writeln!(writer, "{}", serde_json::to_string(record).unwrap()).unwrap();
        }
        writer.flush().unwrap();
        output_paths.push(jsonl_path);
    }

    if let Some(duckdb_path) = path_for(OutputFormat::Duckdb) {
        match write_duckdb_metrics(&duckdb_path, &records) {
            Ok(()) => output_paths.push(duckdb_path),
            Err(e) => fatal_error(no_color, &e),
        }
    }

    output_path_info(no_color, output_paths.as_slice(), true);
}

/// Column names for the CSV output. The stdout tables replace `Field`/`Value` with a single
/// column named after the field itself, since they are printed one table per field.
fn csv_header(geo_enabled: bool) -> Vec<&'static str> {
    let mut header = vec![
        "Field",
        "Value",
        "Count",
        "Percent",
        "FirstSeen",
        "LastSeen",
    ];
    if geo_enabled {
        header.extend(["SrcASN", "SrcCity", "SrcCountry"]);
    }
    header
}

fn write_csv(path: &Path, records: &[MetricRecord], geo_enabled: bool, no_color: bool) {
    let mut wtr =
        get_writer(&Some(path.to_path_buf())).unwrap_or_else(|e| fatal_error(no_color, &e));
    wtr.write_record(csv_header(geo_enabled)).unwrap();
    for record in records {
        let mut row = vec![
            record.field.clone(),
            record.value.clone(),
            record.count.to_string(),
            record.percent_str(),
            record.first_seen.clone(),
            record.last_seen.clone(),
        ];
        if geo_enabled {
            row.extend(record.geo_cells());
        }
        let sanitized: Vec<String> = row.iter().map(|f| sanitize_csv_field(f)).collect();
        wtr.write_record(&sanitized).unwrap();
    }
    wtr.flush().ok();
}

/// Write the metrics to a DuckDB database as a single `metrics` table. Unlike the summary
/// output there is no nesting to preserve here: one row per (field, value) is already the
/// relational shape, so `GROUP BY Field` / `WHERE Value = ...` work directly.
fn write_duckdb_metrics(path: &Path, records: &[MetricRecord]) -> Result<(), String> {
    let conn = Connection::open(path)
        .map_err(|e| format!("Cannot write to output file {}: {e}", path.display()))?;
    conn.execute_batch(
        "CREATE OR REPLACE TABLE metrics (
             Field VARCHAR,
             Value VARCHAR,
             Count BIGINT,
             Percent DOUBLE,
             FirstSeen VARCHAR,
             LastSeen VARCHAR,
             SrcASN VARCHAR,
             SrcCity VARCHAR,
             SrcCountry VARCHAR
         );",
    )
    .map_err(|e| format!("Cannot create DuckDB tables in {}: {e}", path.display()))?;

    let mut app = conn
        .appender("metrics")
        .map_err(|e| format!("Cannot write metrics rows to {}: {e}", path.display()))?;
    for record in records {
        app.append_row(duckdb::params![
            record.field,
            record.value,
            record.count as i64,
            record.percent,
            record.first_seen,
            record.last_seen,
            record.src_asn,
            record.src_city,
            record.src_country,
        ])
        .map_err(|e| format!("Cannot write metrics rows to {}: {e}", path.display()))?;
    }
    app.flush()
        .map_err(|e| format!("Cannot write metrics rows to {}: {e}", path.display()))
}

/// Print one table per field to stdout. The first column is named after the field, which is
/// what the hardcoded `EventName` header was always meant to be.
fn print_tables(metrics: &Metrics, records: &[MetricRecord], geo_enabled: bool) {
    for field in &metrics.fields {
        let mut header = vec![field.as_str(), "Count", "Percent", "FirstSeen", "LastSeen"];
        if geo_enabled {
            header.extend(["SrcASN", "SrcCity", "SrcCountry"]);
        }
        let mut table = Table::new();
        table.set_header(
            header
                .iter()
                .map(|s| Cell::new(s).set_alignment(CellAlignment::Center))
                .collect::<Vec<Cell>>(),
        );
        for record in records.iter().filter(|r| &r.field == field) {
            let mut row = vec![
                record.value.clone(),
                record.count.to_string(),
                record.percent_str(),
                record.first_seen.clone(),
                record.last_seen.clone(),
            ];
            if geo_enabled {
                row.extend(record.geo_cells());
            }
            table.add_row(row.iter().map(Cell::new));
        }
        println!("{table}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics_from(fields: &[&str], events: &[&str], include_sts: bool) -> Metrics {
        let fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
        let mut metrics = Metrics::new(&fields, include_sts);
        for json in events {
            metrics.add_event(&event_from_json(json).unwrap());
        }
        metrics
    }

    fn records_of<'a>(records: &'a [MetricRecord], field: &str) -> Vec<&'a MetricRecord> {
        records.iter().filter(|r| r.field == field).collect()
    }

    const EVENTS: [&str; 3] = [
        r#"{"eventTime":"2024-01-02T00:00:00Z","eventName":"ListBuckets","sourceIPAddress":"1.1.1.1","awsRegion":"us-east-1","userIdentity":{"accessKeyId":"AKIA1"}}"#,
        r#"{"eventTime":"2024-01-01T00:00:00Z","eventName":"ListBuckets","sourceIPAddress":"2.2.2.2","awsRegion":"us-east-1","userIdentity":{"accessKeyId":"ASIA1"}}"#,
        r#"{"eventTime":"2024-01-03T00:00:00Z","eventName":"DeleteTrail","sourceIPAddress":"1.1.1.1","awsRegion":"ap-northeast-1"}"#,
    ];

    #[test]
    fn counts_every_requested_field_in_one_pass() {
        let metrics = metrics_from(
            &["eventName", "sourceIPAddress", "awsRegion"],
            &EVENTS,
            true,
        );
        let records = build_records(&metrics, None);

        let event_names = records_of(&records, "eventName");
        assert_eq!(event_names.len(), 2);
        assert_eq!(event_names[0].value, "ListBuckets");
        assert_eq!(event_names[0].count, 2);

        let ips = records_of(&records, "sourceIPAddress");
        assert_eq!(ips[0].value, "1.1.1.1");
        assert_eq!(ips[0].count, 2);

        let regions = records_of(&records, "awsRegion");
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].value, "us-east-1");
    }

    // A mis-cased field name (CloudTrail's field is `sourceIPAddress`) tallies every event as
    // "-", which looks like real data, so it has to be reported back to the user.
    #[test]
    fn a_field_no_event_has_is_reported() {
        let metrics = metrics_from(&["sourceIPaddress", "eventName"], &EVENTS, true);
        assert_eq!(metrics.fields_with_no_values(), ["sourceIPaddress"]);

        let metrics = metrics_from(&["sourceIPAddress", "eventName"], &EVENTS, true);
        assert!(metrics.fields_with_no_values().is_empty());
    }

    // A field only *some* events have is normal (`userIdentity.accessKeyId` is absent for
    // console sessions) and must not be reported.
    #[test]
    fn a_partially_present_field_is_not_reported() {
        let metrics = metrics_from(&["userIdentity.accessKeyId"], &EVENTS, true);
        assert!(metrics.fields_with_no_values().is_empty());
    }

    // Each value carries the time window in which THAT value was seen, not the dataset's.
    #[test]
    fn tracks_first_and_last_seen_per_value() {
        let metrics = metrics_from(&["sourceIPAddress"], &EVENTS, true);
        let records = build_records(&metrics, None);
        let top = records_of(&records, "sourceIPAddress")[0];
        assert_eq!(top.value, "1.1.1.1");
        assert_eq!(top.first_seen, "2024-01-02 00:00:00");
        assert_eq!(top.last_seen, "2024-01-03 00:00:00");
    }

    // An event missing the field is counted as "-" so percentages stay a share of all events.
    #[test]
    fn missing_field_is_counted_as_no_value() {
        let metrics = metrics_from(&["userIdentity.accessKeyId"], &EVENTS, true);
        let records = build_records(&metrics, None);
        let rows = records_of(&records, "userIdentity.accessKeyId");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.iter().map(|r| r.count).sum::<usize>(), EVENTS.len());
        assert!(rows.iter().any(|r| r.value == NO_VALUE && r.count == 1));
        for row in rows {
            assert!((row.percent - 33.33).abs() < 0.01, "{}", row.percent);
        }
    }

    #[test]
    fn sts_keys_are_excluded_unless_requested() {
        let without = metrics_from(&["userIdentity.accessKeyId"], &EVENTS, false);
        let records = build_records(&without, None);
        let rows = records_of(&records, "userIdentity.accessKeyId");
        assert!(!rows.iter().any(|r| r.value.starts_with(STS_KEY_PREFIX)));
        // The dropped event is not counted at all, so the remaining two share 100%.
        assert_eq!(rows.iter().map(|r| r.count).sum::<usize>(), 2);

        let with = metrics_from(&["userIdentity.accessKeyId"], &EVENTS, true);
        let records = build_records(&with, None);
        assert!(
            records_of(&records, "userIdentity.accessKeyId")
                .iter()
                .any(|r| r.value == "ASIA1")
        );
    }

    #[test]
    fn values_are_ordered_by_count_within_each_field() {
        let metrics = metrics_from(&["eventName", "sourceIPAddress"], &EVENTS, true);
        let records = build_records(&metrics, None);

        for field in ["eventName", "sourceIPAddress"] {
            let counts: Vec<usize> = records_of(&records, field)
                .iter()
                .map(|r| r.count)
                .collect();
            assert!(
                counts.windows(2).all(|w| w[0] >= w[1]),
                "{field}: {counts:?}"
            );
        }
        assert!((records_of(&records, "eventName")[0].percent - 66.67).abs() < 0.01);
    }

    #[test]
    fn geoip_enriches_ip_values_and_leaves_others_blank() {
        // Small GeoLite2 test databases shipped under test_files/mmdb/.
        let mut geo = GeoIPSearch::new(Path::new("test_files/mmdb"))
            .expect("GeoLite2 test .mmdb files must be present under test_files/mmdb/");
        let events = [
            // 81.2.69.142 is one of the addresses the GeoLite2 test databases resolve.
            r#"{"eventTime":"2024-01-01T00:00:00Z","sourceIPAddress":"81.2.69.142"}"#,
            r#"{"eventTime":"2024-01-01T00:00:00Z","sourceIPAddress":"cloudtrail.amazonaws.com"}"#,
        ];
        let metrics = metrics_from(&["sourceIPAddress"], &events, true);
        let records = build_records(&metrics, Some(&mut geo));

        let service = records
            .iter()
            .find(|r| r.value == "cloudtrail.amazonaws.com")
            .unwrap();
        assert_eq!(service.geo_cells(), [NO_VALUE, NO_VALUE, NO_VALUE]);

        let enriched = records.iter().find(|r| r.value == "81.2.69.142").unwrap();
        assert_eq!(enriched.src_city.as_deref(), Some("London"));
        assert_eq!(enriched.src_country.as_deref(), Some("United Kingdom"));
    }

    // Without -G the GeoIP columns are absent entirely (and omitted from JSON).
    #[test]
    fn no_geoip_columns_without_the_flag() {
        let metrics = metrics_from(&["sourceIPAddress"], &EVENTS, true);
        let records = build_records(&metrics, None);
        assert!(records.iter().all(|r| r.src_asn.is_none()));
        let json = serde_json::to_string(&records[0]).unwrap();
        assert!(!json.contains("src_asn"), "{json}");
    }

    #[test]
    fn csv_header_matches_the_geoip_setting() {
        assert_eq!(csv_header(false).len(), 6);
        assert_eq!(csv_header(true).len(), 9);
        assert_eq!(csv_header(true)[8], "SrcCountry");
    }

    #[test]
    fn equal_counts_sort_by_value_for_stable_output() {
        let events = [
            r#"{"eventTime":"2024-01-01T00:00:00Z","awsRegion":"us-east-1"}"#,
            r#"{"eventTime":"2024-01-01T00:00:00Z","awsRegion":"ap-northeast-1"}"#,
            r#"{"eventTime":"2024-01-01T00:00:00Z","awsRegion":"eu-west-1"}"#,
        ];
        let metrics = metrics_from(&["awsRegion"], &events, true);
        let values: Vec<String> = build_records(&metrics, None)
            .iter()
            .map(|r| r.value.clone())
            .collect();
        assert_eq!(values, ["ap-northeast-1", "eu-west-1", "us-east-1"]);
    }
}
