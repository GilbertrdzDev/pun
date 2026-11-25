use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    /// The globs that match a member, each a path repository.
    globs: Vec<String>,
    pub(crate) members: Vec<Member>,
}

pub(crate) struct Member {
    pub(crate) name: String,
    pub(crate) dir: PathBuf,
}

pub(crate) fn find(dir: &Path) -> Result<Option<Workspace>> {
    for root in dir.ancestors() {
        let Some(globs) = globs(root)? else { continue };
        let workspace = load(root, globs)?;
        let inside = root == dir || workspace.members.iter().any(|m| dir.starts_with(&m.dir));
        return Ok(inside.then_some(workspace));
    }
    Ok(None)
}

fn globs(root: &Path) -> Result<Option<Vec<String>>> {
    let path = root.join("pun.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    if !text.contains("workspace") {
        return Ok(None);
    }
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    let Some(workspace) = table.get("workspace") else {
        return Ok(None);
    };
    let members = (workspace.as_table())
        .filter(|t| t.keys().all(|k| k == "members"))
        .and_then(|t| t.get("members"))
        .and_then(toml::Value::as_array)
        .and_then(|m| {
            m.iter()
                .map(|g| g.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()
        });
    match members {
        Some(m) => Ok(Some(m)),
        None => bail!(
            "{}: [workspace] takes one key, members, a list of directories or globs such as \"packages/*\"",
            path.display()
        ),
    }
}

/// The members the globs match. Only the globs that match a member become path repositories, since Composer's path
/// repository fails on a glob whose directory doesn't exist.
fn load(root: &Path, globs: Vec<String>) -> Result<Workspace> {
    let (mut members, mut matching): (Vec<Member>, Vec<String>) = (vec![], vec![]);
    let real_root =
        crate::canonical(root).with_context(|| format!("can't open {}", root.display()))?;
    for glob in globs {
        let mut matches = false;
        for dir in expand(root, &glob) {
            let Some(name) = name_of(&dir)? else { continue };
            // A member is a folder of this repo: one reached through `..` or a link elsewhere would let a member's
            // `require -p` edit another checkout's composer.json.
            if !crate::canonical(&dir).is_ok_and(|real| real.starts_with(&real_root)) {
                bail!(
                    "the workspace member {name} in {} is outside the workspace root {}",
                    dir.display(),
                    root.display()
                );
            }
            matches = true;
            if let Some(other) = members.iter().find(|m| m.name == name && m.dir != dir) {
                bail!(
                    "two workspace members are named {name}: {} and {}",
                    other.dir.display(),
                    dir.display()
                );
            }
            if !members.iter().any(|m| m.dir == dir) {
                members.push(Member { name, dir });
            }
        }
        if matches {
            matching.push(glob);
        }
    }
    members.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Workspace {
        root: root.to_path_buf(),
        globs: matching,
        members,
    })
}

fn name_of(dir: &Path) -> Result<Option<String>> {
    let Some(manifest) = pun_config::manifest::read(dir)? else {
        return Ok(None);
    };
    match manifest.json.get("name").and_then(Value::as_str) {
        Some(name) => Ok(Some(name.to_ascii_lowercase())),
        None => bail!(
            "the workspace member in {} has no name: set `name` in its composer.json",
            dir.display()
        ),
    }
}

fn expand(root: &Path, glob: &str) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for segment in glob.split('/').filter(|s| !s.is_empty() && *s != ".") {
        if !segment.contains(['*', '?']) {
            dirs = dirs.into_iter().map(|d| d.join(segment)).collect();
            continue;
        }
        let mut next = vec![];
        for dir in &dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let hidden = name.starts_with('.') && !segment.starts_with('.');
                if !hidden && entry.path().is_dir() && wildcard(segment.as_bytes(), name.as_bytes())
                {
                    next.push(entry.path());
                }
            }
        }
        dirs = next;
    }
    dirs.retain(|d| d.is_dir());
    dirs.sort();
    dirs
}

/// Whether `name` matches `pattern`, where `*` is any run of bytes and `?` any one byte. On a mismatch only the
/// last `*` takes one more byte, so a match costs at most `pattern.len() * name.len()` steps, however many stars.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == b'?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    star = Some((sp, sn + 1));
                    p = sp + 1;
                    n = sn + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

impl Workspace {
    pub(crate) fn member(&self, name: Option<&str>, dir: &Path) -> Result<Option<&Member>> {
        match name {
            Some(name) => {
                let name = name.to_ascii_lowercase();
                match self.members.iter().find(|m| m.name == name) {
                    Some(m) => Ok(Some(m)),
                    None => bail!(
                        "{name} is not a workspace member; the members are {}",
                        self.members
                            .iter()
                            .map(|m| m.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            None => Ok(self.members.iter().find(|m| dir.starts_with(&m.dir))),
        }
    }

    pub(crate) fn manifest(&self, mut manifest: Map<String, Value>) -> Map<String, Value> {
        let mut repositories: Vec<Value> = (self.globs.iter())
            .map(|g| json!({"type": "path", "url": g, "options": {"symlink": true}}))
            .collect();
        match manifest.get("repositories") {
            Some(Value::Array(own)) => repositories.extend(own.iter().cloned()),
            Some(Value::Object(own)) => repositories.extend(own.iter().map(|(k, v)| match v {
                Value::Bool(false) => json!({ k.as_str(): false }),
                v => v.clone(),
            })),
            _ => {}
        }
        manifest.insert("repositories".into(), Value::Array(repositories));
        let required = |m: &Map<String, Value>, name: &str| {
            ["require", "require-dev"].iter().any(|s| {
                (m.get(*s).and_then(Value::as_object))
                    .is_some_and(|r| r.keys().any(|k| k.eq_ignore_ascii_case(name)))
            })
        };
        for member in &self.members {
            if !required(&manifest, &member.name) {
                let require = manifest.entry("require").or_insert_with(|| json!({}));
                if !require.is_object() {
                    // Composer's schema rejects such a `require`, and Pun's resolver reads it as no requirements.
                    *require = json!({});
                }
                require[member.name.as_str()] = "*@dev".into();
            }
        }
        manifest
    }
}

#[cfg(test)]
mod tests {
    use super::wildcard;

    #[test]
    fn matches_wildcards() {
        for (pattern, name, want) in [
            ("*", "billing", true),
            ("bill*", "billing", true),
            ("*ing", "billing", true),
            ("b?lling", "billing", true),
            ("b?lling", "blling", false),
            ("*x*", "billing", false),
            ("billing", "billings", false),
            ("", "", true),
            ("**", "", true),
            ("?", "", false),
            ("b*l*g", "billing", true),
            ("*l?n*", "billing", true),
            ("b*?", "b", false),
        ] {
            assert_eq!(
                wildcard(pattern.as_bytes(), name.as_bytes()),
                want,
                "{pattern} {name}"
            );
        }
    }

    /// A segment with many stars against a long name that misses at the end: backtracking into every star took
    /// exponential time; this returns at once.
    #[test]
    fn many_stars_match_in_polynomial_time() {
        let pattern = "*a".repeat(30) + "b";
        let name = "a".repeat(200);
        let started = std::time::Instant::now();
        assert!(!wildcard(pattern.as_bytes(), name.as_bytes()));
        assert!(wildcard(pattern.as_bytes(), (name + "b").as_bytes()));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
