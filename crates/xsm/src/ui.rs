//! Presentation helpers: colour detection, semantic styles, plain-text tables and rules.
//!
//! Colour is used only when stdout is a terminal and nothing disabled it (`--plain`,
//! `--no-color`, `NO_COLOR`, `XSM_NO_COLOR`, `TERM=dumb`). Unicode box-drawing is used only on a
//! terminal without `--plain`; everything else is deterministic ASCII.

use is_terminal::IsTerminal;
use owo_colors::OwoColorize;

/// Semantic role of a piece of text; mapped to a colour by [`Ui::paint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Success.
    Ok,
    /// Failure.
    Err,
    /// Warning.
    Warn,
    /// Labels and keys.
    Key,
    /// De-emphasised text.
    Muted,
    /// Event names and highlights.
    Accent,
    /// Headings.
    Title,
}

/// Terminal capabilities and verbosity, decided once at start-up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ui {
    color: bool,
    unicode: bool,
    verbose: bool,
}

/// `true` when `name` is set to a non-empty value.
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

impl Ui {
    /// Detect capabilities from the flags, the environment and whether stdout is a terminal.
    pub(crate) fn detect(plain: bool, no_color: bool, verbose: bool) -> Self {
        let tty = std::io::stdout().is_terminal();
        let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
        let color = tty
            && !plain
            && !no_color
            && !dumb
            && !env_flag("NO_COLOR")
            && !env_flag("XSM_NO_COLOR");
        Self { color, unicode: tty && !plain && !dumb, verbose }
    }

    /// Plain output: no colour, ASCII only.
    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Self { color: false, unicode: false, verbose: false }
    }

    /// Whether Unicode glyphs may be printed.
    pub(crate) fn unicode(self) -> bool {
        self.unicode
    }

    /// Print a diagnostic line on stderr when `--verbose` is on.
    pub(crate) fn verbose(self, message: &str) {
        if self.verbose {
            eprintln!("xsm: {message}");
        }
    }

    /// Wrap `text` in the colour of `role` (no-op when colour is off).
    pub(crate) fn paint(self, role: Role, text: &str) -> String {
        if !self.color {
            return text.to_owned();
        }
        match role {
            Role::Ok => text.green().to_string(),
            Role::Err => text.red().bold().to_string(),
            Role::Warn => text.yellow().to_string(),
            Role::Key => text.cyan().to_string(),
            Role::Muted => text.dimmed().to_string(),
            Role::Accent => text.blue().to_string(),
            Role::Title => text.bold().to_string(),
        }
    }

    /// A section rule: `== title ==` (or box-drawing on a terminal).
    pub(crate) fn rule(self, title: &str) -> String {
        let bar = if self.unicode { "──" } else { "--" };
        self.paint(Role::Title, &format!("{bar} {title} {bar}"))
    }

    /// Aligned `key  value` lines; keys are painted, values are printed as given.
    pub(crate) fn key_values(self, rows: &[(&str, String)]) -> Vec<String> {
        let width = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        rows.iter()
            .map(|(k, v)| {
                let pad = " ".repeat(width - k.chars().count());
                format!("{}{pad}  {v}", self.paint(Role::Key, k))
            })
            .collect()
    }
}

/// Column alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    /// Pad on the right.
    Left,
    /// Pad on the left (numbers).
    Right,
}

/// One table cell: text plus an optional colour role.
#[derive(Debug, Clone)]
pub(crate) struct Cell {
    text: String,
    role: Option<Role>,
}

impl Cell {
    /// A cell with no colour.
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self { text: sanitize(&text.into()), role: None }
    }

    /// A coloured cell.
    pub(crate) fn styled(role: Role, text: impl Into<String>) -> Self {
        Self { text: sanitize(&text.into()), role: Some(role) }
    }
}

/// Escape control characters so a value can never break a row apart or forge another one.
pub(crate) fn sanitize(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_owned();
    }
    text.chars().flat_map(|c| if c.is_control() { escape(c) } else { vec![c] }).collect()
}

fn escape(c: char) -> Vec<char> {
    c.escape_default().collect()
}

/// A borderless table: header, rule, rows; columns separated by two spaces.
#[derive(Debug)]
pub(crate) struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    /// A table with the given column headers, all left-aligned.
    pub(crate) fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            aligns: vec![Align::Left; headers.len()],
            rows: Vec::new(),
        }
    }

    /// Right-align column `index` (numbers).
    #[must_use]
    pub(crate) fn right(mut self, index: usize) -> Self {
        if let Some(a) = self.aligns.get_mut(index) {
            *a = Align::Right;
        }
        self
    }

    /// Append a row; missing cells are blank, extra cells are dropped.
    pub(crate) fn add(&mut self, mut row: Vec<Cell>) {
        row.resize_with(self.headers.len(), || Cell::text(""));
        self.rows.push(row);
    }

    /// Render to lines, without trailing whitespace.
    pub(crate) fn render(&self, ui: Ui) -> Vec<String> {
        let widths: Vec<usize> = (0..self.headers.len())
            .map(|i| {
                self.rows
                    .iter()
                    .map(|r| r[i].text.chars().count())
                    .chain(std::iter::once(self.headers[i].chars().count()))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let dash = if ui.unicode() { "─" } else { "-" };
        let mut lines = Vec::with_capacity(self.rows.len() + 2);
        let header: Vec<Cell> =
            self.headers.iter().map(|h| Cell::styled(Role::Title, h.clone())).collect();
        lines.push(self.line(ui, &header, &widths));
        lines.push(widths.iter().map(|w| dash.repeat(*w)).collect::<Vec<_>>().join("  "));
        for row in &self.rows {
            lines.push(self.line(ui, row, &widths));
        }
        lines
    }

    fn line(&self, ui: Ui, cells: &[Cell], widths: &[usize]) -> String {
        let mut parts = Vec::with_capacity(cells.len());
        for (i, cell) in cells.iter().enumerate() {
            let pad = widths[i].saturating_sub(cell.text.chars().count());
            let painted = cell.role.map_or_else(|| cell.text.clone(), |r| ui.paint(r, &cell.text));
            parts.push(match self.aligns[i] {
                Align::Left => format!("{painted}{}", " ".repeat(pad)),
                Align::Right => format!("{}{painted}", " ".repeat(pad)),
            });
        }
        parts.join("  ").trim_end().to_owned()
    }
}

/// Print each line on stdout.
pub(crate) fn print_lines(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns_and_trims() {
        let ui = Ui::plain();
        let mut table = Table::new(&["Key", "N"]).right(1);
        table.add(vec![Cell::text("alpha"), Cell::text("7")]);
        table.add(vec![Cell::text("b"), Cell::text("120")]);
        let lines = table.render(ui);
        assert_eq!(lines[0], "Key      N");
        assert_eq!(lines[2], "alpha    7");
        assert_eq!(lines[3], "b      120");
    }

    #[test]
    fn sanitize_escapes_control_characters() {
        assert_eq!(sanitize("a\nb"), "a\\nb");
        assert_eq!(sanitize("plain"), "plain");
    }
}
