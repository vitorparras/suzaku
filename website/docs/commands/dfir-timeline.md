# DFIR Timeline Commands

## `aws-ct-timeline` command

Create an AWS CloudTrail DFIR timeline based on Sigma rules in the `rules` folder.

## Command usage
```
Usage: suzaku aws-ct-timeline [OPTIONS] <--directory <DIR>|--file <FILE>>

General Options:
  -r, --rules <DIR/FILE>  Specify a custom rule directory or file (default: ./rules)
  -h, --help              Show the help menu

Input:
  -d, --directory <DIR>  Directory of multiple gz/json/parquet files
  -f, --file <FILE>      File path to one gz/json/parquet file

Filtering:
      --timeline-start <DATE>  Start time of the events to load (ex: "2022-02-22T23:59:59Z)
      --timeline-end <DATE>    End time of the events to load (ex: "2020-02-22T00:00:00Z")
      --time-offset <OFFSET>   Scan recent events based on an offset (ex: 1y, 3M, 30d, 24h, 30m)

Output:
  -C, --clobber                    Overwrite files when saving
  -G, --geo-ip <MAXMIND-DB-DIR>    Add GeoIP (ASN, city, country) info to IP addresses
  -m, --min-level <LEVEL>          Minimum level for rules to load (default: informational)
  -o, --output <FILE>              Save the results to a file
  -t, --output-type <FORMAT,...>   Output format(s) (only used with -o): csv (default), json, jsonl, duckdb. Comma-separate or repeat to write several at once, e.g. -t csv,duckdb [possible values: csv, json, jsonl, duckdb]
  -R, --raw-output                 Output the original JSON logs (only available in JSON formats or stdout)
      --threads <THREAD NUMBER>    Number of threads to use (default: same as CPU cores)

Display Settings:
  -K, --no-color               Disable color output
  -N, --no-summary             Do not display results summary
  -T, --no-frequency-timeline  Disable event frequency timeline (terminal needs to support Unicode)
  -q, --quiet                  Quiet mode: do not display the launch banner
```

### `aws-ct-timeline` command examples

* Output alerts to screen: `./suzaku aws-ct-timeline -d ../suzaku-sample-data`
* Save results to a CSV file: `./suzaku aws-ct-timeline -d ../suzaku-sample-data -o sample-timeline.csv`
* Save results to CSV and JSONL files: `./suzaku aws-ct-timeline -d ../suzaku-sample-data -o sample-timeline -t csv,jsonl`
* Save results to a DuckDB database: `./suzaku aws-ct-timeline -d ../suzaku-sample-data -o sample-timeline -t duckdb`

### `aws-ct-timeline` output profile

Suzaku will output information based on the `config/aws_profile.yaml` file:
```yaml
Timestamp: '.eventTime'
RuleTitle: 'sigma.title'
RuleAuthor: 'sigma.author'
Level: 'sigma.level'
EventName: '.eventName'
ErrorCode: '.errorCode'
ErrorMessage: '.errorMessage'
EventSource: '.eventSource'
AWS-Region: '.awsRegion'
SrcIP: '.sourceIPAddress'
UserAgent: '.userAgent'
UserName: '.userIdentity.userName'
UserType: '.userIdentity.type'
UserAccountID: '.userIdentity.accountId'
UserARN: '.userIdentity.arn'
UserPrincipalID: '.userIdentity.principalId'
UserAccessKeyID: '.userIdentity.accessKeyId'
EventID: '.eventID'
Tags: 'sigma.tags'
RuleID: 'sigma.id'
```

* Any field value that starts with `.` (ex: `.eventTime`) will be taken from the CloudTrail log.
* Any field value that starts with `sigma.` (ex: `sigma.title`) will be taken from the Sigma rule.
* Currently we only support strings but plan on supporting other types of field values.

> Note: If you want to output the original JSON data and make sure you do not loose any field information, just add the `-R, --raw-output` option to `aws-ct-timeline` command.

### DuckDB output schema

The CSV and JSON outputs are a *rendering* of the profile above; the DuckDB output is a *data
interface*, so it is typed and self-describing instead. The differences are deliberate and apply
to `aws-ct-timeline`, `azure-timeline` and `aws-ct-search`:

| | CSV / JSON | DuckDB |
|---|---|---|
| A missing value | `-` (or empty) | `NULL` |
| `Timestamp` | rendered text | `TIMESTAMP` |
| `Level` | text | `suzaku_level`, an `ENUM` ordered by severity |
| `AWS-Region` | `AWS-Region` | `AwsRegion` (no quoting needed in SQL) |
| `Tags` | one ` ¦ `-joined string | `Tactics`, `TechniqueIDs`, `OtherTags`, each a `VARCHAR[]` |
| `SrcASN` / `SrcCity` / `SrcCountry` | added only under `-G, --geo-ip` | always present (when the profile has `SrcIP`), `NULL` when `-G` was not used |
| Duplicate rows | kept | exact duplicates removed, count reported in `suzaku_meta` |

Every file also carries a one-row `suzaku_meta` table so a reader can tell what produced it
without guessing:

| Column | Meaning |
|---|---|
| `schema_version` | Layout version. Check this before reading the other tables. |
| `suzaku_version`, `command`, `command_line` | Which Suzaku, which subcommand, which exact invocation. |
| `generated_at` | When the file was written. |
| `timestamp_tz` | The zone the `Timestamp` column is expressed in — `UTC`, or the local offset under `-l, --localtime`. |
| `rules_version`, `rules_count` | Ruleset revision (when the rules folder is a git checkout) and how many rules were loaded. |
| `geoip_enabled` | Whether `-G, --geo-ip` ran. Tells an all-`NULL` `SrcCountry` ("enrichment was off") apart from a `NULL` cell in an enriched file ("this value is not an IP address"). |
| `scanned_files`, `scanned_events` | Coverage of the run. |
| `output_rows`, `duplicate_rows_removed` | Rows written, and exact duplicates dropped on write. |

A `timeline` row is one **event × rule match**: an event matching several rules produces one row
per match, so `EventID` is *not* unique. That grain is also recorded as a table comment
(`SELECT comment FROM duckdb_tables()`).

```sql
-- Critical and high alerts in a time range, with their ATT&CK techniques.
-- `Level` is an ENUM, so cast the literal to compare by severity rather than alphabetically.
SELECT Timestamp, RuleTitle, EventName, SrcIP, TechniqueIDs
FROM timeline
WHERE Level >= 'high'::suzaku_level
  AND Timestamp BETWEEN TIMESTAMP '2024-01-01' AND TIMESTAMP '2024-02-01'
  AND ErrorCode IS NULL          -- the call succeeded
ORDER BY Timestamp;

-- ATT&CK technique coverage, no string parsing required
SELECT technique, count(*) AS hits
FROM (SELECT unnest(TechniqueIDs) AS technique FROM timeline)
GROUP BY 1 ORDER BY hits DESC;
```

The database is checkpointed before Suzaku exits, so the `.duckdb` file is complete and can be
opened read-only (copy it after the command finishes, not while it runs).
