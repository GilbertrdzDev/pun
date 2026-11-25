use crate::php::export_str;

/// Length of the root of an absolute path: 1 for `/`, 3 for a Windows drive such as `C:/` (paths here use
/// `/`); 0 for a relative path. Drive roots exist only on Windows, so a file literally named `C:/x` stays relative
/// on Linux and macOS.
fn root_len(p: &str) -> usize {
    let b = p.as_bytes();
    if b.first() == Some(&b'/') {
        1
    } else if cfg!(windows)
        && b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'/'
    {
        3
    } else {
        0
    }
}

/// Whether `p` is an absolute path (`/x`, or on Windows `C:/x` or `C:\x`).
pub fn is_absolute(p: &str) -> bool {
    if cfg!(windows) {
        root_len(&p.replace('\\', "/")) > 0
    } else {
        p.starts_with('/')
    }
}

fn is_root(p: &str) -> bool {
    let n = root_len(p);
    n > 0 && p.len() == n
}

pub fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let (absolute, rest) = match root_len(&path) {
        0 => ("", path.as_str()),
        n => (&path[..n], &path[n..]),
    };
    let mut parts: Vec<&str> = vec![];
    let mut up = false;
    for chunk in rest.split('/') {
        if chunk == ".." && (!absolute.is_empty() || up) {
            parts.pop();
            up = !(parts.is_empty() || parts.last() == Some(&".."));
        } else if chunk != "." && !chunk.is_empty() {
            parts.push(chunk);
            up = chunk != "..";
        }
    }
    format!("{absolute}{}", parts.join("/"))
}

fn dirname(p: &str) -> &str {
    let root = root_len(p);
    match p.rfind('/') {
        Some(i) if i >= root && i > 0 => &p[..i],
        _ if root > 1 => &p[..root],
        _ => "/",
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn common(from: &str, to: &str) -> String {
    let mut common = to.to_string();
    while !format!("{from}/").starts_with(&format!("{common}/")) && !is_root(&common) {
        common = dirname(&common).to_string();
    }
    format!("{}/", common.trim_end_matches('/'))
}

pub fn shortest(from: &str, to: &str, directory: bool) -> String {
    let mut from = normalize(from);
    let to = normalize(to);
    if directory {
        from = format!("{}/dummy_file", from.trim_end_matches('/'));
    }
    if dirname(&from) == dirname(&to) {
        return format!("./{}", basename(&to));
    }
    let common = common(&from, &to);
    let depth = from[common.len()..].matches('/').count();
    if is_root(&common) && depth > 1 {
        return to;
    }
    let result = format!(
        "{}{}",
        "../".repeat(depth),
        &to[common.len().min(to.len())..]
    );
    if result.is_empty() {
        "./".into()
    } else {
        result
    }
}

pub fn shortest_code(from: &str, to: &str, directory: bool, static_code: bool) -> String {
    let from = normalize(from);
    let to = normalize(to);
    if from == to {
        return if directory { "__DIR__" } else { "__FILE__" }.into();
    }
    let common = common(&from, &to);
    if let Some(rest) = to.strip_prefix(&format!("{from}/")) {
        return format!("__DIR__ . {}", export_str(&format!("/{rest}")));
    }
    let depth = from[common.len()..].matches('/').count() + usize::from(directory);
    if is_root(&common) && depth > 1 {
        return export_str(&to);
    }
    let up = if static_code {
        format!("__DIR__ . '{}'", "/..".repeat(depth))
    } else {
        format!("{}__DIR__{}", "dirname(".repeat(depth), ")".repeat(depth))
    };
    let rel = &to[common.len().min(to.len())..];
    if rel.is_empty() {
        up
    } else {
        format!("{up}.{}", export_str(&format!("/{rel}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_composer() {
        assert_eq!(normalize("/a/./b//c/../d/"), "/a/b/d");
        assert_eq!(normalize("../a/../../b"), "../../b");
        assert_eq!(normalize("a\\b"), "a/b");
        let v = "/p/vendor";
        assert_eq!(
            shortest(
                "/p/vendor/bin/sail",
                "/p/vendor/laravel/sail/bin/sail",
                false
            ),
            "../laravel/sail/bin/sail"
        );
        assert_eq!(
            shortest("/p/vendor/composer", "/p/vendor/acme/lib", true),
            "../acme/lib"
        );
        assert_eq!(shortest("/p/vendor/composer", "/p", true), "../../");
        assert_eq!(shortest("/p", "/p", true), "./");
        assert_eq!(shortest("/p/a", "/p/b", false), "./b");
        assert_eq!(shortest("/foo/a/b", "/bar/c", false), "/bar/c");
        assert_eq!(
            shortest_code(v, "/p/vendor/composer", true, false),
            "__DIR__ . '/composer'"
        );
        assert_eq!(
            shortest_code("/p/vendor/composer", v, true, false),
            "dirname(__DIR__)"
        );
        assert_eq!(shortest_code(v, "/p", true, false), "dirname(__DIR__)");
        assert_eq!(
            shortest_code("/p/vendor/composer", "/p", true, true),
            "__DIR__ . '/../..'"
        );
        assert_eq!(
            shortest_code(
                "/p/vendor/bin/pest",
                "/p/vendor/pestphp/pest/bin/pest",
                false,
                true
            ),
            "__DIR__ . '/..'.'/pestphp/pest/bin/pest'"
        );
        assert_eq!(shortest_code(v, v, true, true), "__DIR__");
        assert_eq!(shortest_code("/p/x", "/p/x", false, true), "__FILE__");
        assert_eq!(shortest_code("/foo/a/b", "/bar/c", false, true), "'/bar/c'");
    }

    /// A generated autoload on Windows has to keep the drive (`C:/...`) and still walk `..` inside it.
    #[cfg(windows)]
    #[test]
    fn windows_drive_paths_stay_absolute() {
        assert!(is_absolute(r"C:\proj\src"));
        assert!(is_absolute("C:/proj/src"));
        assert!(!is_absolute("proj/src"));
        assert_eq!(normalize(r"C:\proj\.\src\..\lib"), "C:/proj/lib");
        assert_eq!(
            shortest(
                "C:/proj/vendor/bin/tool",
                "C:/proj/vendor/acme/bin/tool",
                false
            ),
            "../acme/bin/tool"
        );
        assert_eq!(shortest("C:/proj/a/b", "D:/other", false), "D:/other");
    }
}
