use crate::error::{Error, Result, bail};
use pcre2::bytes::{Regex, RegexBuilder};
use pun_tokens::{Kind, tokenize};
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

fn re(pattern: &str) -> Regex {
    RegexBuilder::new()
        .jit_if_available(true)
        .build(pattern)
        .expect("valid pattern")
}

const TYPES: [&str; 4] = ["class", "interface", "trait", "enum"];

static TYPE_AT: LazyLock<Vec<(u8, &str, Regex)>> = LazyLock::new(|| {
    (TYPES.iter())
        .map(|t| {
            let pattern = format!(
                r"(?is)\G.\b(?<![\$:>]){t}\s++[a-zA-Z_\x7f-\xff:][a-zA-Z0-9_\x7f-\xff:\-]*+"
            );
            (t.as_bytes()[0], *t, re(&pattern))
        })
        .collect()
});
static CANDIDATES: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(?:class|interface|trait|enum)\s"));
static HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"\G<<<[ \t]*+(['"]?)([a-zA-Z_\x80-\xff][a-zA-Z0-9_\x80-\xff]*+)\1(?:\r\n|\n|\r)"#)
});
static DECLARATIONS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?ix)
        (?:
             \b(?<![\\$:>])(?P<type>class|interface|trait|enum) \s++ (?P<name>[a-zA-Z_\x7f-\xff:][a-zA-Z0-9_\x7f-\xff:\-]*+)
           | \b(?<![\\$:>])(?P<ns>namespace) (?P<nsname>\s++[a-zA-Z_\x7f-\xff][a-zA-Z0-9_\x7f-\xff]*+(?:\s*+\\\s*+[a-zA-Z_\x7f-\xff][a-zA-Z0-9_\x7f-\xff]*+)*+)? \s*+ [\{;]
        )")
});

#[expect(
    clippy::too_many_lines,
    reason = "long; to be split (code audit: functions over 100 lines)"
)]
fn clean(c: &[u8], max_matches: usize, raw: bool) -> Vec<u8> {
    const REJECT: [bool; 256] = {
        let mut t = [false; 256];
        let set = b"?\"'`</#citre";
        let mut i = 0;
        while i < set.len() {
            t[set[i] as usize] = true;
            i += 1;
        }
        t
    };
    let len = c.len();
    let peek = |i: usize, ch: u8| i + 1 < len && c[i + 1] == ch;
    let mut out = Vec::with_capacity(len);
    let mut i = 0;
    let to_newline = |i: &mut usize, php_comment: bool| loop {
        let stop: &[u8] = if php_comment { b"\r\n?" } else { b"\r\n" };
        *i += c[*i..]
            .iter()
            .position(|b| stop.contains(b))
            .unwrap_or(len - *i);
        if !php_comment || *i >= len || c[*i] != b'?' || peek(*i, b'>') {
            return;
        }
        *i += 1;
    };
    while i < len {
        while i < len {
            if c[i] == b'<' && peek(i, b'?') {
                i += 2;
                break;
            }
            i += 1;
        }
        out.extend_from_slice(b"<?");
        while i < len {
            let ch = c[i];
            if ch == b'?' && peek(i, b'>') {
                out.extend_from_slice(b"?>");
                i += 2;
                break;
            }
            if ch == b'"' || ch == b'\'' || ch == b'`' {
                i += 1;
                while i < len {
                    i += c[i..]
                        .iter()
                        .position(|b| *b == b'\\' || *b == ch)
                        .unwrap_or(len - i);
                    if i >= len {
                        break;
                    }
                    if c[i] == b'\\' && (peek(i, b'\\') || peek(i, ch)) {
                        i += 2;
                        continue;
                    }
                    if c[i] == ch {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(b"null");
                continue;
            }
            if ch == b'<'
                && peek(i, b'<')
                && let Some((end, label)) = heredoc_at(c, i)
            {
                i = end;
                skip_heredoc(c, &mut i, &label, &to_newline);
                out.extend_from_slice(b"null");
                continue;
            }
            if ch == b'#' && raw && !peek(i, b'[') {
                to_newline(&mut i, true);
                continue;
            }
            if ch == b'/' {
                if peek(i, b'/') {
                    to_newline(&mut i, raw);
                    continue;
                }
                if peek(i, b'*') {
                    i += 2;
                    while i < len {
                        i += c[i..].iter().position(|b| *b == b'*').unwrap_or(len - i);
                        if peek(i, b'/') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
            }
            if max_matches == 1
                && let Some((_, t, pattern)) = TYPE_AT.iter().find(|(first, ..)| *first == ch)
                && c[i..].starts_with(t.as_bytes())
                && let Ok(Some(m)) = pattern.find_at(c, i - 1)
            {
                out.extend_from_slice(&c[m.start()..m.end()]);
                return out;
            }
            i += 1;
            let skip = c[i..]
                .iter()
                .position(|b| REJECT[*b as usize])
                .unwrap_or(len - i);
            out.push(ch);
            out.extend_from_slice(&c[i..i + skip]);
            i += skip;
        }
    }
    out
}

fn heredoc_at(c: &[u8], i: usize) -> Option<(usize, Vec<u8>)> {
    let mut locs = HEREDOC.capture_locations();
    let found = HEREDOC.captures_read_at(&mut locs, c, i).ok()??;
    let (start, end) = locs.get(2)?;
    Some((found.end(), c[start..end].to_vec()))
}

fn skip_heredoc(c: &[u8], i: &mut usize, label: &[u8], to_newline: &dyn Fn(&mut usize, bool)) {
    let len = c.len();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    while *i < len {
        match c[*i] {
            b'\t' | b' ' => {
                *i += 1;
                continue;
            }
            b if b == label[0]
                && c[*i..].starts_with(label)
                && !c.get(*i + label.len()).is_some_and(|b| word(*b)) =>
            {
                *i += label.len();
                return;
            }
            _ => {}
        }
        to_newline(i, false);
        *i += c[*i..]
            .iter()
            .take_while(|b| **b == b'\r' || **b == b'\n')
            .count();
    }
}

fn strip_whitespace(code: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(code.len());
    let mut space = false;
    for t in tokenize(code) {
        match t.kind {
            Some(Kind::T_WHITESPACE) => {
                if !space {
                    out.push(b' ');
                    space = true;
                }
            }
            Some(Kind::T_COMMENT | Kind::T_DOC_COMMENT) => {}
            _ => {
                out.extend_from_slice(t.text);
                space = false;
            }
        }
    }
    out
}

/// PHP reads a class name as bytes, so `class ©` in a Latin-1 file names the single byte 0xA9, and Composer writes
/// that byte into the class map. `lossless` keeps each byte that is not UTF-8 (always 0x80 or above) as the code
/// point U+10FF00 + byte, and `restore` gives the bytes back when a file is written. A real U+10FFxx in the text is
/// kept as its own UTF-8 bytes, and `restore` never makes a byte below 0x80, so a name or path can't turn into a
/// quote or a backslash after the PHP around it was escaped.
const RAW: u32 = 0x10_FF00;

fn raw(c: char) -> bool {
    u32::from(c) >= RAW
}

pub fn lossless(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(s) = std::str::from_utf8(bytes)
        && !s.chars().any(raw)
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(bytes.len() * 2);
    let push_raw = |out: &mut String, bytes: &[u8]| {
        out.extend((bytes.iter()).filter_map(|&b| char::from_u32(RAW + u32::from(b))));
    };
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if raw(c) {
                push_raw(&mut out, c.encode_utf8(&mut [0; 4]).as_bytes());
            } else {
                out.push(c);
            }
        }
        push_raw(&mut out, chunk.invalid());
    }
    Cow::Owned(out)
}

pub fn restore(s: &str) -> Cow<'_, [u8]> {
    if !s.chars().any(raw) {
        return Cow::Borrowed(s.as_bytes());
    }
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match (u32::from(c).checked_sub(RAW)).and_then(|b| u8::try_from(b).ok()) {
            Some(b) if b >= 0x80 => out.push(b),
            _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    Cow::Owned(out)
}

pub fn find_classes(code: &[u8]) -> Vec<String> {
    let (contents, raw) = if code.contains(&b'`') {
        (strip_whitespace(code), false)
    } else {
        (code.to_vec(), true)
    };
    let candidates = CANDIDATES.find_iter(&contents).count();
    if candidates == 0 {
        return vec![];
    }
    let cleaned = clean(&contents, candidates, raw);
    let text = |b: &[u8]| lossless(b).into_owned();
    let mut classes = vec![];
    let mut namespace = String::new();
    let mut locs = DECLARATIONS.capture_locations();
    let (ty, name, ns, nsname) = (
        group(&DECLARATIONS, "type"),
        group(&DECLARATIONS, "name"),
        group(&DECLARATIONS, "ns"),
        group(&DECLARATIONS, "nsname"),
    );
    let mut at = 0;
    while let Ok(Some(m)) = DECLARATIONS.captures_read_at(&mut locs, &cleaned, at) {
        at = m.end();
        let get = |g: usize| locs.get(g).map(|(s, e)| &cleaned[s..e]);
        if get(ns).is_some() {
            let declared: Vec<u8> = get(nsname)
                .unwrap_or(b"")
                .iter()
                .copied()
                .filter(|b| !b" \t\r\n".contains(b))
                .collect();
            namespace = format!("{}\\", text(&declared));
            continue;
        }
        let mut class = text(get(name).unwrap_or(b""));
        if class == "extends" || class == "implements" {
            continue;
        }
        if class.starts_with(':') {
            class = format!("xhp{}", &class.replace('-', "_").replace(':', "__")[1..]);
        } else if get(ty).is_some_and(|t| t.eq_ignore_ascii_case(b"enum"))
            && let Some(colon) = class.rfind(':')
        {
            class.truncate(colon);
        }
        classes.push(
            format!("{namespace}{class}")
                .trim_start_matches('\\')
                .to_string(),
        );
    }
    classes
}

fn group(re: &Regex, name: &str) -> usize {
    (re.capture_names().iter())
        .position(|n| n.as_deref() == Some(name))
        .expect("named group")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Classmap,
    Psr0,
    Psr4,
}

#[derive(Debug, Default)]
pub struct ClassMap {
    pub map: Vec<(String, String)>,
    index: HashMap<String, usize>,
    pub ambiguous: Vec<(String, Vec<String>)>,
    /// Each class's place in `ambiguous`, so a new conflict doesn't search the ones before it.
    ambiguous_index: HashMap<String, usize>,
    pub violations: Vec<(String, String)>,
    scanned: HashSet<PathBuf>,
}

impl ClassMap {
    pub fn add(&mut self, class: &str, path: &str) {
        match self.index.get(class) {
            None => {
                self.index.insert(class.to_string(), self.map.len());
                self.map.push((class.to_string(), path.to_string()));
            }
            Some(&i) if self.map[i].1 != path => {
                if let Some(&a) = self.ambiguous_index.get(class) {
                    self.ambiguous[a].1.push(path.to_string());
                } else {
                    (self.ambiguous_index).insert(class.to_string(), self.ambiguous.len());
                    (self.ambiguous).push((class.to_string(), vec![path.to_string()]));
                }
            }
            Some(_) => {}
        }
    }

    pub fn set(&mut self, class: &str, path: &str) {
        match self.index.get(class) {
            Some(&i) => self.map[i].1 = path.to_string(),
            None => self.add(class, path),
        }
    }

    pub fn path_of(&self, class: &str) -> Option<&str> {
        self.index.get(class).map(|&i| self.map[i].1.as_str())
    }

    pub fn sorted(&self) -> Vec<(String, String)> {
        let mut map = self.map.clone();
        map.sort_by(|a, b| restore(&a.0).cmp(&restore(&b.0)));
        map
    }

    pub fn ambiguous_reported(&self) -> Vec<(&str, Vec<&str>)> {
        static DUPLICATES: LazyLock<Regex> =
            LazyLock::new(|| re(r"(?i)(?:^|/)(test|fixture|example|stub)s?/"));
        (self.ambiguous.iter())
            .filter_map(|(class, paths)| {
                let kept: Vec<&str> = (paths.iter())
                    .filter(|p| !DUPLICATES.is_match(p.as_bytes()).unwrap_or(false))
                    .map(String::as_str)
                    .collect();
                (!kept.is_empty()).then_some((class.as_str(), kept))
            })
            .collect()
    }

    pub fn scan_all(&mut self, jobs: &[Job], cwd: &str) -> Result<()> {
        let listed: Vec<Result<Listing>> = jobs.par_iter().map(list).collect();
        let mut unique: HashMap<&Path, usize> = HashMap::new();
        let mut todo: Vec<&str> = vec![];
        for listing in listed.iter().flatten() {
            if listing.cached.is_some() {
                continue;
            }
            for (path, real) in &listing.files {
                if !self.scanned.contains(real) && !unique.contains_key(real.as_path()) {
                    unique.insert(real, todo.len());
                    todo.push(path);
                }
            }
        }
        let parsed: Vec<Result<Vec<String>, String>> = (todo.par_iter())
            .map(|path| match std::fs::read(path) {
                Ok(code) => Ok(find_classes(&code)),
                Err(_) => Err(format!(
                    "File at \"{path}\" is not readable, check its permissions"
                )),
            })
            .collect();
        let found = |listing: &Listing, i: usize, real: &Path| -> Result<Vec<String>> {
            match &listing.cached {
                Some(cached) => Ok(cached[i].clone()),
                None => parsed[unique[real]].clone().map_err(Error::msg),
            }
        };
        for (job, listing) in jobs.iter().zip(&listed) {
            let listing = listing.as_ref().map_err(Error::msg)?;
            if let (Some(file), None, true) = (&job.cache, &listing.cached, listing.cacheable) {
                save(job, listing, file, &found)?;
            }
            for (i, (path, real)) in listing.files.iter().enumerate() {
                if self.scanned.contains(real) {
                    continue;
                }
                let mut classes = found(listing, i, real)?;
                if job.rule == Rule::Classmap {
                    self.scanned.insert(real.clone());
                } else {
                    classes = self.filter(classes, path, &job.namespace, job.rule, &job.dir, cwd);
                    if !classes.is_empty() {
                        self.scanned.insert(real.clone());
                    }
                }
                for class in classes {
                    self.add(&class, path);
                }
            }
        }
        Ok(())
    }

    fn filter(
        &mut self,
        classes: Vec<String>,
        path: &str,
        namespace: &str,
        rule: Rule,
        base: &str,
        cwd: &str,
    ) -> Vec<String> {
        let sub = &path[(base.len() + 1).min(path.len())..];
        let sub = sub.rfind('.').map_or(sub, |dot| &sub[..dot]);
        let (valid, rejected): (Vec<String>, Vec<String>) =
            classes.into_iter().partition(|class| {
                let expected = if rule == Rule::Psr0 {
                    if !namespace.is_empty() && !class.starts_with(namespace) {
                        return false;
                    }
                    match class.rfind('\\') {
                        Some(i) => format!(
                            "{}{}",
                            class[..=i].replace('\\', "/"),
                            class[i + 1..].replace('_', "/")
                        ),
                        None => class.replace('_', "/"),
                    }
                } else {
                    class
                        .get(namespace.len()..)
                        .unwrap_or("")
                        .replace('\\', "/")
                };
                expected == sub
            });
        if valid.is_empty() {
            let short = |p: &str| match p.strip_prefix(cwd) {
                Some(rest) => format!(".{rest}"),
                None => p.to_string(),
            };
            let kind = if rule == Rule::Psr0 { "psr-0" } else { "psr-4" };
            for class in rejected {
                let warning = format!(
                    "Class {class} located in {} does not comply with {kind} autoloading standard (rule: {namespace} => {}). Skipping.",
                    short(path),
                    short(base)
                );
                self.violations.push((path.to_string(), warning));
            }
        }
        valid
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub dir: String,
    pub given: String,
    pub exclude: Option<Regex>,
    pub rule: Rule,
    pub namespace: String,
    /// Where the files this job lists and their classes are kept, for a directory whose contents can't change:
    /// a package from the store (see `autoload::ClassCache`).
    pub cache: Option<PathBuf>,
}

/// A job's files (as given and resolved), and their classes when they came from the job's cache.
struct Listing {
    files: Vec<(String, PathBuf)>,
    cached: Option<Vec<Vec<String>>>,
    /// No symlink below the job's directory, so the files can be found again from the cache.
    cacheable: bool,
}

fn list(job: &Job) -> Result<Listing> {
    if let Some(listing) = job.cache.as_deref().and_then(|file| cached(job, file)) {
        return Ok(listing);
    }
    let dir = Path::new(&job.dir);
    let mut files = vec![];
    let mut cacheable = true;
    match pun_tokens::php_compat::canonicalize(dir) {
        Ok(real) if dir.is_file() => files.push((job.dir.clone(), real)),
        Ok(real) if dir.is_dir() => {
            cacheable = walk(&job.dir, &real, &mut files, &mut vec![real.clone()]);
            if files.len() >= MAX_LISTED {
                bail!(
                    "\"{}\" holds more than {MAX_LISTED} files, counting each path its symlinks reach",
                    job.given
                );
            }
        }
        _ => bail!(
            "Could not scan for classes inside \"{}\" which does not appear to be a file nor a folder",
            job.given
        ),
    }
    let mut kept = vec![];
    for (path, real) in files {
        if ![".php", ".inc", ".hh"].iter().any(|e| path.ends_with(e)) {
            continue;
        }
        if let Some(x) = &job.exclude
            && (x.is_match(real.to_string_lossy().as_bytes())? || x.is_match(path.as_bytes())?)
        {
            continue;
        }
        kept.push((path, real));
    }
    Ok(Listing {
        files: kept,
        cached: None,
        cacheable,
    })
}

/// The listing kept in `file`: each file relative to the job's directory, with its classes.
fn cached(job: &Job, file: &Path) -> Option<Listing> {
    let entries: Vec<(String, Vec<String>)> =
        serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    let real = pun_tokens::php_compat::canonicalize(&job.dir).ok()?;
    let (mut files, mut classes) = (vec![], vec![]);
    for (rel, found) in entries {
        if rel.is_empty() {
            files.push((job.dir.clone(), real.clone()));
        } else {
            files.push((format!("{}/{rel}", job.dir), real.join(&rel)));
        }
        classes.push(found);
    }
    Some(Listing {
        files,
        cached: Some(classes),
        cacheable: true,
    })
}

/// The classes of a listing's file: from the cache or from this scan.
type Found<'a> = dyn Fn(&Listing, usize, &Path) -> Result<Vec<String>> + 'a;

/// Keeps a job's listing in `file`. A file that could not be read is the scan's error, as it is without the cache.
fn save(job: &Job, listing: &Listing, file: &Path, found: &Found) -> Result<()> {
    let entries: Vec<(&str, Vec<String>)> = (listing.files.iter().enumerate())
        .map(|(i, (path, real))| {
            let rel = path.strip_prefix(&job.dir).unwrap_or(path);
            Ok((rel.trim_start_matches('/'), found(listing, i, real)?))
        })
        .collect::<Result<_>>()?;
    // Best effort: a failed write only costs a scan next time.
    let _ = crate::fetch::store(file, &serde_json::to_vec(&entries)?);
    Ok(())
}

/// The most files one classmap directory may list. The largest packages hold tens of thousands; only symlinks that
/// alias directories over and over reach this.
const MAX_LISTED: usize = 1_000_000;

/// Lists the files below `dir`; false when it went through a symlink. A linked directory that is one of the walk's
/// own ancestors (`ancestors`, real paths) is a cycle and is not entered again: its files are listed already.
fn walk(
    dir: &str,
    real: &Path,
    out: &mut Vec<(String, PathBuf)>,
    ancestors: &mut Vec<PathBuf>,
) -> bool {
    const VCS: [&str; 4] = ["_svn", "CVS", "_darcs", ".arch-params"];
    let mut plain = true;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || VCS.contains(&name.as_ref()) {
            continue;
        }
        let path = format!("{dir}/{name}");
        let Ok(kind) = e.file_type() else { continue };
        let (is_dir, real) = if kind.is_symlink() {
            plain = false;
            match (
                std::fs::metadata(&path),
                pun_tokens::php_compat::canonicalize(&path),
            ) {
                (Ok(m), Ok(target)) => (m.is_dir(), target),
                _ => continue,
            }
        } else if kind.is_dir() || kind.is_file() {
            (kind.is_dir(), real.join(name.as_ref()))
        } else {
            continue;
        };
        if out.len() >= MAX_LISTED {
            break;
        }
        if is_dir {
            if ancestors.contains(&real) {
                continue;
            }
            ancestors.push(real.clone());
            plain &= walk(&path, &real, out, ancestors);
            ancestors.pop();
        } else {
            out.push((path, real));
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_classes_like_composer() {
        let code = br"<?php
namespace App\Models;

// class Commented {}
/* interface Hidden {} */
$s = 'class InString {}';
$h = <<<EOT
class InHeredoc {}
EOT;
#[Attribute]
final class User extends Model {}
enum Status: string { case A = 'a'; }
$anon = new class extends Base {};
interface Contract {}
trait Helpers {}
namespace Other { abstract class Base {} }
";
        assert_eq!(
            find_classes(code),
            [
                "App\\Models\\User",
                "App\\Models\\Status",
                "App\\Models\\Contract",
                "App\\Models\\Helpers",
                "Other\\Base"
            ]
        );
        assert_eq!(find_classes(b"<?php\nclass Only {}\n"), ["Only"]);
        assert_eq!(
            find_classes(b"<?php\n$x = `ls`; # class NotThis\nclass Shell {}\n"),
            ["Shell"]
        );
        assert_eq!(find_classes(b"<?php\n$x = 1;\n"), Vec::<String>::new());
        assert_eq!(
            find_classes(b"<?php\nclass :x:my-tag {}\n"),
            ["xhp_x__my_tag"]
        );
        assert_eq!(
            find_classes(b"<html><?php class A {} ?> class B <?php class C {}"),
            ["A", "C"]
        );
        let tricky = br#"<?php
# note ? here, class FakeHash {}
class A {}
$s = 'it\'s class Fake1 {}'; $t = "a\\"; $u = "x\y class Fake2 {}";
/* a * b class Fake3 {} */
$h = <<<EOT
  class Fake4 {}
  EOT;
class B {}
$d = 4 / 2;
$x = 'unterminated class Fake5 {}
"#;
        assert_eq!(find_classes(tricky), ["A", "B"]);
        assert_eq!(
            find_classes(b"<?php\nclass C {}\n/* never closed class Fake6 {}\n"),
            ["C"]
        );
    }

    /// A file the scan can't read is the same error with the class cache as without it, and nothing is kept.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_not_cached() {
        use std::os::unix::fs::PermissionsExt;
        let t =
            std::env::temp_dir().join(format!("pun-classmap-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("d")).unwrap();
        let file = t.join("d/A.php");
        std::fs::write(&file, "<?php class A {}\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let dir = t.join("d").to_string_lossy().into_owned();
        let job = Job {
            dir: dir.clone(),
            given: dir,
            exclude: None,
            rule: Rule::Classmap,
            namespace: String::new(),
            cache: Some(t.join("cache.json")),
        };
        let err = ClassMap::default().scan_all(&[job], "/usr").unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "File at \"{}\" is not readable, check its permissions",
                file.display()
            )
        );
        assert!(!t.join("cache.json").exists());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scans_like_composer() {
        let t = std::env::temp_dir().join(format!("pun-classmap-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let d = t.join("d");
        for dir in ["Foo/Bar/Baz", ".hidden", "CVS"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        std::fs::create_dir_all(t.join("real")).unwrap();
        for (file, code) in [
            (
                "d/Foo/Bar/Baz/Qux.php",
                "<?php\nnamespace Foo\\Bar;\nclass Baz_Qux {}\n",
            ),
            (
                "d/Foo/Bar/Wrong.php",
                "<?php\nnamespace Other;\nclass X {}\n",
            ),
            (
                "d/Twice.php",
                "<?php\nif (PHP_VERSION_ID > 1) { class Twice {} } else { class Twice {} }\n",
            ),
            ("d/.hidden.php", "<?php class Hidden {}\n"),
            ("d/.hidden/In.php", "<?php class InHidden {}\n"),
            ("d/CVS/C.php", "<?php class InCvs {}\n"),
            ("real/Linked.php", "<?php class Linked {}\n"),
        ] {
            std::fs::write(t.join(file), code).unwrap();
        }
        std::os::unix::fs::symlink(t.join("real"), d.join("linked")).unwrap();
        std::os::unix::fs::symlink(t.join("nowhere"), d.join("dangling")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(d.join("pipe"))
                .status()
                .unwrap()
                .success()
        );
        let dir = d.to_string_lossy().into_owned();
        let job = |rule, namespace: &str| Job {
            dir: dir.clone(),
            given: dir.clone(),
            exclude: None,
            rule,
            namespace: namespace.into(),
            cache: None,
        };
        let at = |p: &str| format!("{dir}/{p}");
        let mut map = ClassMap::default();
        map.scan_all(&[job(Rule::Classmap, ""), job(Rule::Psr0, "Foo\\")], "/usr")
            .unwrap();
        assert_eq!(
            map.sorted(),
            [
                ("Foo\\Bar\\Baz_Qux".to_string(), at("Foo/Bar/Baz/Qux.php")),
                ("Linked".to_string(), at("linked/Linked.php")),
                ("Other\\X".to_string(), at("Foo/Bar/Wrong.php")),
                ("Twice".to_string(), at("Twice.php")),
            ]
        );
        assert!(map.ambiguous.is_empty() && map.violations.is_empty());
        map.set("Twice", "/elsewhere.php");
        assert_eq!(map.path_of("Twice"), Some("/elsewhere.php"));

        let mut psr0 = ClassMap::default();
        psr0.scan_all(&[job(Rule::Psr0, "Foo\\")], "/usr").unwrap();
        let rule = format!("(rule: Foo\\ => {dir}). Skipping.");
        let violation = |class: &str, file: &str| {
            format!(
                "Class {class} located in {} does not comply with psr-0 autoloading standard {rule}",
                at(file)
            )
        };
        let mut violations: Vec<String> = psr0.violations.iter().map(|v| v.1.clone()).collect();
        violations.sort();
        let mut want = [
            violation("Other\\X", "Foo/Bar/Wrong.php"),
            violation("Linked", "linked/Linked.php"),
            violation("Twice", "Twice.php"),
            violation("Twice", "Twice.php"),
        ];
        want.sort();
        assert_eq!(violations, want);
        assert_eq!(
            psr0.sorted(),
            [("Foo\\Bar\\Baz_Qux".to_string(), at("Foo/Bar/Baz/Qux.php"))]
        );
        std::fs::remove_dir_all(&t).unwrap();
    }

    /// Four links back to the directory itself: following each again gave 4^40 paths before the kernel's link
    /// limit stopped it. Each is a cycle, so the class is listed once, at its own path.
    #[cfg(unix)]
    #[test]
    fn links_back_to_an_ancestor_are_not_walked_again() {
        let t = std::env::temp_dir().join(format!("pun-classmap-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("d/sub")).unwrap();
        std::fs::write(t.join("d/sub/A.php"), "<?php class A {}\n").unwrap();
        for link in ["l1", "l2", "l3"] {
            std::os::unix::fs::symlink(t.join("d"), t.join("d/sub").join(link)).unwrap();
        }
        std::os::unix::fs::symlink(t.join("d/sub"), t.join("d/sub/l4")).unwrap();
        let dir = t.join("d").to_string_lossy().into_owned();
        let job = Job {
            dir: dir.clone(),
            given: dir.clone(),
            exclude: None,
            rule: Rule::Classmap,
            namespace: String::new(),
            cache: None,
        };
        let mut map = ClassMap::default();
        map.scan_all(&[job], "/usr").unwrap();
        assert_eq!(
            map.sorted(),
            [("A".to_string(), format!("{dir}/sub/A.php"))]
        );
        std::fs::remove_dir_all(&t).unwrap();
    }

    /// Conflicts keep the order they were found in, each class's paths in order. 100,000 classes with a second file
    /// each searched the earlier conflicts (5e9 comparisons of one long prefix); indexed, this takes milliseconds.
    #[test]
    fn conflicts_keep_their_order_and_are_found_by_name() {
        let mut map = ClassMap::default();
        for (class, path) in [
            ("B", "b1"),
            ("A", "a1"),
            ("A", "a2"),
            ("B", "b2"),
            ("B", "b1"),
            ("A", "a3"),
        ] {
            map.add(class, path);
        }
        let paths = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            map.ambiguous,
            [
                ("A".to_string(), paths(&["a2", "a3"])),
                ("B".to_string(), paths(&["b2"])),
            ]
        );

        let class = |i: usize| format!("{}{i:06}", "Vendor\\Package\\".repeat(20));
        let started = std::time::Instant::now();
        let mut many = ClassMap::default();
        for i in 0..100_000 {
            many.add(&class(i), "first.php");
            many.add(&class(i), "second.php");
        }
        assert_eq!(many.ambiguous.len(), 100_000);
        assert_eq!(
            many.ambiguous[99_999],
            (class(99_999), paths(&["second.php"]))
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
