//! Utilities for working with duct commands, particularly for logging.
//!
//! Provides a [`Cmd`] builder that tracks command metadata for logging purposes
//! while still producing duct [`Expression`]s for execution.

use duct::Expression;
use std::ffi::OsStr;
use std::fmt::{self, Write};
use std::path::{Path, PathBuf};

/// A command builder that tracks its arguments for logging purposes.
///
/// This wraps duct's command creation to maintain metadata about the command
/// being built, allowing us to format it as a shell-like string for logging.
///
/// # Example
/// ```
/// use selfci::duct_util::Cmd;
///
/// let cmd = Cmd::new("git").args(["rebase", "main"]).dir("/repo");
/// assert_eq!(format!("{}", cmd), "git rebase main  # in /repo");
///
/// // Execute with duct
/// // let output = cmd.to_expression().read()?;
/// ```
#[derive(Clone, Debug)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
    env_vars: Vec<(String, String)>,
    dir: Option<PathBuf>,
}

impl Cmd {
    /// Creates a new command with the given program name.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_string_lossy().into_owned(),
            args: Vec::new(),
            env_vars: Vec::new(),
            dir: None,
        }
    }

    /// Adds a single argument to the command.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_string_lossy().into_owned());
        self
    }

    /// Adds multiple arguments to the command.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.args.push(arg.as_ref().to_string_lossy().into_owned());
        }
        self
    }

    /// Sets an environment variable for the command.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env_vars.push((
            key.as_ref().to_string_lossy().into_owned(),
            value.as_ref().to_string_lossy().into_owned(),
        ));
        self
    }

    /// Sets the working directory for the command.
    pub fn dir(mut self, path: impl AsRef<Path>) -> Self {
        self.dir = Some(path.as_ref().to_path_buf());
        self
    }

    /// Converts this command to a duct Expression for execution.
    ///
    /// Note: This only applies program, args, env vars, and dir.
    /// For other duct features (stdin, stdout capture, etc.), chain them
    /// on the returned Expression.
    pub fn to_expression(&self) -> Expression {
        let mut expr = duct::cmd(&self.program, &self.args);
        for (key, value) in &self.env_vars {
            expr = expr.env(key, value);
        }
        if let Some(dir) = &self.dir {
            expr = expr.dir(dir);
        }
        expr
    }

    /// Returns a wrapper that formats as `> <command>\n` for logging.
    ///
    /// # Example
    /// ```
    /// use selfci::duct_util::Cmd;
    ///
    /// let cmd = Cmd::new("git").args(["status"]);
    /// let mut log = String::new();
    /// use std::fmt::Write;
    /// write!(log, "{}", cmd.log_line()).unwrap();
    /// assert_eq!(log, "> git status\n");
    /// ```
    pub fn log_line(&self) -> LogLine<'_> {
        LogLine(self)
    }

    /// Writes the shell-like command representation to a formatter.
    fn fmt_shell(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Format environment variables
        for (key, value) in &self.env_vars {
            write_shell_escaped(f, key)?;
            f.write_char('=')?;
            write_shell_escaped(f, value)?;
            f.write_char(' ')?;
        }

        // Format program
        write_shell_escaped(f, &self.program)?;

        // Format arguments
        for arg in &self.args {
            f.write_char(' ')?;
            write_shell_escaped(f, arg)?;
        }

        // Format working directory if set
        if let Some(dir) = &self.dir {
            write!(f, "  # in {}", dir.display())?;
        }

        Ok(())
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_shell(f)
    }
}

/// A wrapper that formats a command as `> <command>\n` for logging.
///
/// Created by [`Cmd::log_line`].
pub struct LogLine<'a>(&'a Cmd);

impl fmt::Display for LogLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("> ")?;
        self.0.fmt_shell(f)?;
        f.write_char('\n')
    }
}

/// Writes a shell-escaped string to the formatter.
/// Only adds quotes if the string contains special characters.
fn write_shell_escaped(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    if s.is_empty() {
        return f.write_str("''");
    }

    // Check if string needs quoting
    let needs_quoting = s.contains(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\''
                    | '\\'
                    | '$'
                    | '`'
                    | '!'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '|'
                    | '&'
                    | ';'
                    | '#'
                    | '~'
            )
    });

    if !needs_quoting {
        return f.write_str(s);
    }

    // Use single quotes if no single quotes in string
    if !s.contains('\'') {
        f.write_char('\'')?;
        f.write_str(s)?;
        return f.write_char('\'');
    }

    // Use double quotes, escaping special chars
    f.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' | '\\' | '$' | '`' => {
                f.write_char('\\')?;
                f.write_char(c)?;
            }
            _ => f.write_char(c)?,
        }
    }
    f.write_char('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_command() {
        let cmd = Cmd::new("git").arg("status");
        assert_eq!(format!("{}", cmd), "git status");
    }

    #[test]
    fn test_command_with_multiple_args() {
        let cmd = Cmd::new("git").args(["rebase", "main"]);
        assert_eq!(format!("{}", cmd), "git rebase main");
    }

    #[test]
    fn test_command_with_flags() {
        let cmd = Cmd::new("git").args(["log", "--oneline", "-n", "5"]);
        assert_eq!(format!("{}", cmd), "git log --oneline -n 5");
    }

    #[test]
    fn test_command_with_path_arg() {
        let cmd = Cmd::new("git").args(["worktree", "add", "--detach", "/tmp/foo", "abc123"]);
        assert_eq!(
            format!("{}", cmd),
            "git worktree add --detach /tmp/foo abc123"
        );
    }

    #[test]
    fn test_command_with_special_chars() {
        let cmd = Cmd::new("jj").args(["log", "-T", r#"change_id ++ "\n""#]);
        assert_eq!(format!("{}", cmd), r#"jj log -T 'change_id ++ "\n"'"#);
    }

    #[test]
    fn test_command_with_dir() {
        let cmd = Cmd::new("git").arg("status").dir("/some/path");
        assert_eq!(format!("{}", cmd), "git status  # in /some/path");
    }

    #[test]
    fn test_command_with_env() {
        let cmd = Cmd::new("cargo")
            .arg("build")
            .env("RUSTFLAGS", "-Awarnings");
        assert_eq!(format!("{}", cmd), "RUSTFLAGS=-Awarnings cargo build");
    }

    #[test]
    fn test_command_with_env_and_dir() {
        let cmd = Cmd::new("cargo")
            .arg("build")
            .env("RUSTFLAGS", "-Awarnings")
            .dir("/project");
        assert_eq!(
            format!("{}", cmd),
            "RUSTFLAGS=-Awarnings cargo build  # in /project"
        );
    }

    #[test]
    fn test_command_with_spaces_in_arg() {
        let cmd = Cmd::new("echo").arg("hello world");
        assert_eq!(format!("{}", cmd), "echo 'hello world'");
    }

    #[test]
    fn test_command_with_single_quotes_in_arg() {
        let cmd = Cmd::new("echo").arg("it's working");
        assert_eq!(format!("{}", cmd), r#"echo "it's working""#);
    }

    #[test]
    fn test_empty_arg() {
        let cmd = Cmd::new("echo").arg("");
        assert_eq!(format!("{}", cmd), "echo ''");
    }

    #[test]
    fn test_arg_with_equals() {
        let cmd = Cmd::new("git").args(["config", "user.name=Test"]);
        assert_eq!(format!("{}", cmd), "git config user.name=Test");
    }

    #[test]
    fn test_multiple_env_vars() {
        let cmd = Cmd::new("cargo")
            .arg("build")
            .env("RUSTFLAGS", "-Awarnings")
            .env("CARGO_TERM_COLOR", "always");
        assert_eq!(
            format!("{}", cmd),
            "RUSTFLAGS=-Awarnings CARGO_TERM_COLOR=always cargo build"
        );
    }

    #[test]
    fn test_to_expression_executes() {
        // Just verify it compiles and creates an expression
        let cmd = Cmd::new("echo").arg("hello");
        let expr = cmd.to_expression();
        // We can't easily test execution in unit tests, but we can verify
        // the expression is created
        assert!(format!("{:?}", expr).contains("echo"));
    }

    #[test]
    fn test_log_line() {
        let cmd = Cmd::new("git").args(["status", "-s"]);
        assert_eq!(format!("{}", cmd.log_line()), "> git status -s\n");
    }

    #[test]
    fn test_log_line_with_dir() {
        let cmd = Cmd::new("git").arg("rebase").dir("/repo");
        assert_eq!(format!("{}", cmd.log_line()), "> git rebase  # in /repo\n");
    }

    #[test]
    fn test_log_line_write_to_string() {
        let cmd = Cmd::new("cargo").arg("build");
        let mut log = String::new();
        write!(log, "{}", cmd.log_line()).unwrap();
        assert_eq!(log, "> cargo build\n");
    }
}
