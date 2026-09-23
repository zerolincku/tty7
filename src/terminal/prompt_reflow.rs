//! Reflows a pane's grid without stranding the prompt the shell is about to
//! redraw (#654).
//!
//! A shell sitting at its prompt answers a `SIGWINCH` by redrawing the prompt
//! relative to where it left the cursor: zsh and bash move up as many rows as
//! the prompt's last line and the command being edited took at the width they
//! last drew at, return to column 0, and clear everything below. A column
//! change reflows the grid underneath that arithmetic. A prompt whose right
//! side sits near the edge — an `RPROMPT` clock, a right-aligned segment —
//! wraps onto a second row when the pane narrows by two columns or more, the
//! grid keeps the cursor on the row holding the wrapped tail, and the redraw
//! starts there: the head of the old prompt stays behind, still marked as
//! wrapping into the new one. Every such step of a window drag leaves another,
//! and a later widening joins them into one long line of prompt fragments
//! across the pane — the shell and the grid agreed on the width the whole time.
//!
//! So a reflow at a prompt hands the prompt back first, the way kitty does for
//! a shell with integration: the cursor's line, from its first row down, is
//! the shell's to repaint and is cleared, and after the reflow the cursor is
//! put back as many rows below that line's start as it was before — the count
//! the shell is about to move up by. A second reflow that lands before the
//! shell has redrawn (a drag outrunning the prompt) finds those rows blank and
//! unwrapped, so it leaves them where they are and the count still holds.
//!
//! Only the cursor's own line is handed back. A multi-line prompt's upper
//! lines end in a newline, not a wrap, so they reflow like any other output;
//! they are also the rows the shell redraws without being told where they are.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{MIN_COLUMNS, Term, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};

use super::size::TermSize;

/// Resizes `term` to `size`. `shell_redraws` says the pane is at a shell
/// prompt that will repaint itself for the new width — the caller's shell
/// integration state; everything else resizes exactly as `Term::resize` does.
/// Returns whether the prompt was cleared and needs the shell's redraw.
pub(super) fn resize<T: EventListener>(
    term: &mut Term<T>,
    size: TermSize,
    shell_redraws: bool,
) -> bool {
    // Rows alone reflow nothing, and the alternate screen is a full-screen
    // program's, which repaints the whole of it.
    let reflows = size.cols.max(MIN_COLUMNS) != term.columns();
    if !shell_redraws || !reflows || term.mode().contains(TermMode::ALT_SCREEN) {
        term.resize(size);
        return false;
    }

    let cursor = term.grid().cursor.point;
    let last = Column(term.columns() - 1);
    // Stops at the top of the screen: the shell's cursor-up stops there too,
    // so rows already in scrollback are not part of what it will move over.
    let mut start = cursor.line;
    while start > Line(0)
        && term.grid()[Line(start.0 - 1)][last]
            .flags
            .contains(Flags::WRAPLINE)
    {
        start -= 1;
    }
    let rows_up = (cursor.line.0 - start.0) as usize;

    term.grid_mut().cursor.point = Point::new(start, Column(0));
    term.grid_mut().cursor.input_needs_wrap = false;
    term.clear_screen(ClearMode::Below);
    term.resize(size);

    // Linefeeds rather than a jump: a line that sat low on the screen has to
    // scroll the output above it up, as the shell's own layout would have.
    for _ in 0..rows_up {
        term.linefeed();
    }
    let last = Column(term.columns() - 1);
    term.grid_mut().cursor.point.column = cursor.column.min(last);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    const PRIOR: &[u8] = b"echo hi\r\nhi\r\n";
    const CLOCK: &str = "16:46:33";

    fn pane(cols: usize) -> (Term<VoidListener>, Processor) {
        let mut term = Term::new(Config::default(), &TermSize::new(cols, 12), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, PRIOR);
        (term, parser)
    }

    /// What zsh 5.9 writes on `SIGWINCH` for `PROMPT='[~] '` with an
    /// `RPROMPT` clock, as captured off a real pty: up the rows it last drew
    /// above the cursor, back to column 0, clear below, and lay the prompt out
    /// for `cols` — the clock right-aligned one column short of the edge, or
    /// dropped when a long command line leaves it no room.
    fn zsh_redraw(cols: usize, rows_up: usize, buffer: &str) -> Vec<u8> {
        let mut out = "\x1b[A".repeat(rows_up);
        out.push_str("\r\r\x1b[0m\x1b[J[~] ");
        if buffer.is_empty() {
            out.push_str(&format!(
                "\x1b[K\x1b[{}C{CLOCK}\x1b[{}D",
                cols - 13,
                cols - 5
            ));
        } else {
            out.push_str(buffer);
            out.push_str("\x1b[K");
        }
        out.into_bytes()
    }

    /// Every non-blank row, scrollback included, trailing blanks trimmed.
    fn text(term: &Term<VoidListener>) -> Vec<String> {
        let top = -(term.grid().history_size() as i32);
        (top..term.screen_lines() as i32)
            .map(|line| {
                let row = &term.grid()[Line(line)];
                (0..term.columns())
                    .map(|col| row[Column(col)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|row| !row.is_empty())
            .collect()
    }

    fn prompt_row(cols: usize) -> String {
        format!("[~]{}{CLOCK}", " ".repeat(cols - 12))
    }

    /// The reported pattern: a clock prompt redrawn after each step of a drag
    /// that narrows the pane by more than one column at a time, then widens it
    /// back. Measured against the unguarded resize, so the test also proves it
    /// reproduces the fragments rather than passing on a scenario that never
    /// produced any.
    #[test]
    fn a_prompt_narrowed_under_its_right_side_leaves_no_fragments() {
        let drag = |guarded: bool| {
            let (mut term, mut parser) = pane(98);
            parser.advance(&mut term, &zsh_redraw(98, 0, ""));
            for cols in [95, 92, 89, 86, 89, 92, 95, 98] {
                if guarded {
                    resize(&mut term, TermSize::new(cols, 12), true);
                } else {
                    term.resize(TermSize::new(cols, 12));
                }
                parser.advance(&mut term, &zsh_redraw(cols, 0, ""));
            }
            text(&term)
        };

        let unguarded = drag(false);
        assert!(
            unguarded.iter().filter(|row| row.contains("[~]")).count() > 1,
            "the scenario no longer strands a prompt without the guard: {unguarded:#?}"
        );
        assert_eq!(drag(true), ["echo hi", "hi", &prompt_row(98)]);
    }

    /// A command line long enough to wrap: zsh moves up by the rows it took at
    /// the old width, which the reflow changes (two rows at 98 columns, three
    /// at 60). Landing one row off either way strands a row of the old line or
    /// clears a row of the output above it.
    #[test]
    fn a_wrapped_command_line_is_redrawn_in_place() {
        let buffer = "x".repeat(150);
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, &buffer));

        resize(&mut term, TermSize::new(60, 12), true);
        parser.advance(&mut term, &zsh_redraw(60, 1, &buffer));
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(56)),
                "x".repeat(60),
                "x".repeat(34),
            ]
        );

        resize(&mut term, TermSize::new(98, 12), true);
        parser.advance(&mut term, &zsh_redraw(98, 2, &buffer));
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(94)),
                "x".repeat(56),
            ]
        );
    }

    /// A drag can outrun the shell: a second reflow lands before the redraw
    /// for the first, on a line the first one already cleared. The shell still
    /// moves up by the rows of the layout it drew last, which the cleared rows
    /// above the cursor still are.
    #[test]
    fn back_to_back_reflows_before_the_redraw_strand_nothing() {
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, ""));
        resize(&mut term, TermSize::new(92, 12), true);
        resize(&mut term, TermSize::new(86, 12), true);
        parser.advance(&mut term, &zsh_redraw(86, 0, ""));
        assert_eq!(text(&term), ["echo hi", "hi", &prompt_row(86)]);

        let buffer = "x".repeat(150);
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, &buffer));
        resize(&mut term, TermSize::new(60, 12), true);
        resize(&mut term, TermSize::new(80, 12), true);
        parser.advance(&mut term, &zsh_redraw(80, 1, &buffer));
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(76)),
                "x".repeat(74),
            ]
        );
    }

    /// Low on the screen, putting the cursor back below the cleared line means
    /// scrolling: the shell's layout had those rows, so the output above moves
    /// up rather than the redraw climbing into it.
    #[test]
    fn a_line_at_the_bottom_scrolls_the_output_above_it() {
        let buffer = "x".repeat(150);
        let mut term = Term::new(Config::default(), &TermSize::new(98, 4), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"one\r\ntwo\r\n");
        parser.advance(&mut term, &zsh_redraw(98, 0, &buffer));
        assert_eq!(term.grid().cursor.point.line, Line(3));

        resize(&mut term, TermSize::new(60, 4), true);
        parser.advance(&mut term, &zsh_redraw(60, 1, &buffer));
        assert_eq!(
            text(&term),
            [
                "one".to_string(),
                "two".to_string(),
                format!("[~] {}", "x".repeat(56)),
                "x".repeat(60),
                "x".repeat(34),
            ]
        );
    }

    /// Away from a prompt nothing is going to repaint the line, so it reflows
    /// like any output — as does a prompt whose width did not change.
    #[test]
    fn only_a_column_change_at_a_prompt_is_handed_back() {
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, "y".repeat(150).as_bytes());
        resize(&mut term, TermSize::new(60, 12), false);
        resize(&mut term, TermSize::new(98, 12), false);
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                "y".repeat(98),
                "y".repeat(52)
            ]
        );

        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, ""));
        resize(&mut term, TermSize::new(98, 8), true);
        assert_eq!(text(&term), ["echo hi", "hi", &prompt_row(98)]);
    }

    /// The alternate screen belongs to a full-screen program, which repaints
    /// all of it; a stale prompt flag must not blank it.
    #[test]
    fn the_alternate_screen_is_left_alone() {
        let (mut term, mut parser) = pane(60);
        parser.advance(&mut term, b"\x1b[?1049h\x1b[Hstatus bar");
        resize(&mut term, TermSize::new(98, 12), true);
        assert_eq!(text(&term), ["status bar"]);
    }
}
