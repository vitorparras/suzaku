# ログをParquetに変換する

SuzakuのAWS系コマンド（`aws-ct-timeline`、`aws-ct-metrics`、`aws-ct-summary`、`aws-ct-search`）は、`.parquet` ファイルを `-f/--file` と `-d/--directory` の両方で直接読み込めます。Parquetは列指向で圧縮されるため、大きなCloudTrailエクスポートでも同等のJSONよりディスク上のサイズがはるかに小さく、スキャンも高速です。

このページでは、Suzakuが期待するParquetのレイアウトと、CloudTrailログをそれに変換するための検証済みレシピを説明します。

## Suzakuが期待する形式

Suzakuは各Parquetの**行（row）**を1つのCloudTrailイベントとして扱います。そのため、1行1イベントで、CloudTrailの各フィールドが最上位のカラムになっている必要があります。

検出を正しく動作させるには、次の2点が重要です。

1. **フィールド名は元のCloudTrailの大文字小文字を保つこと**（`eventName`、`eventSource`、`userIdentity`、`sourceIPAddress` など）。Sigmaルールはこの正確な名前でマッチします。カラム名を小文字化する変換（例: `eventname`）を行うと、イベントは読み込まれますが**どのルールにもマッチしません**。下記の警告を参照してください。
2. **ネストされたフィールド**（`userIdentity`、`requestParameters`、`responseElements`、`additionalEventData`、`serviceEventDetails`、`resources`、`tlsDetails`、`insightDetails`）は、本物のネストされたstructカラムとして格納されていても、JSON文字列としてエンコードされていても構いません。Suzakuは両方に対応します。structカラムはネストされたオブジェクトとして読み込み、これらの既知のエンベロープフィールドがJSON文字列として格納されている場合（Athena/Glue/Firehoseパイプラインがよく生成する形式）はオブジェクトに復元するため、`requestParameters.bucketName` のようなネストされた値にもルールがマッチできます。

`eventTime` は文字列でも、Parquetの `TIMESTAMP` カラムでも構いません。タイムゾーンなしの `TIMESTAMP` はUTCとして扱われる（CloudTrailでは正しい）ため、時刻フィルタ（`--timeline-start/--timeline-end`）や日付サマリも機能します。

対応している圧縮コーデック: **snappy、gzip、zstd、lz4**（および無圧縮）。

!!! warning "フィールド名の大文字小文字を保持すること"
    最も安全な変換は、各イベントの最上位JSONキーをそのままカラムにする方法で、これなら大文字小文字が保持されます。データをいったんネストされたstruct型に通すと、フィールド名が**小文字化**されがちです。具体的には:

    - **JSONL（1行1イベント）**に対するDuckDB `read_json_auto()` → キーがカラムになり、大文字小文字が保持される。✅
    - `{"Records":[…]}` オブジェクトに対するDuckDB `unnest(Records)` → structを経由するため、フィールド名が小文字化される。❌
    - AWS **Athena `CREATE TABLE AS`** も同様にカラム名を小文字化する。❌

    Parquetのフィールドが小文字化されている（`eventname`、`useridentity` など）場合、Suzakuはイベントをスキャンしたと報告しますが検出は0件になります。まず1行1イベントのJSONLに平坦化してから（下記参照）変換してください。

## レシピ1 — DuckDB（推奨）

[DuckDB](https://duckdb.org/) は単体で完結するバイナリで、全行にわたってスキーマを自動推論します（そのため、先頭のイベントにたまたま欠けているフィールドがあってもカラムが落ちません）。

ログがすでに**JSONL**（1行1JSONイベント）の場合:

```bash
duckdb -c "COPY (SELECT * FROM read_json_auto('events.jsonl'))
           TO 'events.parquet' (FORMAT PARQUET, COMPRESSION ZSTD);"
```

ログが標準のCloudTrail配信形式 `{"Records":[ … ]}`（1つまたは複数の `.json` ファイル）の場合、まず `jq` で1行1イベントのJSONLに平坦化してから変換します。これでフィールド名の大文字小文字が保持されます:

```bash
jq -c '.Records[]' cloudtrail.json > events.jsonl
duckdb -c "COPY (SELECT * FROM read_json_auto('events.jsonl'))
           TO 'events.parquet' (FORMAT PARQUET, COMPRESSION ZSTD);"
```

gzip圧縮されたCloudTrailオブジェクトのディレクトリ全体を一度に変換する場合:

```bash
zcat AWSLogs/**/*.json.gz | jq -c '.Records[]' > events.jsonl
duckdb -c "COPY (SELECT * FROM read_json_auto('events.jsonl'))
           TO 'events.parquet' (FORMAT PARQUET, COMPRESSION ZSTD);"
```

## レシピ2 — Python（pyarrow）

```python
import json
import pyarrow as pa
import pyarrow.parquet as pq

# 1行1イベント（JSONL）。{"Records":[...]} 形式のファイルなら、読み込んで data["Records"] を使う。
rows = [json.loads(line) for line in open("events.jsonl")]

# 先頭行に欠けているフィールドがあってもスキーマ推論でカラムが落ちないよう、
# 全イベントにわたってキーの和集合を取る。
keys = []
for r in rows:
    for k in r:
        if k not in keys:
            keys.append(k)
rows = [{k: r.get(k) for k in keys} for r in rows]

pq.write_table(pa.Table.from_pylist(rows), "events.parquet", compression="zstd")
```

pyarrowはネストされた `dict`/`list` の値を本物のParquetのstruct/listカラムとしてシリアライズするため、Suzakuはそのまま読み込めます。

## レシピ3 — AWS Athena / Glue

AthenaとGlueが書き出すParquetは、エンベロープフィールドがJSON文字列で、`eventTime` がタイムゾーンなしの `TIMESTAMP` になっています（Suzakuはどちらにも対応）。**ただし `CREATE TABLE AS SELECT` は出力カラム名を小文字化する**ため、ルールがマッチしなくなります（上記の警告を参照）。Suzakuが小文字化されたCloudTrailフィールド名を正規化するようになるまでは、レシピ1か2を使うか、CTASの `SELECT` で全カラムを元の大文字小文字に別名付け（re-alias）してください。

## 変換結果の確認

いずれかのAWSコマンドをParquetに対して実行し、イベント数と検出が妥当か確認します:

```bash
./suzaku aws-ct-timeline -f events.parquet -o timeline.csv
```

`Events with hits / Total events` の行が、同じログをJSON形式で処理したときと一致するはずです。`Total events` が0でないのに**全レベルで検出が0件**の場合、変換時にフィールド名が小文字化された可能性が非常に高いので、1行1イベントのJSONL経由で再変換してください（レシピ1）。
