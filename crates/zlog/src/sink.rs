use std::io::Write;

use crate::{SCOPE_STRING_SEP_CHAR, Scope};

const LEVEL_OUTPUT_STRINGS: [&str; 5] = [
    //
    "ERROR", //
    "WARN ", //
    "INFO ", //
    "DEBUG", //
    "TRACE", //
];

pub fn submit(record: Record) {
    let mut stdout = std::io::stdout().lock();
    _ = writeln!(
        &mut stdout,
        "{} {} [{}] {}",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
        LEVEL_OUTPUT_STRINGS[record.level as usize],
        ScopeFmt(record.scope),
        record.message
    );
}

pub fn flush() {
    _ = std::io::stdout().lock().flush();
}

struct ScopeFmt(Scope);

impl std::fmt::Display for ScopeFmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        f.write_str(self.0[0])?;
        for scope in &self.0[1..] {
            if !scope.is_empty() {
                f.write_char(SCOPE_STRING_SEP_CHAR)?;
            }
            f.write_str(scope)?;
        }
        Ok(())
    }
}

pub struct Record<'a> {
    pub scope: Scope,
    pub level: log::Level,
    pub message: &'a std::fmt::Arguments<'a>,
}
