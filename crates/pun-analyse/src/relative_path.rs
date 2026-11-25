use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct RelativePath {
    cwd: String,
    trim: Option<String>,
}

/// Windows paths (and the `\\?\` form `canonicalize` returns) become `C:/a/b`. Elsewhere a path is unchanged.
#[cfg(windows)]
fn norm(path: &str) -> String {
    let path = path.replace('\\', "/");
    match path.strip_prefix("//?/") {
        Some(rest) => rest.to_string(),
        None => path,
    }
}

#[cfg(not(windows))]
fn norm(path: &str) -> String {
    path.to_string()
}

fn trim_beginning(path: &str) -> (&str, &str) {
    match path.strip_prefix('/') {
        Some(rest) => ("/", rest),
        None => ("", path),
    }
}

impl RelativePath {
    pub fn new(cwd: &str, analysed_paths: &[String]) -> RelativePath {
        let cwd = norm(cwd);
        let cwd = cwd.as_str();
        let analysed_paths: Vec<String> = analysed_paths.iter().map(|p| norm(p)).collect();
        let analysed_paths = analysed_paths.as_slice();
        let mut beginning = "";
        let mut to_trim: Option<Vec<&str>> = None;
        if !matches!(cwd, "" | "/") {
            let (b, rest) = trim_beginning(cwd);
            beginning = b;
            to_trim = Some(rest.split('/').collect());
        }
        'paths: for (number, path) in analysed_paths.iter().enumerate() {
            let (path_beginning, path) = trim_beginning(path);
            let parts: Vec<&str> = path.split('/').collect();
            let mut temp = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                if i == parts.len() - 1 && part.ends_with(".php") {
                    continue;
                }
                match to_trim.as_ref().and_then(|t| t.get(i)) {
                    None if number != 0 => {
                        to_trim = Some(temp);
                        continue 'paths;
                    }
                    Some(p) if p != part => {
                        to_trim = Some(temp);
                        continue 'paths;
                    }
                    _ => {}
                }
                temp.push(*part);
            }
            beginning = path_beginning;
            to_trim = Some(temp);
        }
        let trim = to_trim.filter(|t| !t.is_empty()).map(|t| {
            let trim = format!("{beginning}{}", t.join("/"));
            pun_tokens::php_compat::canonicalize(&trim).map_or(trim, |p| norm(&p.to_string_lossy()))
        });
        RelativePath {
            cwd: cwd.to_string(),
            trim,
        }
    }

    pub fn of(analysed_paths: &[String]) -> RelativePath {
        let cwd = std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        RelativePath::new(&cwd, analysed_paths)
    }

    pub fn get(&self, filename: &str) -> String {
        let filename = norm(filename);
        let filename = filename.as_str();
        if let Some(rest) = self.trim.as_deref().and_then(|t| filename.strip_prefix(t)) {
            return rest.trim_start_matches('/').to_string();
        }
        self.parent_directory(filename)
    }

    fn parent_directory(&self, filename: &str) -> String {
        let filename = filename
            .find("://")
            .map_or(filename, |at| &filename[at + 3..]);
        let parent: Vec<&str> = self.cwd.trim_matches('/').split('/').collect();
        let parts: Vec<&str> = filename.trim_matches('/').split('/').collect();
        let mut i = 0;
        while i < parts.len() && i < parent.len() && parent[..=i] == parts[..=i] {
            i += 1;
        }
        if i == 0 {
            return filename.to_string();
        }
        let mut out = vec![".."; parent.len() - i];
        out.extend(&parts[i..]);
        out.join("/")
    }

    pub(crate) fn simple(&self, filename: &str) -> String {
        let filename = norm(filename);
        let filename = filename.as_str();
        let rest = (!self.cwd.is_empty())
            .then(|| filename.strip_prefix(self.cwd.as_str()))
            .flatten();
        rest.map_or(filename, |r| r.get(1..).unwrap_or(""))
            .replace('\\', "/")
    }
}

pub(crate) fn absolute(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}
