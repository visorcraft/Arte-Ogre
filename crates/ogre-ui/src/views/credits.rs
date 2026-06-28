// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! The Credits page: third-party dependency acknowledgements.
use crate::state::AppState;
use egui::RichText;

const CREDITS_TEXT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CREDITS.md"));

/// Render the Credits page.
pub fn render(ui: &mut egui::Ui, state: &mut AppState) {
    let palette = crate::theme::resolve(state.preferences.theme, ui.ctx());
    super::page_header(ui, "Credits", &mut state.view);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(crate::theme::SPACE_M);
            super::card(
                ui,
                &palette,
                "Acknowledgements",
                "The open-source projects and assets Arte Ogre is built on.",
                |ui| render_markdown(ui, CREDITS_TEXT),
            );
        });
}

/// Render the small, known subset of markdown `cargo-about` emits: `##`
/// headings, pipe tables (with clickable links), and prose. The top-level `#`
/// title is dropped — the page header already names the surface.
fn render_markdown(ui: &mut egui::Ui, text: &str) {
    let mut lines = text.lines().peekable();
    let mut table_idx = 0usize;
    while let Some(line) = lines.next() {
        let line = line.trim_end();
        if let Some(h) = line.strip_prefix("## ") {
            ui.add_space(crate::theme::SPACE_M);
            ui.label(
                RichText::new(plain(h))
                    .size(crate::theme::TEXT_SUBHEADING)
                    .strong(),
            );
            ui.add_space(crate::theme::SPACE_XS);
        } else if line.starts_with("# ") {
            // page header already shows the title — skip the document's own h1
        } else if line.starts_with('|') {
            let mut rows = vec![line];
            while let Some(next) = lines.peek() {
                if next.trim_start().starts_with('|') {
                    rows.push(lines.next().unwrap().trim_end());
                } else {
                    break;
                }
            }
            render_table(ui, &rows, table_idx);
            table_idx += 1;
        } else if line.is_empty() {
            ui.add_space(crate::theme::SPACE_S);
        } else {
            ui.label(plain(line));
        }
    }
}

/// Render a markdown pipe table as a striped grid; the first non-separator row
/// is the bold header.
fn render_table(ui: &mut egui::Ui, rows: &[&str], idx: usize) {
    let parsed: Vec<Vec<&str>> = rows.iter().map(|r| cells(r)).collect();
    let cols = parsed.iter().map(Vec::len).max().unwrap_or(0);
    egui::Grid::new(("credits_table", idx))
        .striped(true)
        .num_columns(cols)
        .show(ui, |ui| {
            let mut header_done = false;
            for row in &parsed {
                if is_separator(row) {
                    continue;
                }
                if header_done {
                    for cell in row {
                        render_cell(ui, cell);
                    }
                } else {
                    for cell in row {
                        ui.label(RichText::new(plain(cell)).strong());
                    }
                    header_done = true;
                }
                ui.end_row();
            }
        });
}

/// Render one table cell: a lone `[label](url)` becomes a clickable link
/// (scheme stripped for display); anything else is plain text.
fn render_cell(ui: &mut egui::Ui, cell: &str) {
    if let Some((_, url)) = parse_link(cell) {
        ui.hyperlink_to(short_url(url), url);
    } else {
        ui.label(plain(cell));
    }
}

/// Split a pipe-table row into trimmed cells, dropping the leading/trailing `|`.
fn cells(row: &str) -> Vec<&str> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = row.strip_suffix('|').unwrap_or(row);
    row.split('|').map(str::trim).collect()
}

/// A table's `|---|---|` separator: every cell is only dashes/colons.
fn is_separator(row: &[&str]) -> bool {
    row.iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':')))
}

/// Parse a string that is exactly `[label](url)` into `(label, url)`.
fn parse_link(s: &str) -> Option<(&str, &str)> {
    let s = s.trim().strip_prefix('[')?;
    let close = s.find("](")?;
    let url = s[close + 2..].strip_suffix(')')?;
    Some((&s[..close], url))
}

/// Drop the scheme and trailing slash for a compact, still-clickable link label.
fn short_url(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// Flatten inline markdown for display: `[label](url)` → `label`, drop backticks.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(close) = after.find("](") {
            if let Some(end) = after[close + 2..].find(')') {
                out.push_str(&after[1..close]); // the label
                rest = &after[close + 2 + end + 1..];
                continue;
            }
        }
        out.push('[');
        rest = &after[1..];
    }
    out.push_str(rest);
    out.replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_link_extracts_label_and_url() {
        assert_eq!(
            parse_link("[https://example.com/x](https://example.com/x)"),
            Some(("https://example.com/x", "https://example.com/x"))
        );
        assert_eq!(parse_link("Apache-2.0"), None);
    }

    #[test]
    fn plain_collapses_links_and_drops_backticks() {
        assert_eq!(
            plain("using [`cargo-about`](https://x)."),
            "using cargo-about."
        );
        assert_eq!(plain("plain text"), "plain text");
    }

    #[test]
    fn separator_and_cells_parse() {
        assert!(is_separator(&["---", ":--", "--:"]));
        assert!(!is_separator(&["ab_glyph", "0.2"]));
        assert_eq!(cells("| a | b | c |"), vec!["a", "b", "c"]);
    }

    #[test]
    fn short_url_strips_scheme() {
        assert_eq!(short_url("https://github.com/a/b/"), "github.com/a/b");
    }
}
