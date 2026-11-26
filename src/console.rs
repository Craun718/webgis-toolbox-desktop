use chrono::Local;
use std::{
    io::{self, IsTerminal, Write},
    sync::OnceLock,
};

enum Terminal {
    Stderr,
    Stdout,
    None,
}

#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub const ERROR_LABEL: &str = "[ERRO]";
    pub const WARN_LABEL: &str = "[WARN]";
}

pub fn record(level: Level, source: &str, message: &str) -> String {
    let label = match level {
        Level::Info => "[INFO]",
        Level::Warn => "[WARN]",
        Level::Error => Level::ERROR_LABEL,
    };
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let body = format!("{timestamp} {source} | {message}");
    let line = format!("{label} {body}");
    let write_line = |writer: &mut dyn Write| match level {
        Level::Info => writeln!(writer, "{line}"),
        Level::Warn => writeln!(writer, "\x1b[33m{label}\x1b[0m {body}"),
        Level::Error => writeln!(writer, "\x1b[31m{label}\x1b[0m {body}"),
    };
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
            let _ = write_line(&mut io::stderr().lock());
        }
        Terminal::Stdout => {
            let _ = write_line(&mut io::stdout().lock());
        }
        Terminal::None => {}
    }
    line
}
