# Analiz Komutları

## `aws-ct-metrics` komutu

Bu komutu AWS CloudTrail günlüklerindeki alanlar üzerinde metrikler oluşturmak için kullanın.
Varsayılan olarak, `eventName` alanını tarayacaktır.
Şu anda bu komutu, tespit kurallarının yazılmasına öncelik verebilmek için hangi API çağrılarının en yaygın olduğunu belirlemek amacıyla kullanıyoruz.

## Komut kullanımı
```
Usage: suzaku aws-ct-metrics <INPUT> [OPTIONS]

Input:
  -d, --directory <DIR>  Directory of multiple gz/json files
  -f, --file <FILE>      File path to one gz/json file

Filtering:
  -s, --include-sts-keys       Include temporary AWS STS access key IDs
      --timeline-start <DATE>  Start time of the events to load (ex: "2022-02-22T23:59:59Z)
      --timeline-end <DATE>    End time of the events to load (ex: "2020-02-22T00:00:00Z")
      --time-offset <OFFSET>   Scan recent events based on an offset (ex: 1y, 3M, 30d, 24h, 30m)
      --file-date-from <DATE>  Filter files by start date based on AWSLogs S3 path date structure (ex: "20240101")
      --file-date-to <DATE>    Filter files by end date based on AWSLogs S3 path date structure (ex: "20241231")

Output:
  -F, --field-name <FIELD_NAME,...>  The field(s) to generate metrics for. Comma-separate or repeat to aggregate several in a single scan, e.g. -F sourceIPAddress,userAgent [default: eventName]
  -C, --clobber                      Overwrite files when saving
  -G, --geo-ip <MAXMIND-DB-DIR>      Add GeoIP (ASN, city, country) info to IP addresses [alias: --GeoIP]
  -o, --output <FILE>                Save the results to a file
  -t, --output-type <FORMAT,...>     Output format(s) (only used with -o): csv (default), json, jsonl, duckdb. Comma-separate or repeat to write several at once, e.g. -t csv,duckdb [default: csv] [possible values: csv, json, jsonl, duckdb]

General Options:
  -h, --help  Show the help menu

Display Settings:
  -K, --no-color  Disable color output
  -q, --quiet     Quiet mode: do not display the launch banner
```

### `aws-ct-metrics` komut örnekleri

* `eventName` API çağrılarının bir tablosunu ekrana yazdır: `./suzaku aws-ct-metrics -d ../suzaku-sample-data`
* Bir CSV dosyasına kaydet: `./suzaku aws-ct-metrics -d ../suzaku-sample-data -o sample-metrics.csv`
