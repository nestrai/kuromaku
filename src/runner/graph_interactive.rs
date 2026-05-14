//! Production stdin reader for the inline human-handoff path (issue #361).
//!
//! Separated from `graph.rs` so the driver loop stays focused on state
//! progression and the I/O surface lives in one file. Tests in
//! `graph.rs` pass a stub reader; production wires
//! [`StdinInteractiveReader`] through [`super::flow_api::select_human_handoff_policy`].
//!
//! Implementation notes
//! --------------------
//! * Reads via `std::io::stdin()` under `tokio::task::spawn_blocking` --
//!   `tokio::io::stdin()` has documented quirks on macOS and Windows
//!   (line buffering, partial reads under signals); `spawn_blocking`
//!   sidesteps the platform-specific behaviour and matches how the
//!   existing spinner already runs on a plain thread.
//! * Terminator: a blank line OR Ctrl-D (EOF). Leading blank lines are
//!   ignored so the operator can take a moment to start typing without
//!   the reader closing immediately. Once any non-blank line has been
//!   accumulated, the first blank line ends the read.
//! * The prompt header is rendered to stderr by
//!   [`crate::ui::print_human_prompt`] so it does not pollute any
//!   structured stdout artefact a caller may pipe downstream.

use crate::ui;

/// Default reader: prints an inline prompt to stderr, blocks on stdin
/// until the operator finishes typing (blank line or Ctrl-D), and
/// returns the accumulated body. Empty bodies are legal and surface as
/// `Ok("")` -- the driver decides whether to write a synthetic step.
#[derive(Default)]
pub struct StdinInteractiveReader;

#[async_trait::async_trait]
impl super::graph::HumanInputReader for StdinInteractiveReader {
    async fn read(
        &mut self,
        state_id: &str,
        task: Option<&str>,
        allowed_targets: &[String],
    ) -> std::io::Result<String> {
        // Render the prompt header on stderr before blocking on stdin.
        // Doing the print BEFORE spawn_blocking guarantees the user sees
        // the cursor where their typing will land; the executor's
        // worker thread cannot interleave additional output between
        // these two operations because nothing else is running.
        ui::print_human_prompt(state_id, task, allowed_targets);

        // Hand the blocking read to the tokio blocking pool. The
        // closure owns the std stdin lock for its full lifetime so
        // partial reads on slow typists do not race with anything
        // else that might attempt stdin.
        let join = tokio::task::spawn_blocking(read_blocking).await;
        match join {
            Ok(result) => result,
            Err(e) => Err(std::io::Error::other(format!(
                "inline stdin reader task panicked: {e}"
            ))),
        }
    }
}

/// Blocking line-by-line stdin reader, factored out so the closure
/// passed to `spawn_blocking` stays trivial and the loop semantics are
/// unit-testable via [`read_until_terminator`] (which the production
/// path also calls).
fn read_blocking() -> std::io::Result<String> {
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    read_until_terminator(&mut handle)
}

/// Read lines from `reader` until a blank line is seen after any
/// non-blank content, or until EOF.
///
/// Leading blank lines are ignored so the operator can pause before
/// typing. The returned body includes the trailing newlines on each
/// non-blank line; the caller trims one trailing newline before
/// constructing [`super::flow_api::LocalHumanInput`] so the formatted
/// step body shape is stable.
pub(crate) fn read_until_terminator<R: std::io::BufRead>(
    reader: &mut R,
) -> std::io::Result<String> {
    let mut acc = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF (Ctrl-D or closed pipe) -- return whatever we have,
            // even if empty. The driver collapses empty bodies to "no
            // synthetic step", mirroring #340's contract.
            break;
        }
        if line.trim().is_empty() {
            if acc.is_empty() {
                // Leading blank line -- the operator has not started
                // typing yet. Keep waiting rather than aborting.
                continue;
            }
            // Blank line after content -- terminator.
            break;
        }
        acc.push_str(&line);
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_until_terminator_accumulates_until_blank_line() {
        // The canonical happy path: a multi-line body terminated by a
        // blank line. Each non-blank line is appended verbatim including
        // its trailing newline, so the body shape matches what an
        // operator typing in a real terminal would produce.
        let input = "approve\nlooks good\n\n";
        let mut cursor = Cursor::new(input.as_bytes());
        let out = read_until_terminator(&mut cursor).unwrap();
        assert_eq!(out, "approve\nlooks good\n");
    }

    #[test]
    fn read_until_terminator_handles_eof_without_blank_line() {
        // Closing stdin (Ctrl-D or a piped stdin that closes after the
        // payload) must be a valid terminator -- the production caller
        // pipes `"approve\n"` with no trailing blank and expects the
        // body to come back. Anything less is a deadlock waiting to
        // happen.
        let input = "approve\n";
        let mut cursor = Cursor::new(input.as_bytes());
        let out = read_until_terminator(&mut cursor).unwrap();
        assert_eq!(out, "approve\n");
    }

    #[test]
    fn read_until_terminator_skips_leading_blanks() {
        // An operator who hits Enter before typing must not end the
        // read prematurely -- the reader silently waits for content
        // and then for a terminator.
        let input = "\n\napprove\n\n";
        let mut cursor = Cursor::new(input.as_bytes());
        let out = read_until_terminator(&mut cursor).unwrap();
        assert_eq!(out, "approve\n");
    }

    #[test]
    fn read_until_terminator_empty_input_returns_empty_string() {
        // Immediate EOF (e.g. `< /dev/null`) must return an empty
        // string rather than erroring; the driver collapses empty
        // bodies to no synthetic step on disk.
        let input = "";
        let mut cursor = Cursor::new(input.as_bytes());
        let out = read_until_terminator(&mut cursor).unwrap();
        assert_eq!(out, "");
    }
}
