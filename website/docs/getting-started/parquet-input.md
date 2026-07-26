# Converting Logs to Parquet

Suzaku's AWS commands (`aws-ct-timeline`, `aws-ct-metrics`, `aws-ct-summary`, `aws-ct-search`) read `.parquet` files directly, with both `-f/--file` and `-d/--directory`. Parquet is columnar and compressed, so a large CloudTrail export is far smaller on disk and faster to scan than the equivalent JSON.

This page explains the Parquet layout Suzaku expects and gives tested recipes for converting CloudTrail logs to it.

## What Suzaku expects

Suzaku turns each Parquet **row** into one CloudTrail event, so the file must be one event per row, with the CloudTrail fields as top-level columns.

Two things matter for detection to work:

1. **Field names must keep their original CloudTrail casing** (`eventName`, `eventSource`, `userIdentity`, `sourceIPAddress`, …). Sigma rules match these exact names. Any conversion that lowercases column names (e.g. `eventname`) will load the events but match **no rules**. See the warning below.
2. **Nested fields** (`userIdentity`, `requestParameters`, `responseElements`, `additionalEventData`, `serviceEventDetails`, `resources`, `tlsDetails`, `insightDetails`) may be stored either as real nested struct columns **or** as JSON-encoded strings. Suzaku handles both: struct columns are read as nested objects, and these known envelope fields stored as JSON strings — the shape Athena/Glue/Firehose pipelines often produce — are parsed back into objects so rules can match nested values like `requestParameters.bucketName`.

`eventTime` may be a string or a Parquet `TIMESTAMP` column. A `TIMESTAMP` with no timezone is treated as UTC (correct for CloudTrail), so time filtering (`--timeline-start/--timeline-end`) and the date summaries keep working.

Supported compression codecs: **snappy, gzip, zstd, lz4** (and uncompressed).

!!! warning "Preserve the field-name casing"
    The safest conversion keeps each event's top-level JSON keys as columns, which preserves their casing. Routing the data through a nested struct type first tends to **lowercase** field names. Concretely:

    - DuckDB `read_json_auto()` over **JSONL (one event per line)** → keys become columns, casing preserved. ✅
    - DuckDB `unnest(Records)` over a `{"Records":[…]}` object → goes through a struct, field names come out lowercased. ❌
    - AWS **Athena `CREATE TABLE AS`** likewise lowercases column names. ❌

    If your Parquet has lowercased fields (`eventname`, `useridentity`, …), Suzaku will report events scanned but zero detections. Flatten to one-event-per-line JSONL first (see below) and convert that.

## Recipe 1 — DuckDB (recommended)

[DuckDB](https://duckdb.org/) is a single self-contained binary and infers the schema across all rows automatically (so no column is dropped if the first event happens to lack a field).

If your logs are already **JSONL** (one JSON event per line):

```bash
duckdb -c "COPY (SELECT * FROM read_json_auto('events.jsonl'))
           TO 'events.parquet' (FORMAT PARQUET, COMPRESSION ZSTD);"
```

If your logs are the standard CloudTrail delivery shape `{"Records":[ … ]}` (one or many `.json` files), flatten them to one-event-per-line JSONL first with `jq`, then convert. This preserves field-name casing:

```bash
jq -c '.Records[]' cloudtrail.json > events.jsonl
duckdb -c "COPY (SELECT * FROM read_json_auto('events.jsonl'))
           TO 'events.parquet' (FORMAT PARQUET, COMPRESSION ZSTD);"
```

To convert a whole directory of gzipped CloudTrail objects at once:

```bash
zcat AWSLogs/**/*.json.gz | jq -c '.Records[]' > events.jsonl
duckdb -c "COPY (SELECT * FROM read_json_auto('events.jsonl'))
           TO 'events.parquet' (FORMAT PARQUET, COMPRESSION ZSTD);"
```

## Recipe 2 — Python (pyarrow)

```python
import json
import pyarrow as pa
import pyarrow.parquet as pq

# One event per line (JSONL). For a {"Records":[...]} file, load it and use data["Records"].
rows = [json.loads(line) for line in open("events.jsonl")]

# Union the keys across all events so schema inference doesn't drop a column
# that is missing from the first row.
keys = []
for r in rows:
    for k in r:
        if k not in keys:
            keys.append(k)
rows = [{k: r.get(k) for k in keys} for r in rows]

pq.write_table(pa.Table.from_pylist(rows), "events.parquet", compression="zstd")
```

pyarrow serializes nested `dict`/`list` values as real Parquet struct/list columns, which Suzaku reads natively.

## Recipe 3 — AWS Athena / Glue

Athena and Glue write Parquet whose envelope fields are JSON strings and whose `eventTime` is a naive `TIMESTAMP` — Suzaku handles both. **However, `CREATE TABLE AS SELECT` lowercases the output column names**, which stops rules from matching (see the warning above). Until Suzaku normalizes lowercased CloudTrail field names, prefer Recipe 1 or 2, or re-alias every column back to its original casing in the CTAS `SELECT`.

## Verifying the conversion

Run any AWS command against the Parquet and confirm the event count and detections look right:

```bash
./suzaku aws-ct-timeline -f events.parquet -o timeline.csv
```

The `Events with hits / Total events` line should match what you get from the same logs in JSON form. If `Total events` is non-zero but there are **0 detections across all levels**, the field names were almost certainly lowercased during conversion — reconvert via one-event-per-line JSONL (Recipe 1).
