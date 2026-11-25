use pcre2::bytes::Regex;
use pun_tokens::php_compat;

pub fn preg(pattern: &str) -> Result<Regex, String> {
    php_compat::preg_compile(pattern).map_err(|m| format!("{m} in pattern: {pattern}"))
}

pub fn is_match(re: &Regex, subject: &str) -> bool {
    re.is_match(subject.as_bytes()).unwrap_or(false)
}

pub fn preg_quote(s: &str, delimiter: Option<char>) -> String {
    let d = delimiter.map(|d| d.to_string().as_bytes()[0]);
    String::from_utf8_lossy(&php_compat::preg_quote(s.as_bytes(), d)).into_owned()
}

pub fn fnmatch(pattern: &str, string: &str) -> bool {
    php_compat::fnmatch(pattern.as_bytes(), string.as_bytes())
}

pub fn glob(pattern: &str) -> Vec<String> {
    php_compat::glob(pattern.as_bytes())
        .iter()
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

pub fn normalize_path(original: &str) -> String {
    let bytes = original.as_bytes();
    let local = original.starts_with('/')
        || bytes.get(1..3) == Some(b":\\")
        || (cfg!(windows) && bytes.get(1..3) == Some(b":/"));
    let (mut scheme, path) = match original.split_once("://") {
        Some((s, p)) if !local && !p.is_empty() && is_scheme(s) => {
            (Some(s.to_ascii_lowercase()), p)
        }
        _ => (None, original),
    };
    let path = path.replace('\\', "/").replace("//", "/");
    let (root, body) = if let Some(rest) = path.strip_prefix('/') {
        ("/", rest)
    } else if cfg!(windows)
        && path.len() >= 2
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
    {
        (&path[..2], path[2..].trim_start_matches('/'))
    } else {
        ("", path.as_str())
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in body.trim_end_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        match part {
            "." => {}
            ".." => {
                let removed = parts.pop();
                if scheme.as_deref() == Some("phar")
                    && removed.is_some_and(|r| r.ends_with(".phar"))
                {
                    scheme = None;
                }
            }
            p => parts.push(p),
        }
    }
    let prefix = scheme.map_or_default(|s| s + "://");
    let joined = parts.join("/");
    #[cfg(windows)]
    if root.len() == 2 {
        if joined.is_empty() {
            return format!("{prefix}{root}");
        }
        return format!("{prefix}{root}/{joined}");
    }
    format!("{prefix}{root}{joined}")
}

pub fn absolutize_path(cwd: &str, path: &str) -> String {
    if path.starts_with('/') || has_scheme(path) || windows_absolute(path) {
        return path.to_string();
    }
    format!(
        "{}/{}",
        cwd.trim_end_matches(['/', '\\']),
        path.trim_start_matches(['/', '\\'])
    )
}

fn windows_absolute(path: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
}

fn has_scheme(path: &str) -> bool {
    path.split_once("://").is_some_and(|(s, _)| is_scheme(s))
}

fn is_scheme(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
}

pub fn is_fnmatch_pattern(path: &str) -> bool {
    path.contains(['*', '?', '[', ']'])
}

pub struct FileExcluder {
    files: Vec<String>,
    dirs: Vec<String>,
    patterns: Vec<String>,
}

impl FileExcluder {
    pub fn new<'a>(excludes: impl IntoIterator<Item = &'a str>) -> FileExcluder {
        let mut ex = FileExcluder {
            files: Vec::new(),
            dirs: Vec::new(),
            patterns: Vec::new(),
        };
        for exclude in excludes {
            let trailing = exclude.ends_with(['/', '\\']);
            let mut normalized = normalize_path(exclude);
            if trailing {
                normalized.push('/');
            }
            if is_fnmatch_pattern(&normalized) {
                ex.patterns.push(normalized);
            } else if std::path::Path::new(&normalized).is_file() {
                ex.files.push(normalized);
            } else if std::path::Path::new(&normalized).is_dir() {
                if !trailing {
                    normalized.push('/');
                }
                ex.dirs.push(normalized);
            }
        }
        ex
    }

    pub fn is_excluded(&self, file: &str) -> bool {
        let file = normalize_path(file);
        self.files.contains(&file)
            || self.dirs.iter().any(|d| file.starts_with(d.as_str()))
            || self.patterns.iter().any(|p| fnmatch(p, &file))
    }
}

pub fn parent_relative_path(parent: &str, filename: &str) -> String {
    let filename = filename
        .find("://")
        .map_or(filename, |p| &filename[p + 3..]);
    let parent = parent.replace('\\', "/");
    let file = filename.replace('\\', "/");
    let pp: Vec<&str> = parent.trim_matches('/').split('/').collect();
    let fp: Vec<&str> = file.trim_matches('/').split('/').collect();
    let mut i = 0;
    while i < fp.len() && i < pp.len() && pp[..=i] == fp[..=i] {
        i += 1;
    }
    if i == 0 {
        return filename.to_string();
    }
    let mut parts = vec![".."; pp.len() - i];
    parts.extend(&fp[i..]);
    parts.join("/")
}
