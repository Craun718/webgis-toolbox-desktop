use chrono::{DateTime, SecondsFormat, Utc};
use std::{
    io::{self, IsTerminal, Write},
    sync::OnceLock,
    time::SystemTime,
};

enum Terminal {
    Stderr,
    Stdout,
    None,
}

pub fn record(level: &str, source: &str, message: &str) -> String {
    let timestamp =
        DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Millis, true);
    let line = format!("{timestamp} [{level}] {source} | {message}");
    static TERMINAL: OnceLock<Terminal> = OnceLock::new();
    match TERMINAL.get_or_init(|| {
        if io::stderr().is_terminal() {
            Terminal::Stderr
        } else if io::stdout().is_terminal() {
            Terminal::Stdout
        } else {
            Terminal::None
        }
    }) {
        Terminal::Stderr => {
            let _ = writeln!(io::stderr().lock(), "{line}");
        }
        Terminal::Stdout => {
            let _ = writeln!(io::stdout().lock(), "{line}");
        }
        Terminal::None => {}
    }
    line
}
