//! Facts about the terminal and the output format of a run.
//!
//! [`Terminal`] is probed once at startup and owned by [`crate::mode::Mode`],
//! so every module agrees on what a terminal is: bordered tables and indented
//! JSON on a terminal, tab-separated tables and compact JSON on a pipe.
//! [`OutputFormat`] is what a run prints. Both live here, below `mode` and
//! `output`, so each can name them without importing the other.

use std::fmt;
use std::io::IsTerminal;

use clap::ValueEnum;

/// Which standard streams are terminals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Terminal {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl Terminal {
    /// The process's real standard streams. Only the entry point calls this.
    #[must_use]
    pub fn probe() -> Self {
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
            stderr: std::io::stderr().is_terminal(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Pretty,
    Table,
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pretty => write!(f, "pretty"),
            Self::Table => write!(f, "table"),
            Self::Json => write!(f, "json"),
        }
    }
}
