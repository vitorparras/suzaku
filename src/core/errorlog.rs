//! Error/warning log file.
//!
//! Warnings and errors raised while processing logs are written to
//! `logs/errorlog-<YYYYMMDD_HHMMSS>.log` instead of the terminal. Writing them to
//! stdout/stderr interleaves with the progress bar (which redraws by moving the cursor
//! over the lines it wrote last, so any write it does not know about strands the whole
//! previous block on screen) and a directory full of unreadable files can bury the
//! scan results under thousands of lines. The file keeps every message, and the run
//! prints a single pointer to it at the end.
//!
//! The file is created lazily on the first message, so a clean run leaves no `logs/`
//! directory behind. Its first line is the command line that produced it, so a log sent
//! in with a bug report says which invocation it came from.

use chrono::Local;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const LOG_DIR: &str = "logs";

struct ErrorLog {
    path: PathBuf,
    writer: BufWriter<File>,
    count: usize,
}

fn state() -> &'static Mutex<Option<ErrorLog>> {
    static STATE: OnceLock<Mutex<Option<ErrorLog>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// Records a non-fatal warning. Not shown on the terminal.
pub fn log_warn(msg: &str) {
    write_entry("WARNING", msg);
}

/// Records an error. Not shown on the terminal, except for the run-stopping errors
/// that their call sites also print themselves.
pub fn log_error(msg: &str) {
    write_entry("ERROR", msg);
}

/// Path of the log file, once one has been created.
pub fn log_path() -> Option<PathBuf> {
    state()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|log| log.path.clone()))
}

/// Number of messages recorded so far.
pub fn entry_count() -> usize {
    state()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|log| log.count))
        .unwrap_or(0)
}

fn write_entry(level: &str, msg: &str) {
    let line = format!("[{level}] {msg}");
    let Ok(mut guard) = state().lock() else {
        // The mutex is only poisoned if a previous writer panicked; do not lose the message.
        eprintln!("{line}");
        return;
    };
    if guard.is_none() {
        match create_log(Path::new(LOG_DIR), &command_line()) {
            Ok(log) => *guard = Some(log),
            Err(e) => {
                // `logs/` is not writable (read-only mount, permissions). Falling back to
                // stderr keeps the message rather than dropping it silently.
                eprintln!("Cannot write to the error log in ./{LOG_DIR}: {e}");
                eprintln!("{line}");
                return;
            }
        }
    }
    let Some(log) = guard.as_mut() else { return };
    // Flush per message: `fatal_error` exits the process without running destructors,
    // and a killed scan should still leave the warnings it had already produced.
    if writeln!(log.writer, "{line}")
        .and_then(|_| log.writer.flush())
        .is_err()
    {
        eprintln!("{line}");
        return;
    }
    log.count += 1;
}

/// Creates `<dir>/errorlog-<timestamp>.log` and writes the command-line header.
fn create_log(dir: &Path, command_line: &str) -> std::io::Result<ErrorLog> {
    fs::create_dir_all(dir)?;
    let name = format!("errorlog-{}.log", Local::now().format("%Y%m%d_%H%M%S"));
    let path = dir.join(name);
    let mut writer = BufWriter::new(File::create(&path)?);
    writeln!(writer, "{command_line}")?;
    writeln!(writer)?;
    writer.flush()?;
    Ok(ErrorLog {
        path,
        writer,
        count: 0,
    })
}

/// The command line that started this run, quoted so it can be pasted back into a shell.
fn command_line() -> String {
    format_command_line(std::env::args())
}

fn format_command_line<I: IntoIterator<Item = String>>(args: I) -> String {
    args.into_iter()
        .map(|arg| {
            if arg.is_empty() || arg.contains([' ', '\t', '"', '\'', '\\']) {
                format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                arg
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read(path: &Path) -> String {
        let mut s = String::new();
        File::open(path).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn create_log_writes_the_command_line_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = create_log(dir.path(), "suzaku aws-ct-timeline -d ./sample").unwrap();
        writeln!(log.writer, "[WARNING] Skipping a.json: broken").unwrap();
        log.writer.flush().unwrap();

        let contents = read(&log.path);
        let mut lines = contents.lines();
        assert_eq!(lines.next(), Some("suzaku aws-ct-timeline -d ./sample"));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("[WARNING] Skipping a.json: broken"));
    }

    #[test]
    fn create_log_names_the_file_by_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let log = create_log(dir.path(), "suzaku").unwrap();
        let name = log.path.file_name().unwrap().to_string_lossy().to_string();
        // errorlog-20260710_200133.log
        assert!(name.starts_with("errorlog-"), "{name}");
        assert!(name.ends_with(".log"), "{name}");
        let stamp = &name["errorlog-".len()..name.len() - ".log".len()];
        assert_eq!(stamp.len(), 15, "{name}");
        assert_eq!(stamp.as_bytes()[8], b'_', "{name}");
        assert!(
            stamp
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 8 || b.is_ascii_digit()),
            "{name}"
        );
    }

    #[test]
    fn create_log_creates_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("logs");
        let log = create_log(&nested, "suzaku").unwrap();
        assert!(log.path.starts_with(&nested));
    }

    #[test]
    fn format_command_line_quotes_arguments_with_spaces() {
        let args = [
            "suzaku".to_string(),
            "aws-ct-timeline".to_string(),
            "-d".to_string(),
            "/my logs/ct".to_string(),
        ];
        assert_eq!(
            format_command_line(args),
            "suzaku aws-ct-timeline -d \"/my logs/ct\""
        );
    }

    #[test]
    fn format_command_line_escapes_quotes_and_backslashes() {
        let args = ["suzaku".to_string(), r#"C:\Users\a b"#.to_string()];
        assert_eq!(format_command_line(args), r#"suzaku "C:\\Users\\a b""#);
    }
}
