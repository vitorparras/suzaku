//! Shared pieces of Suzaku's DuckDB (`-t duckdb`) output.
//!
//! The `.duckdb` file is a *data* interface: it is queried by BI tools, notebooks and
//! LLM-generated SQL, none of which can see Suzaku's rendering conventions. The CSV/JSON writers
//! stay exactly as they are — placeholders, packed strings and rendered timestamps are right for
//! a spreadsheet — while everything written here is typed, NULL-able and self-describing:
//!
//! * `suzaku_meta` records which Suzaku, which command, which ruleset and which timezone produced
//!   the file, so a consumer can look that up instead of guessing it from the table names.
//! * `-` and `''` become `NULL`, so `IS NULL` is the right question to ask.
//! * timestamps are `TIMESTAMP`, severities are an ordered `ENUM`, multi-value fields are
//!   `VARCHAR[]`.
//!
//! The helpers below are the single source of truth for those conventions, shared by the timeline
//! sink (`core::timeline_writer`) and the summary writer (`cmd::aws::aws_summary`).

use crate::option::cli::VERSION;
use chrono::Local;
use duckdb::{Connection, params};
use std::path::Path;

/// Version of the DuckDB layout Suzaku writes, published as `suzaku_meta.schema_version`.
///
/// Bump this on any change a consumer must adapt to (a renamed, retyped or dropped column). A
/// consumer that checks it can refuse a file it does not understand instead of silently
/// mis-visualizing it, which is the whole reason the metadata table exists.
pub const SCHEMA_VERSION: i32 = 1;

/// Sigma severity as an ordered `ENUM`, least to most severe. Ordering lives in the type, so
/// `ORDER BY Level DESC` and `max(Level)` replace the `CASE WHEN Level = 'critical' THEN 5 ...`
/// rank every consumer would otherwise hand-write, and an unknown severity becomes a visible
/// `NULL` rather than a silent new category.
pub const LEVEL_ENUM: &str = "ENUM('informational','low','medium','high','critical')";

/// The severity ENUM is registered as a *named* type in every database that has a severity
/// column. DuckDB compares an ENUM against a bare string literal as text — so `Level >= 'high'`
/// would silently mean the alphabetical `'high' <= 'informational'` — but `Level >= 'high'
/// ::suzaku_level` compares by severity. Naming the type is what makes that threshold filter
/// writable at all.
pub const LEVEL_TYPE: &str = "suzaku_level";

/// Register [`LEVEL_TYPE`]. `IF NOT EXISTS` because `--clobber` rewrites the tables of an
/// existing database, where the type is already defined (and still referenced by them).
pub fn create_level_type(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(&format!(
        "CREATE TYPE IF NOT EXISTS {LEVEL_TYPE} AS {LEVEL_ENUM};"
    ))
    .map_err(|e| format!("Cannot create the {LEVEL_TYPE} type: {e}"))
}

/// Whether an API call succeeded, as an `ENUM`. One half of the old `summary_api_calls.Category`
/// string (`abused_success` / `other_failed` / ...); the other half is the `IsAbused` boolean.
pub const OUTCOME_ENUM: &str = "ENUM('success','failed')";

/// Separator Suzaku's text writers use to pack a multi-value field (rule tags, correlation
/// values) into one column. In DuckDB those columns are split back into `VARCHAR[]`.
pub const MULTI_VALUE_SEPARATOR: &str = " ¦ ";

/// The MaxMind enrichment of a source IP, in the order every writer emits it.
///
/// The text outputs add these columns only under `-G`, which is right for a spreadsheet. The
/// DuckDB output always has them: a column that appears and disappears with a run-time flag makes
/// the *same* query a binder error on half the files, and every consumer — dashboards, notebooks,
/// generated SQL — pays for that. All-NULL is a value; a missing column is a broken query.
pub const GEO_COLUMNS: [&str; 3] = ["SrcASN", "SrcCity", "SrcCountry"];

/// Why a geo cell is NULL — the ambiguity that made a conditional column look attractive in the
/// first place, resolved by pointing at the flag in `suzaku_meta` rather than by dropping the
/// column.
pub const GEO_COLUMN_COMMENT: &str = "MaxMind GeoIP enrichment of the source IP. NULL when -G/--geo-ip was not used \
     (see suzaku_meta.geoip_enabled) or when the value is not a parseable IP address.";

/// Quote an identifier for use in generated SQL. Column names come from the output profile YAML,
/// so they are not statically known and must be escaped rather than trusted.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a string literal for use in generated SQL.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// SQL mapping Suzaku's two text placeholders — `-` (no value) and `''` — onto `NULL`.
///
/// Both appear in the same column today (`UserAccessKeyID` has 49 empty strings alongside its
/// dashes), so a consumer that handles only one of them is wrong for the other. In DuckDB there is
/// exactly one way to say "absent", and it is the one `IS NULL`, `count(DISTINCT ...)` and every
/// BI filter already understand.
pub fn nullable(expr: &str) -> String {
    format!("nullif(nullif({expr}, '-'), '')")
}

/// SQL casting Suzaku's rendered timestamp text to a real `TIMESTAMP`.
///
/// `TRY_CAST` rather than `CAST` so one unparseable value yields a `NULL` row instead of failing
/// the whole write. The wall-clock reading is preserved as rendered; which zone that is stated in
/// `suzaku_meta.timestamp_tz`.
pub fn timestamp_expr(expr: &str) -> String {
    format!("TRY_CAST({} AS TIMESTAMP)", nullable(expr))
}

/// SQL splitting a [`MULTI_VALUE_SEPARATOR`]-joined string into a `VARCHAR[]`. An absent value
/// becomes an empty list rather than `NULL`, so `unnest`/`list_contains` need no guard.
pub fn list_expr(expr: &str) -> String {
    let inner = nullable(expr);
    format!(
        "CASE WHEN {inner} IS NULL THEN []::VARCHAR[] ELSE string_split({inner}, {sep}) END",
        sep = quote_literal(MULTI_VALUE_SEPARATOR)
    )
}

/// Provenance for one DuckDB output file, written to the `suzaku_meta` table.
///
/// Everything here is knowable to the writer and unknowable to the reader. Without it a consumer
/// has to infer the producing command from a table-name signature, an incident report cannot cite
/// the ruleset that produced a detection, and the timezone of every timestamp is a guess.
#[derive(Debug, Clone)]
pub struct SuzakuMeta {
    /// The subcommand that produced the file, e.g. `aws-ct-timeline`.
    pub command: &'static str,
    /// Full invocation, for reproducing the run from the evidence file alone.
    pub command_line: String,
    /// Timezone the `Timestamp` / `*Seen` columns are expressed in (`UTC`, or the local offset
    /// under `--localtime`). The values themselves carry no offset, so without this they cannot
    /// be correlated with evidence from another timezone.
    pub timestamp_tz: String,
    /// Revision of the ruleset used, when the rules directory is a git checkout.
    pub rules_version: Option<String>,
    pub rules_count: Option<i64>,
    /// Whether `-G, --geo-ip` ran. The geo columns exist either way, so this is what tells an
    /// all-NULL `SrcCountry` ("enrichment was off") apart from a NULL cell in an enriched file
    /// ("this value is not an IP address").
    pub geoip_enabled: bool,
    pub scanned_files: Option<i64>,
    pub scanned_events: Option<i64>,
    /// Rows in the main table after deduplication.
    pub output_rows: Option<i64>,
    /// Exact-duplicate rows dropped on write; see the timeline sink for why they occur.
    pub duplicate_rows_removed: Option<i64>,
}

impl SuzakuMeta {
    pub fn new(command: &'static str) -> Self {
        Self {
            command,
            command_line: current_command_line(),
            timestamp_tz: "UTC".to_string(),
            rules_version: None,
            rules_count: None,
            geoip_enabled: false,
            scanned_files: None,
            scanned_events: None,
            output_rows: None,
            duplicate_rows_removed: None,
        }
    }

    /// Record which zone the timestamp columns are rendered in. Suzaku writes UTC by default and
    /// the machine's local time under `--localtime`.
    pub fn with_localtime(mut self, localtime: bool) -> Self {
        self.timestamp_tz = if localtime {
            Local::now().offset().to_string()
        } else {
            "UTC".to_string()
        };
        self
    }

    /// Record whether the run enriched source IPs with MaxMind data.
    pub fn with_geoip(mut self, geoip_enabled: bool) -> Self {
        self.geoip_enabled = geoip_enabled;
        self
    }

    /// Record the ruleset that produced the detections.
    pub fn with_rules(mut self, rules_path: &Path, rules_count: usize) -> Self {
        self.rules_version = rules_revision(rules_path);
        self.rules_count = Some(rules_count as i64);
        self
    }
}

/// The command line this process was started with, used for DFIR provenance.
fn current_command_line() -> String {
    std::env::args().collect::<Vec<_>>().join(" ")
}

/// Short commit id of the rules checkout, when `rules_path` is a git working directory (which is
/// how `update-rules` creates it). `None` for a hand-assembled rules folder or a single rule file.
fn rules_revision(rules_path: &Path) -> Option<String> {
    // `open` rather than `discover`: discovery would walk up out of a non-repo rules folder and
    // report the *suzaku* repository's revision, which is a different fact entirely.
    let repo = git2::Repository::open(rules_path).ok()?;
    let id = repo.head().ok()?.peel_to_commit().ok()?.id().to_string();
    Some(id.chars().take(12).collect())
}

/// Create and populate the single-row `suzaku_meta` table.
pub fn write_meta(conn: &Connection, meta: &SuzakuMeta) -> Result<(), String> {
    conn.execute_batch(
        "CREATE OR REPLACE TABLE suzaku_meta (
             schema_version         INTEGER NOT NULL,
             suzaku_version         VARCHAR NOT NULL,
             command                VARCHAR NOT NULL,
             command_line           VARCHAR,
             generated_at           TIMESTAMP WITH TIME ZONE NOT NULL,
             timestamp_tz           VARCHAR,
             rules_version          VARCHAR,
             rules_count            BIGINT,
             geoip_enabled          BOOLEAN NOT NULL,
             scanned_files          BIGINT,
             scanned_events         BIGINT,
             output_rows            BIGINT,
             duplicate_rows_removed BIGINT
         );",
    )
    .map_err(|e| format!("Cannot create the suzaku_meta table: {e}"))?;
    conn.execute(
        "INSERT INTO suzaku_meta VALUES (?, ?, ?, ?, now(), ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            SCHEMA_VERSION,
            VERSION,
            meta.command,
            meta.command_line,
            meta.timestamp_tz,
            meta.rules_version,
            meta.rules_count,
            meta.geoip_enabled,
            meta.scanned_files,
            meta.scanned_events,
            meta.output_rows,
            meta.duplicate_rows_removed,
        ],
    )
    .map_err(|e| format!("Cannot write the suzaku_meta row: {e}"))?;
    comment_on_table(
        conn,
        "suzaku_meta",
        "Provenance of this file: which Suzaku version, command, ruleset and timezone produced it. \
         Exactly one row. Consumers should check schema_version before reading the other tables.",
    )
}

/// Attach a `COMMENT` to a table. Comments are how the row grain is documented in the file itself
/// (`duckdb_tables()`), instead of only in prose a consumer may never read.
pub fn comment_on_table(conn: &Connection, table: &str, comment: &str) -> Result<(), String> {
    conn.execute_batch(&format!(
        "COMMENT ON TABLE {} IS {};",
        quote_ident(table),
        quote_literal(comment)
    ))
    .map_err(|e| format!("Cannot comment on table {table}: {e}"))
}

/// Attach a `COMMENT` to a column, readable via `duckdb_columns()`.
pub fn comment_on_column(
    conn: &Connection,
    table: &str,
    column: &str,
    comment: &str,
) -> Result<(), String> {
    conn.execute_batch(&format!(
        "COMMENT ON COLUMN {}.{} IS {};",
        quote_ident(table),
        quote_ident(column),
        quote_literal(comment)
    ))
    .map_err(|e| format!("Cannot comment on column {table}.{column}: {e}"))
}

/// Force a checkpoint so the database file is complete and self-contained when Suzaku exits.
///
/// A `.duckdb` left with an un-replayed `<file>.wal` cannot be opened from a read-only mount,
/// which is how dashboards and containers typically attach the evidence file. Checkpointing on
/// the way out means "copy the file after Suzaku finishes" is all a user has to know.
pub fn checkpoint(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("CHECKPOINT;")
        .map_err(|e| format!("Cannot checkpoint the DuckDB database: {e}"))
}

/// Create `table` from `ddl` and fill it with `rows`, converting each staged string column with
/// the matching expression in `columns`.
///
/// Suzaku assembles every value as a string, but the appender API can only bind scalars — it
/// cannot produce a `TIMESTAMP` from text without failing the whole write on one bad value, and
/// it cannot produce a `LIST` at all. So rows are appended verbatim to a `TEMP` staging table and
/// converted by one `INSERT ... SELECT`, which is also what lets the real table keep the
/// `NOT NULL` constraints its `ddl` declares. The staging table is `TEMP`, so none of the raw
/// text ends up in the finished database file.
///
/// `columns` is `(name, expression)` in the target table's column order; `name` is both the
/// staging column and the target column, and `rows` supply the staging values in that order.
pub fn stage_and_type(
    conn: &Connection,
    table: &str,
    ddl: &str,
    columns: &[(&str, String)],
    rows: &[Vec<String>],
) -> Result<(), String> {
    let staging = format!("suzaku_stage_{table}");
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TABLE {} ({ddl});",
        quote_ident(table)
    ))
    .map_err(|e| format!("Cannot create the {table} table: {e}"))?;
    let staging_ddl = columns
        .iter()
        .map(|(name, _)| format!("{} VARCHAR", quote_ident(name)))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TEMP TABLE {} ({staging_ddl});",
        quote_ident(&staging)
    ))
    .map_err(|e| format!("Cannot stage rows for {table}: {e}"))?;

    {
        let mut appender = conn
            .appender_to_catalog_and_db(&staging, "temp", "main")
            .map_err(|e| format!("Cannot stage rows for {table}: {e}"))?;
        for row in rows {
            let params: Vec<&dyn duckdb::ToSql> =
                row.iter().map(|v| v as &dyn duckdb::ToSql).collect();
            appender
                .append_row(params.as_slice())
                .map_err(|e| format!("Cannot stage rows for {table}: {e}"))?;
        }
        appender
            .flush()
            .map_err(|e| format!("Cannot stage rows for {table}: {e}"))?;
    }

    let select = columns
        .iter()
        .map(|(name, expr)| format!("{expr} AS {}", quote_ident(name)))
        .collect::<Vec<_>>()
        .join(",\n       ");
    conn.execute_batch(&format!(
        "INSERT INTO {} SELECT {select} FROM {};",
        quote_ident(table),
        quote_ident(&staging)
    ))
    .map_err(|e| format!("Cannot write rows to {table}: {e}"))?;
    conn.execute_batch(&format!("DROP TABLE {};", quote_ident(&staging)))
        .map_err(|e| format!("Cannot drop the staging table for {table}: {e}"))
}

/// Number of rows in `table`.
pub fn count_rows(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(
        &format!("SELECT count(*) FROM {}", quote_ident(table)),
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("Cannot count rows in {table}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(conn: &Connection, sql: &str) -> String {
        conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap()
    }

    #[test]
    fn quoting_escapes_embedded_delimiters() {
        assert_eq!(quote_ident("AWS-Region"), "\"AWS-Region\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn nullable_maps_both_placeholders_to_null() {
        let conn = Connection::open_in_memory().unwrap();
        // Both sentinels appear in the same column in real output, so both must map to NULL.
        for value in ["'-'", "''"] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM (SELECT {})", nullable(value)),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1);
            let is_null: bool = conn
                .query_row(&format!("SELECT {} IS NULL", nullable(value)), [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(is_null, "{value} should be NULL");
        }
        // A real value is untouched.
        assert_eq!(
            scalar(&conn, &format!("SELECT {}", nullable("'AccessDenied'"))),
            "AccessDenied"
        );
    }

    #[test]
    fn timestamp_expr_types_values_and_tolerates_garbage() {
        let conn = Connection::open_in_memory().unwrap();
        let t = scalar(
            &conn,
            &format!(
                "SELECT strftime({}, '%Y-%m-%d %H:%M:%S')",
                timestamp_expr("'2019-10-16 16:37:01'")
            ),
        );
        assert_eq!(t, "2019-10-16 16:37:01");
        // An unparseable value must not fail the whole write.
        let is_null: bool = conn
            .query_row(
                &format!("SELECT {} IS NULL", timestamp_expr("'not a time'")),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(is_null);
    }

    #[test]
    fn list_expr_splits_and_empties() {
        let conn = Connection::open_in_memory().unwrap();
        let joined = format!("'PrivEsc{MULTI_VALUE_SEPARATOR}InitAccess'");
        assert_eq!(
            scalar(
                &conn,
                &format!(
                    "SELECT list_aggregate({}, 'string_agg', ',')",
                    list_expr(&joined)
                )
            ),
            "PrivEsc,InitAccess"
        );
        // A placeholder becomes an empty list, never a one-element list holding "-".
        let len: i64 = conn
            .query_row(&format!("SELECT len({})", list_expr("'-'")), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(len, 0);
    }

    #[test]
    fn level_enum_orders_by_severity_not_alphabetically() {
        let conn = Connection::open_in_memory().unwrap();
        // 'critical' < 'low' alphabetically; the ENUM must rank it highest instead.
        let top = scalar(
            &conn,
            &format!(
                "SELECT max(CAST(l AS {LEVEL_ENUM}))::VARCHAR FROM (VALUES ('low'), ('critical'), ('medium')) t(l)"
            ),
        );
        assert_eq!(top, "critical");
    }

    #[test]
    fn meta_table_is_single_row_and_self_describing() {
        let conn = Connection::open_in_memory().unwrap();
        let mut meta = SuzakuMeta::new("aws-ct-timeline")
            .with_localtime(false)
            .with_geoip(true);
        meta.output_rows = Some(42);
        write_meta(&conn, &meta).unwrap();

        let (version, command, tz, geoip, rows): (i32, String, String, bool, i64) = conn
            .query_row(
                "SELECT schema_version, command, timestamp_tz, geoip_enabled, output_rows
                 FROM suzaku_meta",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(command, "aws-ct-timeline");
        assert_eq!(tz, "UTC");
        // The geo columns are unconditional, so this flag is the only thing that says whether an
        // all-NULL SrcCountry means "enrichment was off" or "nothing resolved".
        assert!(geoip);
        assert_eq!(rows, 42);
        assert_eq!(count_rows(&conn, "suzaku_meta").unwrap(), 1);
        // The table documents itself, so `duckdb_tables()` answers "what is this file?".
        let comment = scalar(
            &conn,
            "SELECT comment FROM duckdb_tables() WHERE table_name = 'suzaku_meta'",
        );
        assert!(comment.contains("schema_version"), "got: {comment}");
    }

    #[test]
    fn localtime_meta_records_the_offset_not_utc() {
        let meta = SuzakuMeta::new("aws-ct-timeline").with_localtime(true);
        // The exact offset depends on the test machine; what matters is that UTC is not claimed
        // unless the machine really is on UTC.
        let expected = Local::now().offset().to_string();
        assert_eq!(meta.timestamp_tz, expected);
    }
}
