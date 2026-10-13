//! How one transcript entry looks. Ports `src/ui/MessageView.tsx` (tests: `tests/messageview.test.tsx`).
//!
//! - user: `> ` + the text in `theme::USER`; assistant: `● ` + Markdown; tool:
//!   `● name label` then `  ⎿ summary` (`running…` while it runs; error red, declined yellow), then
//!   (ctrl+o) its subagent steps `    · step`, then its diff collapsed to [`COLLAPSED_DIFF_LINES`]
//!   with an `(ctrl+o)` hint; system: a notice block, Markdown, or dim/error text. A blank line
//!   follows every entry (Marv's `marginBottom`).
//! - The 2-column marker ("● ", "> ") is a fixed-width gutter: Ink laid out "● " as 1 column but
//!   drew it as 2, so text beside it wrapped one column too wide and lost a character at every
//!   line break. Here the gutter is a fixed span and the text wraps at `width - 2` with a hanging
//!   indent.
//! - A saved session may have been edited by hand: a malformed diff (or a line in it) is skipped
//!   rather than taking the App down on resume (serde `default`s; non-object lines dropped by the
//!   engine's loader).
//! - Memoised in Marv (`memo`): here the transcript caches each entry's lines per width and
//!   `show_steps`, so a transcript change only re-lays-out entries that changed.
//! - Every string goes through `printable()` here, at render time.

use marv_core::printable;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::diff_view::{self, DiffOptions};
use super::markdown::{self, beside, wrap_text};
use super::select::truncate_end;
use super::{notice_view, theme};
use crate::view::{Message, Role, ToolState};

/// A change's diff under its entry: enough to see what changed; ctrl+o shows the rest.
pub const COLLAPSED_DIFF_LINES: usize = 15;

/// `lines` beside the 2-column gutter: `mark` on the first row, a hanging indent after.
fn gutter(lines: Vec<Line<'static>>, mark: &'static str, style: Style) -> Vec<Line<'static>> {
    beside(lines, Span::styled(mark, style), Span::raw("  "))
}

/// Whether an entry's lines depend on ctrl+o (its steps or its diff), so only those are laid out
/// again when it's toggled.
pub fn has_details(message: &Message) -> bool {
    message.tool.as_ref().is_some_and(|t| t.steps.is_some() || t.diff.is_some())
}

/// One entry as lines. `streaming`: the assistant reply still coming in (parsed incrementally).
/// `show_steps`: ctrl+o, details (subagents' steps, whole diffs).
pub fn render(
    message: &Message,
    width: u16,
    show_steps: bool,
    streaming: Option<&mut markdown::StreamLexer>,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme::DIM);
    let mut lines = match message.role {
        Role::User => {
            let style = Style::new().fg(theme::USER);
            gutter(wrap_text(&printable(&message.text), style, width.saturating_sub(2)), "> ", style)
        }
        Role::Assistant => {
            // The raw reply goes to markdown::render, which applies printable() itself.
            let body = markdown::render(&message.text, width.saturating_sub(2), streaming);
            gutter(body, "● ", Style::new().fg(theme::ASSISTANT))
        }
        Role::Tool => {
            // ● read_file src/app.tsx
            //   ⎿ 250 lines
            let (label, state, summary) = match &message.tool {
                Some(tool) => (tool.label.as_str(), tool.status, tool.summary.as_deref()),
                None => ("", ToolState::Done, None),
            };
            let failed = state == ToolState::Error;
            let mut lines = vec![truncate_end(
                Line::from(vec![
                    Span::styled("● ", Style::new().fg(if failed { theme::ERROR } else { theme::ACCENT })),
                    Span::styled(printable(&message.text), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}", printable(label)), dim),
                ]),
                width,
            )];
            let summary = if state == ToolState::Running { summary.unwrap_or("running…") } else { summary.unwrap_or("") };
            let color = match state {
                ToolState::Error => theme::ERROR,
                ToolState::Declined => theme::WARNING,
                _ => theme::DIM,
            };
            let summary = Span::styled(format!("  ⎿ {}", printable(summary)), Style::new().fg(color));
            lines.push(truncate_end(Line::from(summary), width));
            let tool = message.tool.as_ref();
            if show_steps {
                for step in tool.and_then(|t| t.steps.as_ref()).into_iter().flatten() {
                    lines.push(truncate_end(Line::from(Span::styled(format!("    · {}", printable(step)), dim)), width));
                }
            }
            if let Some(diff) = tool.and_then(|t| t.diff.as_ref()).filter(|d| !d.lines.is_empty()) {
                let options = DiffOptions {
                    max: (!show_steps).then_some(COLLAPSED_DIFF_LINES),
                    more: diff.more,
                    hint: (!show_steps).then_some("ctrl+o"),
                    indent: 4,
                };
                lines.extend(diff_view::render(&diff.lines, width, options));
            }
            lines
        }
        Role::System => match &message.notice {
            // The notice block ends in its own blank line.
            Some(notice) => return notice_view::render(notice, width),
            None if message.markdown => markdown::render(&message.text, width, None),
            None => {
                let color = if message.is_error { theme::ERROR } else { theme::DIM };
                wrap_text(&printable(&message.text), Style::new().fg(color), width)
            }
        },
    };
    lines.push(Line::default());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{DiffKind, DiffLine, DiffShown, Notice, NoticeItem, NoticeTone, ToolStatus};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::{Terminal, widgets::Paragraph};

    fn message(role: Role, text: &str) -> Message {
        Message { id: 0, role, text: text.into(), is_error: false, markdown: false, notice: None, tool: None }
    }

    fn tool(name: &str, status: ToolStatus) -> Message {
        Message { tool: Some(status), ..message(Role::Tool, name) }
    }

    fn status(label: &str, state: ToolState, summary: Option<&str>) -> ToolStatus {
        ToolStatus { label: label.into(), status: state, summary: summary.map(Into::into), steps: None, diff: None }
    }

    /// Draws the entry through ratatui's `TestBackend` at a terminal `width` and returns the buffer.
    fn draw(width: u16, message: &Message, show_steps: bool) -> Buffer {
        let lines = render(message, width, show_steps, None);
        let mut terminal = Terminal::new(TestBackend::new(width, lines.len() as u16)).unwrap();
        terminal.draw(|f| f.render_widget(Paragraph::new(lines), f.area())).unwrap();
        terminal.backend().buffer().clone()
    }

    /// The drawn rows as text, blank rows dropped, trailing spaces trimmed.
    fn rows(width: u16, message: &Message, show_steps: bool) -> Vec<String> {
        let buf = draw(width, message, show_steps);
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .filter(|l| !l.trim().is_empty())
            .collect()
    }

    const LONG: &str = "ScrollView calculates one Page Up scroll distance using the formula `viewport.clientHeight - 1` (line 35), which \
        ensures exactly one row of overlap with the previously visible area remains pinned to the top. When PgUp is pressed, \
        it calls scrollBy(-page), moving up by that many rows before clamping against 0.";

    #[test]
    fn text_never_exceeds_the_terminal_and_no_words_are_lost() {
        // A line wider than the terminal wraps on its own and the next line overwrites the overflow, so a character
        // disappears at every wrap.
        for width in [40, 79, 80, 120, 121] {
            for role in [Role::Assistant, Role::User] {
                let lines = render(&message(role, LONG), width, false, None);
                for line in &lines {
                    assert!(line.width() <= width as usize, "{role:?} at {width}: {line}");
                }
                let shown = rows(width, &message(role, LONG), false).join(" ");
                let words: Vec<&str> = shown.trim_start_matches(['●', '>']).split_whitespace().collect();
                // Replies are rendered as Markdown, so their `code` loses its backticks.
                let expected = if role == Role::Assistant { LONG.replace('`', "") } else { LONG.to_string() };
                assert_eq!(words, expected.split_whitespace().collect::<Vec<_>>(), "{role:?} at {width}");
            }
        }
    }

    #[test]
    fn nested_markdown_lists_stay_within_the_terminal_at_any_width() {
        let text = "- Core: `agent.ts`, `app.tsx`, `cli.tsx`, `prompt.ts`, `paths.ts`, `mouse.ts`, `selection.ts`\n  - UI: `Martian.tsx`, `MessageView.tsx`, `ModelPicker.tsx`, `PromptInput.tsx`, `ScrollView.tsx`";
        for width in [30, 57, 80, 121] {
            for line in render(&message(Role::Assistant, text), width, false, None) {
                assert!(line.width() <= width as usize);
            }
        }
    }

    #[test]
    fn a_markdown_notice_like_skills_wraps_with_a_hanging_indent() {
        let text = "- `/count-lines`: Count lines of code in the project, broken down by file type. Use when asked how big the project is.";
        let lines = rows(40, &Message { markdown: true, ..message(Role::System, text) }, false);
        assert!(lines[0].starts_with("• /count-lines: Count"), "{lines:?}");
        for line in &lines[1..] {
            assert!(line.starts_with("  "), "continuation lines stay under the text: {line:?}");
        }
    }

    #[test]
    fn tool_lines_stay_within_the_terminal_too() {
        let m = tool("read_file", status("src/a/very/long/path/to/some/file.ts", ToolState::Done, Some("1 line")));
        for line in render(&m, 40, false, None) {
            assert!(line.width() <= 40);
        }
        assert_eq!(rows(80, &m, false), ["● read_file src/a/very/long/path/to/some/file.ts", "  ⎿ 1 line"]);
        let running = tool("bash", status("ls", ToolState::Running, None));
        assert_eq!(rows(80, &running, false)[1], "  ⎿ running…");
    }

    #[test]
    fn markers_sit_in_a_two_column_gutter() {
        assert_eq!(rows(80, &message(Role::User, "hi\nthere"), false), ["> hi", "  there"]);
        assert_eq!(rows(80, &message(Role::Assistant, "hi"), false), ["● hi"]);
        // One blank line after every entry.
        assert_eq!(render(&message(Role::User, "hi"), 80, false, None).len(), 2);
    }

    #[test]
    fn errors_and_declines_take_their_colours() {
        let failed = render(&tool("read_file", status("a.ts", ToolState::Error, Some("not found"))), 80, false, None);
        assert_eq!(failed[0].spans[0].style.fg, Some(theme::ERROR));
        assert_eq!(failed[1].spans[0].style.fg, Some(theme::ERROR));
        let declined = render(&tool("bash", status("rm", ToolState::Declined, Some("declined"))), 80, false, None);
        assert_eq!(declined[1].spans[0].style.fg, Some(theme::WARNING));
        let error = render(&Message { is_error: true, ..message(Role::System, "bad") }, 80, false, None);
        assert_eq!(error[0].spans[0].style.fg, Some(theme::ERROR));
    }

    #[test]
    fn steps_show_only_with_ctrl_o() {
        let m = tool(
            "agent",
            ToolStatus { steps: Some(vec!["read_file a.ts".into()]), ..status("Explore", ToolState::Done, Some("done")) },
        );
        assert!(!rows(80, &m, false).join("\n").contains("read_file a.ts"));
        assert!(rows(80, &m, true).contains(&"    · read_file a.ts".to_string()));
        assert!(has_details(&m));
        assert!(!has_details(&message(Role::User, "x")));
    }

    #[test]
    fn a_notice_is_its_own_block() {
        let notice = Notice {
            title: "Startup".into(),
            tone: NoticeTone::Info,
            items: vec![NoticeItem { text: "one".into(), detail: None }],
        };
        let m = Message { notice: Some(notice), ..message(Role::System, "") };
        assert_eq!(rows(40, &m, false), ["│ Startup", "│ • one"]);
    }

    // Terminal control sequences: every entry kind is cleaned before it's drawn.

    #[test]
    fn no_entry_reaches_the_terminal_with_an_escape_sequence() {
        const EVIL: &str = "\x1b]52;c;aGk=\x07";
        let messages = [
            message(Role::Assistant, &format!("before {EVIL}after")),
            Message {
                tool: Some(ToolStatus {
                    steps: Some(vec![format!("s{EVIL}tep")]),
                    ..status(&format!("a{EVIL}.ts"), ToolState::Done, Some(&format!("1{EVIL} line")))
                }),
                ..message(Role::Tool, &format!("read{EVIL}_file"))
            },
            message(Role::System, &format!("note {EVIL}here")),
            Message { markdown: true, ..message(Role::System, &format!("note {EVIL}here")) },
            message(Role::User, &format!("me {EVIL}too")),
        ];
        for m in messages {
            let text: String = render(&m, 80, true, None).iter().map(|l| l.to_string()).collect();
            assert!(!text.contains('\x1b') && !text.contains('\x07'), "{:?}: {text:?}", m.role);
        }
    }

    // Diffs

    fn edit(count: usize, more: usize) -> Message {
        let lines = (1..=count)
            .map(|i| DiffLine { kind: DiffKind::Add, text: format!("row {i}"), old_line: None, new_line: Some(i) })
            .collect();
        tool(
            "write_file",
            ToolStatus { diff: Some(DiffShown { lines, more }), ..status("a.txt", ToolState::Done, Some("created · 20 lines")) },
        )
    }

    #[test]
    fn collapsed_the_first_15_lines_numbered_then_how_many_more_and_how_to_see_them() {
        let out = rows(80, &edit(20, 0), false).join("\n");
        assert!(out.contains(" 1 + row 1"));
        assert!(out.contains("15 + row 15"));
        assert!(!out.contains("row 16"));
        assert!(out.contains("… 5 more lines (ctrl+o)"));
    }

    #[test]
    fn expanded_ctrl_o_everything_kept_and_what_the_result_left_out() {
        let out = rows(80, &edit(20, 30), true).join("\n");
        assert!(out.contains("20 + row 20"));
        assert!(out.contains("… 30 more lines not kept"));
        assert!(!out.contains("(ctrl+o)"));
    }

    #[test]
    fn the_number_column_fits_every_line_so_expanding_past_line_99_doesnt_shift_the_text() {
        let collapsed = rows(80, &edit(120, 0), false).join("\n");
        let expanded = rows(80, &edit(120, 0), true).join("\n");
        assert!(collapsed.contains("      1 + row 1"));
        assert!(expanded.contains("      1 + row 1"));
        assert!(expanded.contains("    120 + row 120"));
    }

    #[test]
    fn a_malformed_diff_a_hand_edited_session_doesnt_break_the_entry() {
        // Marv filtered bad lines at render time. Here serde reads the session: missing fields default (a line without
        // text, a diff without `more`); a diff serde can't read at all fails the entry's parse, which the engine's
        // loader handles (it drops what it can't read), so it never reaches the view.
        for diff in [r#"{"lines":[],"more":0}"#, r#"{"lines":[{"kind":"add","newLine":2}]}"#] {
            let json = format!(
                r#"{{"id":1,"role":"tool","text":"edit_file","tool":{{"label":"a.txt","status":"done","summary":"+1 −1","diff":{diff}}}}}"#
            );
            let m: Message = serde_json::from_str(&json).unwrap();
            let out = rows(80, &m, false).join("\n");
            assert!(out.contains("edit_file a.txt"), "{diff}: {out}");
            assert!(out.contains("+1 −1"), "{diff}: {out}");
        }
    }

    #[test]
    fn removed_lines_show_their_old_number() {
        let lines = vec![
            DiffLine { kind: DiffKind::Del, text: "old".into(), old_line: Some(7), new_line: None },
            DiffLine { kind: DiffKind::Add, text: "new".into(), old_line: None, new_line: Some(7) },
        ];
        let m = tool(
            "edit_file",
            ToolStatus { diff: Some(DiffShown { lines, more: 0 }), ..status("a.txt", ToolState::Done, Some("+1 −1")) },
        );
        let out = rows(80, &m, false).join("\n");
        assert!(out.contains("7 - old"));
        assert!(out.contains("7 + new"));
    }

    #[test]
    fn a_streaming_reply_lays_out_like_the_finished_one() {
        let text = "Intro with **bold**.\n\n- one\n- two\n\n```rs\nfn main() {}\n```\n\nEnd.";
        let whole = render(&message(Role::Assistant, text), 50, false, None);
        let mut lexer = markdown::StreamLexer::new();
        let mut last = Vec::new();
        for end in (1..=text.len()).step_by(3).chain([text.len()]) {
            last = render(&message(Role::Assistant, &text[..end]), 50, false, Some(&mut lexer));
        }
        assert_eq!(last, whole);
    }
}
