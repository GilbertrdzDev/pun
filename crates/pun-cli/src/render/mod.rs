mod check;
mod look;
mod spinner;
#[cfg(test)]
mod tests;

pub(crate) use check::{CheckItem, StepLine};
pub(crate) use look::{ColorChoice, Look, Role, Step};
#[cfg(test)]
pub(crate) use look::{Depth, Theme, nearest_256};
pub(crate) use spinner::Spinner;

use pun_lint::problem::{Level, LineIndex, Problem, Safety};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PATH_COLUMN: usize = 27;
const LABEL_COLUMN: usize = 77;
const MAX_SPAN_LINES: usize = 3;
const INDENT: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileStatus {
    Fixed,
    NeedsFix,
    Unchanged,
}

#[derive(Clone, Debug)]
pub(crate) struct FileLine {
    pub(crate) path: PathBuf,
    pub(crate) status: FileStatus,
    pub(crate) rules: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Summary<'a> {
    pub(crate) errors: usize,
    pub(crate) warnings: usize,
    pub(crate) fixable: usize,
    pub(crate) fix_command: &'a str,
    pub(crate) need_formatting: usize,
    pub(crate) files: usize,
    pub(crate) elapsed: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct FormatSummary {
    pub(crate) formatted: usize,
    pub(crate) unchanged: usize,
    pub(crate) errors: usize,
    pub(crate) files: usize,
    pub(crate) test: bool,
    pub(crate) elapsed: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct RunError {
    pub(crate) path: Option<PathBuf>,
    pub(crate) line: Option<usize>,
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Renderer {
    pub(crate) look: Look,
    pub(crate) concise: bool,
    root: PathBuf,
}

impl Renderer {
    pub(crate) fn new(look: Look, root: impl Into<PathBuf>) -> Renderer {
        Renderer {
            look,
            concise: false,
            root: root.into(),
        }
    }

    pub(crate) fn file_lines(&self, files: &[FileLine]) -> Vec<String> {
        let width = files
            .iter()
            .map(|f| f.path.display().to_string().chars().count() + 2)
            .fold(PATH_COLUMN, usize::max);
        files.iter().map(|f| self.file_line(f, width)).collect()
    }

    fn file_line(&self, file: &FileLine, width: usize) -> String {
        let (glyph, role) = match file.status {
            FileStatus::Fixed => (self.look.glyph("✔", "ok"), Role::Ok),
            FileStatus::NeedsFix => (self.look.glyph("✖", "x"), Role::Error),
            FileStatus::Unchanged => (self.look.glyph("✔", "ok"), Role::Dim),
        };
        let shown = file.path.display().to_string();
        let mut path = self.path_link(&file.path, &shown);
        if file.status == FileStatus::Unchanged {
            path = self.look.paint(Role::Dim, &path);
        }
        let mut out = format!(" {} {path}", self.look.paint(role, glyph));
        if !file.rules.is_empty() {
            let rules: Vec<String> = file.rules.iter().map(|r| self.rule(r)).collect();
            let pad = width - shown.chars().count();
            let _ = write!(out, "{}{}", " ".repeat(pad), rules.join(", "));
        }
        out.push('\n');
        out
    }

    pub(crate) fn diff(&self, unified: &str) -> String {
        enum Row {
            Gap,
            Line(Option<usize>, Option<usize>, char, String),
        }
        let mut rows = vec![];
        let (mut old, mut new, mut hunks) = (0, 0, 0);
        for line in unified.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(header) = line.strip_prefix("@@ -") {
                let start = |s: &str| s.split([',', ' ']).next().and_then(|n| n.parse().ok());
                old = start(header).unwrap_or(1);
                new = header
                    .split_once(" +")
                    .and_then(|(_, n)| start(n))
                    .unwrap_or(1);
                if hunks > 0 {
                    rows.push(Row::Gap);
                }
                hunks += 1;
                continue;
            }
            if hunks == 0 {
                continue;
            }
            let (sign, text) = match line.chars().next() {
                Some(c @ ('+' | '-' | ' ')) => (c, &line[1..]),
                _ => continue,
            };
            let (o, n) = match sign {
                '-' => (Some(old), None),
                '+' => (None, Some(new)),
                _ => (Some(old), Some(new)),
            };
            old += usize::from(sign != '+');
            new += usize::from(sign != '-');
            rows.push(Row::Line(o, n, sign, visible(text)));
        }
        let width = old.max(new).saturating_sub(1).max(1).to_string().len();
        let number = |n: Option<usize>| match n {
            Some(n) => format!("{n:>width$}"),
            None => " ".repeat(width),
        };
        let bar = self.look.paint(Role::Dim, self.look.glyph("│", "|"));
        let mut out = String::new();
        for row in rows {
            let Row::Line(o, n, sign, text) = row else {
                let gap = self.look.glyph("…", "...");
                let _ = writeln!(out, "   {}", self.look.paint(Role::Dim, gap));
                continue;
            };
            let numbers = self
                .look
                .paint(Role::Dim, &format!("{} {}", number(o), number(n)));
            let code = match sign {
                '-' => self.look.paint(Role::Error, &format!("-{text}")),
                '+' => self.look.paint(Role::Ok, &format!("+{text}")),
                _ if text.is_empty() => String::new(),
                _ => format!(" {}", self.look.paint(Role::Dim, &text)),
            };
            let space = if code.is_empty() { "" } else { " " };
            let _ = writeln!(out, "   {numbers} {bar}{space}{code}");
        }
        out
    }

    pub(crate) fn problems<'a>(
        &self,
        problems: impl IntoIterator<Item = (&'a Problem, &'a str, &'a LineIndex)>,
    ) -> String {
        let blocks: Vec<String> = problems
            .into_iter()
            .map(|(p, source, index)| self.problem(p, source, index))
            .collect();
        blocks.join(if self.concise { "" } else { "\n" })
    }

    pub(crate) fn problem(&self, problem: &Problem, source: &str, index: &LineIndex) -> String {
        let (glyph, role) = level(self.look, problem.level);
        let at = index.line_col(problem.span.start);
        let location = format!("{}:{}:{}", problem.path.display(), at.line, at.column);
        let label = problem.fix.as_ref().map(|fix| match fix.safety {
            Safety::Safe => "fixable",
            Safety::Unsafe => "fixable (unsafe)",
        });
        let glyph = self.look.paint(role, glyph);
        let rule = self.rule(&problem.rule);
        let message = visible(&problem.message);
        if self.concise {
            let location = self.path_link(&problem.path, &location);
            let label = label.map_or(String::new(), |l| {
                format!("  {}", self.look.paint(Role::Dim, l))
            });
            return format!(" {glyph} {location}  {rule}  {message}{label}\n");
        }
        let mut out = format!(" {glyph} {rule}  {message}");
        if let Some(label) = label {
            let used = 5 + problem.rule.chars().count() + problem.message.chars().count();
            let pad = LABEL_COLUMN.saturating_sub(used).max(2);
            let _ = write!(
                out,
                "{}{}",
                " ".repeat(pad),
                self.look.paint(Role::Dim, label)
            );
        }
        out.push('\n');
        out.push_str(&self.frame(problem, &location, source, index, role));
        out
    }

    fn frame(
        &self,
        problem: &Problem,
        location: &str,
        source: &str,
        index: &LineIndex,
        role: Role,
    ) -> String {
        let span = problem.span;
        let first = index.line_col(span.start).line;
        let last = index
            .line_col(span.end.max(span.start + 1) - 1)
            .line
            .max(first);
        let text = |line: usize| {
            let r = index.line_range(line).expect("a line of the span");
            (r.start, &source[r])
        };
        let mut lines: Vec<Option<usize>> = Vec::new();
        if first > 1 && !text(first - 1).1.trim().is_empty() {
            lines.push(Some(first - 1));
        }
        if last - first + 1 > MAX_SPAN_LINES {
            lines.extend([Some(first), Some(first + 1), None, Some(last)]);
        } else {
            lines.extend((first..=last).map(Some));
        }
        let strip = lines
            .iter()
            .flatten()
            .map(|&l| text(l).1)
            .filter(|t| !t.trim().is_empty())
            .map(|t| columns(&t[..t.len() - t.trim_start_matches([' ', '\t']).len()]))
            .min()
            .unwrap_or(0)
            .saturating_sub(INDENT)
            .min({
                let (start, code) = text(first);
                columns(&code[..(span.start - start).min(code.len())])
            });
        let width = last.to_string().len();
        let bar = self.look.paint(Role::Dim, self.look.glyph("│", "|"));
        let mut out = format!(
            " {:width$}{} {}\n",
            "",
            self.look.paint(Role::Dim, self.look.glyph("╭─", ",-")),
            self.path_link(&problem.path, location),
        );
        for line in lines {
            let Some(line) = line else {
                let more = self.look.glyph("…", "...");
                let _ = writeln!(
                    out,
                    " {}",
                    self.look.paint(Role::Dim, &format!("{more:>width$}"))
                );
                continue;
            };
            let (start, code) = text(line);
            let shown: String = visible(&expand(code)).chars().skip(strip).collect();
            let number = self.look.paint(Role::Dim, &format!("{line:>width$}"));
            out.push_str(format!(" {number}{bar} {shown}").trim_end());
            out.push('\n');
            if (first..=last).contains(&line) && (first == last || !code.trim().is_empty()) {
                let indent = code.len() - code.trim_start().len();
                let from = if line == first {
                    (span.start - start).min(code.len())
                } else {
                    indent
                };
                let to = if line == last {
                    (span.end - start).min(code.len())
                } else {
                    code.len()
                };
                let pad = columns(&code[..from]) - strip;
                let marks = columns(&code[from..to.max(from)]).max(1);
                let marker = self.look.glyph("━", "^").repeat(marks);
                let _ = writeln!(
                    out,
                    " {:width$}{bar} {}{}",
                    "",
                    " ".repeat(pad),
                    self.look.paint(role, &marker)
                );
            }
        }
        let corner = self.look.paint(Role::Dim, self.look.glyph("╰─", "`-"));
        let _ = match &problem.help {
            Some(help) => writeln!(out, " {:width$}{corner} help: {help}", ""),
            None => writeln!(out, " {:width$}{corner}", ""),
        };
        out
    }

    pub(crate) fn summary(&self, s: &Summary) -> String {
        let mut parts = Vec::new();
        if s.errors > 0 {
            parts.push(
                self.look
                    .paint(Role::Error, &plural(s.errors, "error", "errors")),
            );
        }
        if s.warnings > 0 {
            parts.push(
                self.look
                    .paint(Role::Warning, &plural(s.warnings, "warning", "warnings")),
            );
        }
        if s.fixable > 0 {
            parts.push(format!("{} fixable (run {})", s.fixable, s.fix_command));
        }
        if s.need_formatting > 0 {
            parts.push(plural(
                s.need_formatting,
                "file needs formatting",
                "files need formatting",
            ));
        }
        let (glyph, role) = if s.errors > 0 || s.need_formatting > 0 {
            (self.look.glyph("✖", "x"), Role::Error)
        } else if s.warnings > 0 {
            (self.look.glyph("⚠", "!"), Role::Warning)
        } else {
            parts.push(self.look.paint(Role::Ok, "No problems"));
            (self.look.glyph("✔", "ok"), Role::Ok)
        };
        let files = plural(s.files, "file", "files");
        parts.push(self.look.paint(
            Role::Dim,
            &format!("{files} in {:.2}s", s.elapsed.as_secs_f64()),
        ));
        let separator = self.look.paint(Role::Dim, self.look.glyph("·", "-"));
        format!(
            "{} {}\n",
            self.look.paint(role, glyph),
            parts.join(&format!(" {separator} "))
        )
    }

    pub(crate) fn format_summary(&self, s: &FormatSummary) -> String {
        let time = format!("{:.2}s", s.elapsed.as_secs_f64());
        let files = plural(s.files, "file", "files");
        let mut parts = vec![];
        let mut timing = format!("{files} in {time}");
        if !s.test {
            parts.push(format!(
                "Formatted {}, {} unchanged",
                plural(s.formatted, "file", "files"),
                s.unchanged
            ));
        } else if s.formatted > 0 {
            let need = if s.formatted == 1 { "needs" } else { "need" };
            parts.push(format!("{} of {files} {need} formatting", s.formatted));
            timing = time;
        } else if s.errors == 0 {
            parts.push(format!("All {files} formatted"));
            timing = time;
        }
        if s.errors > 0 {
            parts.push(
                self.look
                    .paint(Role::Error, &plural(s.errors, "error", "errors")),
            );
        }
        if s.test && s.formatted > 0 {
            parts.push("run `pun format` to fix".into());
        }
        parts.push(self.look.paint(Role::Dim, &timing));
        let separator = self.look.paint(Role::Dim, self.look.glyph("·", "-"));
        format!("{}\n", parts.join(&format!(" {separator} ")))
    }

    pub(crate) fn done(&self, message: &str) -> String {
        let glyph = self.look.paint(Role::Ok, self.look.glyph("✔", "ok"));
        format!("{glyph} {message}\n")
    }

    pub(crate) fn run_error(&self, e: &RunError) -> String {
        let glyph = self.look.glyph("✖", "x");
        let line = match &e.path {
            Some(path) => {
                let shown = match e.line {
                    Some(line) => format!("{}:{line}", path.display()),
                    None => path.display().to_string(),
                };
                format!("{glyph} {}  {}", self.path_link(path, &shown), e.message)
            }
            None => format!("{glyph} {}", e.message),
        };
        let mut out = format!("{}\n", self.look.paint(Role::Error, &line));
        if let Some(hint) = &e.hint {
            for line in hint.lines() {
                let _ = writeln!(out, "  {line}");
            }
        }
        out
    }

    pub(crate) fn rule(&self, id: &str) -> String {
        let painted = self.look.paint(Role::Rule, id);
        if !self.look.links || pun_config::rule(id).is_none() {
            return painted;
        }
        (self.look).link(&pun_config::rule_docs_url(id), &painted)
    }

    pub(crate) fn path_link(&self, path: &Path, shown: &str) -> String {
        self.look
            .link(&file_url(&self.root.join(path)), &visible(shown))
    }
}

fn level(look: Look, level: Level) -> (&'static str, Role) {
    match level {
        Level::Error => (look.glyph("✖", "x"), Role::Error),
        Level::Warning => (look.glyph("⚠", "!"), Role::Warning),
    }
}

/// `text` with each control character a terminal or a CI log would act on (an escape sequence, a line break that
/// starts a workflow command) shown as a visible character instead, one for one, so columns still line up.
pub(crate) fn visible(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => c,
            '\0'..='\x1f' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{FFFD}'),
            '\x7f' => '\u{2421}',
            '\u{80}'..='\u{9f}' => '\u{FFFD}',
            c => c,
        })
        .collect()
}

fn expand(text: &str) -> String {
    text.replace('\t', &" ".repeat(INDENT))
}

pub(crate) fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn columns(text: &str) -> usize {
    text.chars()
        .map(|c| if c == '\t' { INDENT } else { 1 })
        .sum()
}

pub(crate) fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    // File URLs use `/`. A Windows drive (`C:/...`) needs the extra slash of `file:///C:/...`.
    let mut text = path.to_string_lossy().replace('\\', "/");
    if text.as_bytes().get(1) == Some(&b':') {
        text.insert(0, '/');
    }
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            url.push(b as char);
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}
