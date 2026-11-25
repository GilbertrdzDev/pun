use crate::classmap::{ClassMap, Job, Rule};
use crate::constraint::{Bound, lower_bound};
use crate::error::{Error, Result};
use crate::installed::Installed;
use crate::paths::{is_absolute, normalize, shortest, shortest_code};
use crate::php::{Key, Php, export_str, md5, strtr, var_export};
use crate::version::IgnorePlatform;
use pcre2::bytes::Regex;
use pun_tokens::php_compat::{preg_quote, strnatcasecmp};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformCheck {
    Off,
    PhpOnly,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Scan {
    /// The classes of the `classmap` rules.
    #[default]
    Classmap,
    /// The PSR-0 and PSR-4 classes too (`--optimize-autoloader`).
    Optimized,
    /// As `Optimized`, and the loader looks nowhere else (`--classmap-authoritative`).
    Authoritative,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Apcu {
    #[default]
    Off,
    /// A prefix picked at random on each dump.
    RandomPrefix,
    Prefix(String),
}

#[derive(Debug)]
pub struct Options {
    pub scan: Scan,
    pub apcu: Apcu,
    pub dev_mode: bool,
    pub platform_check: PlatformCheck,
    pub ignore_platform: IgnorePlatform,
    pub use_include_path: bool,
    pub prepend: bool,
    pub suffix: String,
    /// Where the classes of packages from the store are kept between dumps (see `ClassCache`).
    pub class_cache: Option<std::path::PathBuf>,
}

/// The class map cache: a package installed from a dist archive is taken not to change while it stays at one version
/// and reference in one place (the store links the same files there), so the files a scan lists under it and the
/// classes they declare are kept, keyed by the package, its install path, the directory scanned, the exclusions and
/// the Pun binary. Root paths, path repositories and source installs are scanned every time. A hand edit to a
/// package in `vendor/` is not seen until the package is installed again.
struct ClassCache<'a> {
    dir: &'a Path,
    packages: Vec<(String, String)>,
}

impl ClassCache<'_> {
    fn file(&self, dir: &str, exclude: Option<&Regex>) -> Option<std::path::PathBuf> {
        let (install, id) = (self.packages.iter())
            .find(|(install, _)| dir == install || dir.starts_with(&format!("{install}/")))?;
        let key = format!(
            "{id}\0{install}\0{dir}\0{}",
            exclude.map_or("", Regex::as_str)
        );
        let key = crate::php::sha256(key.as_bytes());
        Some(self.dir.join(&key[..2]).join(format!("{key}.json")))
    }
}

/// A file of the autoloader, relative to the project when it is inside it.
#[derive(Debug, PartialEq, Eq)]
pub struct DumpedFile {
    pub path: String,
    /// `None` when the file is not needed, so an older one is removed.
    pub bytes: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct Dumped {
    pub files: Vec<DumpedFile>,
    pub classes: usize,
    pub warnings: Vec<String>,
}

/// A dump as `Dumped::save` keeps it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept<'a> {
    #[serde(borrow)]
    files: Vec<(Cow<'a, str>, Option<Cow<'a, str>>)>,
    classes: usize,
    warnings: Vec<String>,
}

impl Dumped {
    /// Keeps the dump in `file` under `key` (the first line). Best effort: a failed write only costs a dump next time.
    pub fn save(&self, file: &Path, key: &str) {
        let kept = Kept {
            files: (self.files.iter())
                .map(|f| {
                    let text = f.bytes.as_deref().map(crate::classmap::lossless);
                    (Cow::from(f.path.as_str()), text)
                })
                .collect(),
            classes: self.classes,
            warnings: self.warnings.clone(),
        };
        let mut text = format!("{key}\n").into_bytes();
        serde_json::to_writer(&mut text, &kept).expect("a dump serializes");
        let _ = crate::fetch::store(file, &text);
    }

    /// The dump `save` kept in `file`, when it was kept under `key`.
    pub fn load(file: &Path, key: &str) -> Option<Dumped> {
        let text = std::fs::read(file).ok()?;
        let rest = text.strip_prefix(key.as_bytes())?.strip_prefix(b"\n")?;
        let kept: Kept = serde_json::from_slice(rest).ok()?;
        Some(Dumped {
            files: (kept.files.into_iter())
                .map(|(path, text)| DumpedFile {
                    path: path.into_owned(),
                    bytes: text.map(|t| crate::classmap::restore(&t).into_owned()),
                })
                .collect(),
            classes: kept.classes,
            warnings: kept.warnings,
        })
    }
}

struct FileEntry {
    id: String,
    code: String,
    path: String,
}

struct StaticMaps<'a> {
    psr0: &'a [(String, Vec<String>)],
    psr4: &'a [(String, Vec<String>)],
    /// Each PSR path's absolute form.
    evaluated: &'a HashMap<String, String>,
    classes: &'a [(String, String)],
    files: &'a [FileEntry],
}

struct Loads {
    include_paths: bool,
    files: bool,
    platform_check: bool,
}

struct Pkg<'a> {
    name: String,
    json: &'a Map<String, Value>,
    path: Option<String>,
    root: bool,
    dev: bool,
}

impl Pkg<'_> {
    fn links(&self, kind: &str) -> Vec<(String, String)> {
        (self
            .json
            .get(kind)
            .and_then(Value::as_object)
            .into_iter()
            .flatten())
        .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap_or("").to_string()))
        .collect()
    }
}

fn sort_packages(packages: Vec<Pkg<'_>>) -> Vec<Pkg<'_>> {
    /// Composer's `PackageSorter` weight: minus one, plus the user's weight, for each package that requires `root`;
    /// a package already being weighed counts 0. The walk keeps its own stack, in the order Composer's recursion
    /// takes, so a deep dependency chain can't overflow the thread's.
    fn importance<'a>(
        root: &'a str,
        usage: &'a HashMap<String, Vec<String>>,
        computed: &mut HashMap<String, i64>,
    ) -> i64 {
        if let Some(w) = computed.get(root) {
            return *w;
        }
        // (package, index of its next user, weight so far)
        let mut active: HashSet<&str> = HashSet::from([root]);
        let mut stack: Vec<(&str, usize, i64)> = vec![(root, 0, 0)];
        while let Some(top) = stack.last_mut() {
            let users = usage.get(top.0).map_or(&[][..], Vec::as_slice);
            if let Some(user) = users.get(top.1) {
                top.1 += 1;
                let user = user.as_str();
                let known =
                    (computed.get(user).copied()).or_else(|| active.contains(user).then_some(0));
                if let Some(w) = known {
                    top.2 -= 1 - w;
                } else {
                    active.insert(user);
                    stack.push((user, 0, 0));
                }
                continue;
            }
            let (name, _, weight) = *top;
            stack.pop();
            active.remove(name);
            computed.insert(name.to_string(), weight);
            match stack.last_mut() {
                Some(parent) => parent.2 -= 1 - weight,
                None => return weight,
            }
        }
        unreachable!("the walk returns when it pops the root")
    }
    let mut usage: HashMap<String, Vec<String>> = HashMap::new();
    for p in &packages {
        for (target, _) in p.links("require") {
            usage.entry(target).or_default().push(p.name.clone());
        }
    }
    let mut computed = HashMap::new();
    let mut weighted: Vec<(i64, Pkg)> = (packages.into_iter())
        .map(|p| (importance(&p.name, &usage, &mut computed), p))
        .collect();
    weighted.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| strnatcasecmp(a.1.name.as_bytes(), b.1.name.as_bytes()).cmp(&0))
    });
    weighted.into_iter().map(|w| w.1).collect()
}

fn autoload(p: &Pkg, dev_mode: bool) -> Map<String, Value> {
    let mut a = p
        .json
        .get("autoload")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if dev_mode && p.root {
        for (kind, dev) in p
            .json
            .get("autoload-dev")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            match (a.get_mut(kind), dev) {
                (Some(Value::Object(base)), Value::Object(dev)) => {
                    for (ns, paths) in dev {
                        match base.get_mut(ns) {
                            Some(existing) => {
                                let mut list = as_list(existing);
                                list.extend(as_list(paths));
                                *existing = Value::Array(list);
                            }
                            None => {
                                base.insert(ns.clone(), paths.clone());
                            }
                        }
                    }
                }
                (Some(base), dev) => {
                    let mut list = as_list(base);
                    list.extend(as_list(dev));
                    *base = Value::Array(list);
                }
                (None, dev) => {
                    a.insert(kind.clone(), dev.clone());
                }
            }
        }
    }
    a
}

fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items.clone(),
        Value::Object(m) => m.values().cloned().collect(),
        v => vec![v.clone()],
    }
}

fn strings(v: &Value) -> Vec<String> {
    as_list(v)
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn mapping(packages: &[&Pkg], kind: &str, dev_mode: bool) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = vec![];
    for p in packages {
        let Some(install) = &p.path else { continue };
        let a = autoload(p, dev_mode);
        let entries: Vec<(String, Value)> = match a.get(kind) {
            Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Some(Value::Array(items)) => items.iter().map(|v| (String::new(), v.clone())).collect(),
            _ => continue,
        };
        for (ns, paths) in entries {
            let ns = if kind == "classmap" {
                String::new()
            } else {
                ns.trim_start_matches('\\').to_string()
            };
            for path in strings(&paths) {
                let relative = match install.as_str() {
                    "" if path.is_empty() => ".".to_string(),
                    "" => path,
                    i => format!("{i}/{path}"),
                };
                match out.iter_mut().find(|e| e.0 == ns) {
                    Some(e) => e.1.push(relative),
                    None => out.push((ns.clone(), vec![relative])),
                }
            }
        }
    }
    out
}

fn path_code(base: &str, vendor: &str, path: &str) -> Result<(String, String)> {
    static PHAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.phar([\\/]|$)").unwrap());
    let path = normalize(&if is_absolute(path) {
        path.to_string()
    } else {
        format!("{base}/{path}")
    });
    let (prefix, rel, value) = if format!("{path}/").starts_with(&format!("{vendor}/")) {
        let rel = path[vendor.len()..].to_string();
        ("$vendorDir . ", rel.clone(), format!("{vendor}{rel}"))
    } else {
        let rel = normalize(&shortest(base, &path, true));
        if is_absolute(&rel) {
            ("", rel.clone(), rel)
        } else {
            ("$baseDir . ", format!("/{rel}"), format!("{base}/{rel}"))
        }
    };
    let phar = PHAR.is_match(rel.as_bytes())?;
    let code = format!(
        "{}{prefix}{}",
        if phar { "'phar://' . " } else { "" },
        export_str(&rel)
    );
    Ok((
        code,
        if phar {
            format!("phar://{value}")
        } else {
            value
        },
    ))
}

fn exclusions(packages: &[&Pkg], dev_mode: bool, cwd: &str) -> Result<Vec<String>> {
    static UPDIR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^((?:(?:\\\.){1,2}+/)+)").unwrap());
    let mut out = vec![];
    for p in packages {
        let Some(install) = &p.path else { continue };
        let a = autoload(p, dev_mode);
        for path in a.get("exclude-from-classmap").map_or_default(strings) {
            let trimmed = path.replace('\\', "/");
            let quoted =
                String::from_utf8_lossy(&preg_quote(trimmed.trim_matches('/').as_bytes(), None))
                    .into_owned();
            let mut collapsed = String::new();
            for ch in quoted.chars() {
                if !(ch == '/' && collapsed.ends_with('/')) {
                    collapsed.push(ch);
                }
            }
            let mut pattern = strtr(&collapsed, &[("\\*\\*", ".+?"), ("\\*", "[^/]+?")]);
            let mut updir = String::new();
            if let Some(m) = UPDIR.find(pattern.as_bytes())? {
                updir = pattern[..m.end()].replace("\\.", ".");
                pattern = pattern[m.end()..].to_string();
            }
            let dir = if install.is_empty() {
                cwd.to_string()
            } else {
                install.clone()
            };
            let resolved = pun_tokens::php_compat::canonicalize(format!("{dir}/{updir}")).ok();
            out.extend(resolved.map(|r| {
                let quoted = preg_quote(r.to_string_lossy().as_bytes(), None);
                format!("{}/{pattern}($|/)", String::from_utf8_lossy(&quoted))
            }));
        }
    }
    Ok(out)
}

fn exclusion_regex(dir: &str, excluded: &[String], vendor: &str) -> Result<Option<Regex>> {
    let mut excluded = excluded.to_vec();
    if vendor.starts_with(&format!("{dir}/")) {
        excluded.push(
            String::from_utf8_lossy(&preg_quote(format!("{vendor}/").as_bytes(), None))
                .into_owned(),
        );
    }
    if excluded.is_empty() {
        return Ok(None);
    }
    if let Ok(real) = pun_tokens::php_compat::canonicalize(dir) {
        static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^(([^.+*?\[^\]$(){}=!<>|:\\#-]+|\\[.+*?\[^\]$(){}=!<>|:#-])*).*").unwrap()
        });
        let quote = |s: &str| String::from_utf8_lossy(&preg_quote(s.as_bytes(), None)).into_owned();
        let (real, plain) = (quote(&real.to_string_lossy()), quote(dir));
        let symlink = real != plain;
        let mut related = vec![];
        for pattern in excluded {
            let prefix = (PREFIX.captures(pattern.as_bytes())?)
                .map_or_else(String::new, |c| String::from_utf8_lossy(&c[1]).into_owned());
            let under = |d: &str| prefix.starts_with(d) || d.starts_with(&prefix);
            if under(&real) || (symlink && under(&plain)) {
                related.push(pattern);
            }
        }
        excluded = related;
    }
    if excluded.is_empty() {
        return Ok(None);
    }
    Ok(Some(Regex::new(&format!("({})", excluded.join("|")))?))
}

#[expect(
    clippy::too_many_lines,
    reason = "long; to be split (code audit: functions over 100 lines)"
)]
pub fn dump(
    base: &str,
    vendor: &str,
    root: &Map<String, Value>,
    installed: &[Installed],
    o: &Options,
) -> Result<Dumped> {
    let target = format!("{vendor}/composer");
    let root_name = root
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("__root__")
        .to_ascii_lowercase();
    let root_pkg = Pkg {
        name: root_name,
        json: root,
        path: Some(String::new()),
        root: true,
        dev: false,
    };
    let packages: Vec<Pkg> = (installed.iter())
        .filter(|p| o.dev_mode || !p.dev)
        .map(|p| Pkg {
            name: p.name(),
            json: &p.package,
            path: p.path.clone(),
            root: false,
            dev: p.dev,
        })
        .collect();
    let mut include = vec![];
    for p in std::iter::once(&root_pkg).chain(&packages) {
        let Some(install) = &p.path else { continue };
        for path in p.json.get("include-path").map_or_default(strings) {
            let path = path.trim_matches('/').to_string();
            include.push(if install.is_empty() {
                path
            } else {
                format!("{install}/{path}")
            });
        }
    }
    let mut sorted = sort_packages(packages);
    sorted.push(root_pkg);
    let sorted_refs: Vec<&Pkg> = sorted.iter().collect();
    let reversed: Vec<&Pkg> = sorted.iter().rev().collect();
    let mut psr0 = mapping(&reversed, "psr-0", o.dev_mode);
    let mut psr4 = mapping(&reversed, "psr-4", o.dev_mode);
    let classmap = mapping(&reversed, "classmap", o.dev_mode)
        .pop()
        .map_or_default(|e| e.1);
    psr0.sort_by(|a, b| b.0.as_bytes().cmp(a.0.as_bytes()));
    psr4.sort_by(|a, b| b.0.as_bytes().cmp(a.0.as_bytes()));
    let excluded = exclusions(&sorted_refs, o.dev_mode, base)?;

    let vendor_code = shortest_code(&target, vendor, true, false);
    let base_code = shortest_code(vendor, base, true, false).replace("__DIR__", "$vendorDir");
    let header = |name: &str| {
        format!(
            "<?php\n\n// {name} @generated by Composer\n\n$vendorDir = {vendor_code};\n$baseDir = {base_code};\n\n"
        )
    };
    let mut dumped = Dumped::default();

    let mut evaluated: HashMap<String, String> = HashMap::new();
    let mut map_file = |name: &str, entries: &[(String, Vec<String>)]| {
        let mut out = header(name);
        out.push_str("return array(\n");
        for (ns, paths) in entries {
            let mut codes = vec![];
            for p in paths {
                let (code, value) = path_code(base, vendor, p)?;
                evaluated.insert(p.clone(), value);
                codes.push(code);
            }
            writeln!(
                out,
                "    {} => array({}),",
                export_str(ns),
                codes.join(", ")
            )
            .unwrap();
        }
        out.push_str(");\n");
        Ok::<_, Error>(out)
    };
    let namespaces = map_file("autoload_namespaces.php", &psr0)?;
    let psr4_text = map_file("autoload_psr4.php", &psr4)?;

    // The scanner that found the classes is part of the key: another Pun build may find others.
    let scanner = std::env::current_exe()
        .map_or_default(|exe| crate::fingerprint::stamp(&exe, true).to_string());
    let cache = o.class_cache.as_deref().map(|dir| ClassCache {
        dir,
        packages: (installed.iter())
            // A source install is a checkout that may have moved on from its dist archive.
            .filter(|p| {
                p.package
                    .0
                    .get("installation-source")
                    .is_none_or(|s| s == "dist")
            })
            .filter_map(|p| {
                let id = crate::store::Store::key(&p.package)?;
                Some((
                    p.path.clone()?,
                    format!("{}\0{}\0{id}\0{scanner}", p.name(), p.package.version()),
                ))
            })
            .collect(),
    });
    let cached =
        |dir: &str, exclude: Option<&Regex>| cache.as_ref().and_then(|c| c.file(dir, exclude));
    let mut map = ClassMap::default();
    let cwd = base.to_string();
    let mut jobs = vec![];
    for given in &classmap {
        let dir = normalize(&if is_absolute(given) {
            given.clone()
        } else {
            format!("{base}/{given}")
        });
        let exclude = exclusion_regex(&dir, &excluded, vendor)?;
        jobs.push(Job {
            cache: cached(&dir, exclude.as_ref()),
            dir,
            given: given.clone(),
            exclude,
            rule: Rule::Classmap,
            namespace: String::new(),
        });
    }
    if o.scan != Scan::Classmap {
        let mut scan: Vec<(String, Vec<_>)> = vec![];
        for (rule, entries) in [(Rule::Psr4, &psr4), (Rule::Psr0, &psr0)] {
            for (ns, paths) in entries {
                match scan.iter_mut().find(|s| s.0 == *ns) {
                    Some(s) => s.1.push((rule, paths.clone())),
                    None => scan.push((ns.clone(), vec![(rule, paths.clone())])),
                }
            }
        }
        scan.sort_by(|a, b| b.0.as_bytes().cmp(a.0.as_bytes()));
        for (ns, groups) in scan {
            for (rule, paths) in groups {
                for dir in paths {
                    let dir = normalize(&if is_absolute(&dir) {
                        dir
                    } else {
                        format!("{base}/{dir}")
                    });
                    if Path::new(&dir).is_dir() {
                        let exclude = exclusion_regex(&dir, &excluded, vendor)?;
                        jobs.push(Job {
                            cache: cached(&dir, exclude.as_ref()),
                            given: dir.clone(),
                            dir,
                            exclude,
                            rule,
                            namespace: ns.clone(),
                        });
                    }
                }
            }
        }
    }
    map.scan_all(&jobs, &cwd)?;
    map.set(
        "Composer\\InstalledVersions",
        &format!("{vendor}/composer/InstalledVersions.php"),
    );
    for (class, paths) in map.ambiguous_reported() {
        let first = map.path_of(class).unwrap_or_default();
        let others = paths.join("\", \"");
        dumped.warnings.push(if paths.len() > 1 {
            format!("Ambiguous class resolution, \"{class}\" was found {}x: in \"{first}\" and \"{others}\", the first will be used.", paths.len() + 1)
        } else {
            format!("Ambiguous class resolution, \"{class}\" was found in both \"{first}\" and \"{others}\", the first will be used.")
        });
    }
    let violations: Vec<String> = (map.violations.iter())
        .filter(|(path, _)| !(path == vendor || path.starts_with(&format!("{vendor}/"))))
        .map(|v| v.1.clone())
        .collect();
    dumped.warnings.extend(violations);
    let classes = map.sorted();
    dumped.classes = classes.len();
    let mut classmap_text = header("autoload_classmap.php");
    classmap_text.push_str("return array(\n");
    let mut class_values = vec![];
    for (class, path) in &classes {
        let (code, value) = path_code(base, vendor, path)?;
        writeln!(classmap_text, "    {} => {code},", export_str(class)).unwrap();
        class_values.push((class.clone(), value));
    }
    classmap_text.push_str(");\n");

    let mut files: Vec<FileEntry> = vec![];
    for p in &sorted {
        let Some(install) = &p.path else { continue };
        for path in autoload(p, o.dev_mode).get("files").map_or_default(strings) {
            let relative = if install.is_empty() {
                path.clone()
            } else {
                format!("{install}/{path}")
            };
            let (code, value) = path_code(base, vendor, &relative)?;
            let id = md5(format!("{}:{path}", p.name).as_bytes());
            match files.iter_mut().find(|f| f.id == id) {
                Some(f) => (f.code, f.path) = (code, value),
                None => files.push(FileEntry {
                    id,
                    code,
                    path: value,
                }),
            }
        }
    }
    let files_text = (!files.is_empty()).then(|| {
        let mut out = header("autoload_files.php");
        out.push_str("return array(\n");
        for f in &files {
            writeln!(out, "    {} => {},", export_str(&f.id), f.code).unwrap();
        }
        out.push_str(");\n");
        out
    });

    let mut include_text = None;
    if !include.is_empty() {
        let mut out = header("include_paths.php");
        out.push_str("return array(\n");
        for p in &include {
            writeln!(out, "    {},", path_code(base, vendor, p)?.0).unwrap();
        }
        out.push_str(");\n");
        include_text = Some(out);
    }

    let platform = match (&o.ignore_platform, o.platform_check) {
        (IgnorePlatform::All, _) | (_, PlatformCheck::Off) => None,
        (ignored, check) => platform_check(&sorted, check == PlatformCheck::Full, ignored),
    };

    let maps = StaticMaps {
        psr0: &psr0,
        psr4: &psr4,
        evaluated: &evaluated,
        classes: &class_values,
        files: &files,
    };
    let static_text = static_file(&o.suffix, &target, vendor, base, &maps);
    let loads = Loads {
        include_paths: include_text.is_some(),
        files: files_text.is_some(),
        platform_check: platform.is_some(),
    };
    let real = real_file(o, &loads);
    let autoload_php = format!(
        "<?php\n\n// autoload.php @generated by Composer\n\nif (PHP_VERSION_ID < 50600) {{\n    if (!headers_sent()) {{\n        header('HTTP/1.1 500 Internal Server Error');\n    }}\n    $err = 'Composer 2.3.0 dropped support for autoloading on PHP <5.6 and you are running '.PHP_VERSION.', please upgrade PHP or use Composer 2.2 LTS via \"composer self-update --2.2\". Aborting.'.PHP_EOL;\n    if (!ini_get('display_errors')) {{\n        if (PHP_SAPI === 'cli' || PHP_SAPI === 'phpdbg') {{\n            fwrite(STDERR, $err);\n        }} elseif (!headers_sent()) {{\n            echo $err;\n        }}\n    }}\n    throw new RuntimeException($err);\n}}\n\nrequire_once __DIR__ . '/composer/autoload_real.php';\n\nreturn ComposerAutoloaderInit{}::getLoader();\n",
        o.suffix
    );
    dumped_push(
        &mut dumped,
        &target,
        base,
        "autoload_namespaces.php",
        Some(namespaces),
    );
    dumped_push(
        &mut dumped,
        &target,
        base,
        "autoload_psr4.php",
        Some(psr4_text),
    );
    dumped_push(
        &mut dumped,
        &target,
        base,
        "autoload_classmap.php",
        Some(classmap_text),
    );
    dumped_push(
        &mut dumped,
        &target,
        base,
        "include_paths.php",
        include_text,
    );
    dumped_push(&mut dumped, &target, base, "autoload_files.php", files_text);
    dumped_push(
        &mut dumped,
        &target,
        base,
        "autoload_static.php",
        Some(static_text),
    );
    dumped_push(&mut dumped, &target, base, "platform_check.php", platform);
    dumped.files.push(DumpedFile {
        path: rel(base, &format!("{vendor}/autoload.php")),
        bytes: Some(autoload_php.into_bytes()),
    });
    dumped_push(&mut dumped, &target, base, "autoload_real.php", Some(real));
    dumped_push(
        &mut dumped,
        &target,
        base,
        "ClassLoader.php",
        Some(include_str!("../assets/ClassLoader.php").into()),
    );
    dumped_push(
        &mut dumped,
        &target,
        base,
        "LICENSE",
        Some(include_str!("../assets/LICENSE").into()),
    );
    Ok(dumped)
}

fn dumped_push(dumped: &mut Dumped, target: &str, base: &str, name: &str, body: Option<String>) {
    dumped.files.push(DumpedFile {
        path: rel(base, &format!("{target}/{name}")),
        bytes: body.map(|b| crate::classmap::restore(&b).into_owned()),
    });
}

fn rel(base: &str, path: &str) -> String {
    path.strip_prefix(&format!("{base}/"))
        .unwrap_or(path)
        .to_string()
}

fn platform_check(packages: &[Pkg], full: bool, ignored: &IgnorePlatform) -> Option<String> {
    let mut lowest = Bound::zero();
    let mut php64 = false;
    let mut providers: HashMap<String, Vec<String>> = HashMap::new();
    for p in packages {
        for kind in ["replace", "provide"] {
            for (target, constraint) in p.links(kind) {
                if let Some(ext) = target.strip_prefix("ext-") {
                    providers
                        .entry(ext.to_string())
                        .or_default()
                        .push(constraint);
                }
            }
        }
    }
    let mut extensions: Vec<(String, String)> = vec![];
    for p in packages {
        if p.dev {
            continue;
        }
        for (target, constraint) in p.links("require") {
            if ignored.ignores(&target) {
                continue;
            }
            if (target == "php" || target == "php-64bit")
                && let Ok(b) = lower_bound(&constraint)
                && b.above(&lowest)
            {
                lowest = b;
            }
            php64 |= target == "php-64bit";
            if full && let Some(ext) = target.strip_prefix("ext-") {
                if providers.contains_key(ext) {
                    continue;
                }
                let ext = if ext == "zend-opcache" {
                    "zend opcache"
                } else {
                    ext
                };
                let quoted = export_str(ext);
                let line = if ext == "pcntl" || ext == "readline" {
                    format!(
                        "PHP_SAPI !== 'cli' || extension_loaded({quoted}) || $missingExtensions[] = {quoted};\n"
                    )
                } else {
                    format!("extension_loaded({quoted}) || $missingExtensions[] = {quoted};\n")
                };
                if !extensions.iter().any(|e| e.0 == quoted) {
                    extensions.push((quoted, line));
                }
            }
        }
    }
    extensions.sort_by(|a, b| a.0.cmp(&b.0));
    let mut php = String::new();
    if !lowest.is_zero() {
        let op = if lowest.inclusive { ">=" } else { ">" };
        let chunks: Vec<&str> = lowest.version.split(['.', '-']).collect();
        let n = |i: usize| chunks.get(i).map_or(0, |c| c.parse::<i64>().unwrap_or(0));
        let id = n(0) * 10000 + n(1) * 100 + n(2);
        let human = chunks.iter().take(3).copied().collect::<Vec<_>>().join(".");
        write!(php, "\nif (!(PHP_VERSION_ID {op} {id})) {{\n    $issues[] = 'Your Composer dependencies require a PHP version \"{op} {human}\". You are running ' . PHP_VERSION . '.';\n}}\n").unwrap();
    }
    if php64 {
        php.push_str("\nif (PHP_INT_SIZE !== 8) {\n    $issues[] = 'Your Composer dependencies require a 64-bit build of PHP.';\n}\n");
    }
    let mut exts = String::new();
    if !extensions.is_empty() {
        let lines: String = extensions.iter().map(|e| e.1.as_str()).collect();
        write!(exts, "\n$missingExtensions = array();\n{lines}\nif ($missingExtensions) {{\n    $issues[] = 'Your Composer dependencies require the following PHP extensions to be installed: ' . implode(', ', $missingExtensions) . '.';\n}}\n").unwrap();
    }
    if php.is_empty() && exts.is_empty() {
        return None;
    }
    Some(format!(
        "<?php\n\n// platform_check.php @generated by Composer\n\n$issues = array();\n{php}{exts}\nif ($issues) {{\n    if (!headers_sent()) {{\n        header('HTTP/1.1 500 Internal Server Error');\n    }}\n    if (!ini_get('display_errors')) {{\n        if (PHP_SAPI === 'cli' || PHP_SAPI === 'phpdbg') {{\n            fwrite(STDERR, 'Composer detected issues in your platform:' . PHP_EOL.PHP_EOL . implode(PHP_EOL, $issues) . PHP_EOL.PHP_EOL);\n        }} elseif (!headers_sent()) {{\n            echo 'Composer detected issues in your platform:' . PHP_EOL.PHP_EOL . str_replace('You are running '.PHP_VERSION.'.', '', implode(PHP_EOL, $issues)) . PHP_EOL.PHP_EOL;\n        }}\n    }}\n    throw new \\RuntimeException(\n        'Composer detected issues in your platform: ' . implode(' ', $issues)\n    );\n}}\n"
    ))
}

fn real_file(o: &Options, loads: &Loads) -> String {
    let s = &o.suffix;
    let prepend = if o.prepend { "true" } else { "false" };
    let mut f = format!(
        "<?php\n\n// autoload_real.php @generated by Composer\n\nclass ComposerAutoloaderInit{s}\n{{\n    private static $loader;\n\n    public static function loadClassLoader($class)\n    {{\n        if ('Composer\\Autoload\\ClassLoader' === $class) {{\n            require __DIR__ . '/ClassLoader.php';\n        }}\n    }}\n\n    /**\n     * @return \\Composer\\Autoload\\ClassLoader\n     */\n    public static function getLoader()\n    {{\n        if (null !== self::$loader) {{\n            return self::$loader;\n        }}\n\n"
    );
    if loads.platform_check {
        f.push_str("        require __DIR__ . '/platform_check.php';\n\n");
    }
    write!(f, "        spl_autoload_register(array('ComposerAutoloaderInit{s}', 'loadClassLoader'), true, {prepend});\n        self::$loader = $loader = new \\Composer\\Autoload\\ClassLoader(\\dirname(__DIR__));\n        spl_autoload_unregister(array('ComposerAutoloaderInit{s}', 'loadClassLoader'));\n\n").unwrap();
    if loads.include_paths {
        f.push_str("        $includePaths = require __DIR__ . '/include_paths.php';\n        $includePaths[] = get_include_path();\n        set_include_path(implode(PATH_SEPARATOR, $includePaths));\n\n");
    }
    write!(f, "        require __DIR__ . '/autoload_static.php';\n        call_user_func(\\Composer\\Autoload\\ComposerStaticInit{s}::getInitializer($loader));\n\n").unwrap();
    if o.scan == Scan::Authoritative {
        f.push_str("        $loader->setClassMapAuthoritative(true);\n");
    }
    let prefix = match &o.apcu {
        Apcu::Off => None,
        Apcu::Prefix(prefix) => Some(prefix.clone()),
        Apcu::RandomPrefix => {
            let seed = format!("{s}{:?}", std::time::SystemTime::now());
            Some(md5(seed.as_bytes())[..20].to_string())
        }
    };
    if let Some(prefix) = prefix {
        writeln!(
            f,
            "        $loader->setApcuPrefix({});",
            export_str(&prefix)
        )
        .unwrap();
    }
    if o.use_include_path {
        f.push_str("        $loader->setUseIncludePath(true);\n");
    }
    writeln!(f, "        $loader->register({prepend});\n").unwrap();
    if loads.files {
        write!(f, "        $filesToLoad = \\Composer\\Autoload\\ComposerStaticInit{s}::$files;\n        $requireFile = \\Closure::bind(static function ($fileIdentifier, $file) {{\n            if (empty($GLOBALS['__composer_autoload_files'][$fileIdentifier])) {{\n                $GLOBALS['__composer_autoload_files'][$fileIdentifier] = true;\n\n                require $file;\n            }}\n        }}, null, null);\n        foreach ($filesToLoad as $fileIdentifier => $file) {{\n            $requireFile($fileIdentifier, $file);\n        }}\n\n").unwrap();
    }
    f.push_str("        return $loader;\n    }\n}\n");
    f
}

/// The entries of one PHP array literal in `autoload_static.php`.
type Entries = Vec<(Key, Php)>;

#[expect(
    clippy::too_many_lines,
    reason = "long; to be split (code audit: functions over 100 lines)"
)]
fn static_file(suffix: &str, target: &str, vendor: &str, base: &str, maps: &StaticMaps) -> String {
    static LEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^ *").unwrap());
    let StaticMaps {
        psr0,
        psr4,
        evaluated,
        classes,
        files,
    } = *maps;
    let values = |paths: &[String]| -> Vec<(Key, Php)> {
        ((0..).zip(paths))
            .map(|(i, p)| {
                (
                    Key::Int(i),
                    Php::Str(evaluated.get(p).cloned().unwrap_or_default()),
                )
            })
            .collect()
    };
    let group = |entries: &[(String, Vec<String>)], lengths: bool| -> (Entries, Entries, Entries) {
        let mut by_first: Vec<(Key, Vec<(Key, Php)>)> = vec![];
        let (mut dirs, mut fallback) = (vec![], vec![]);
        for (prefix, paths) in entries {
            let list = values(paths);
            if prefix.is_empty() {
                fallback = list;
                continue;
            }
            let first = Key::of(&prefix[..prefix.chars().next().map_or(0, char::len_utf8).max(1)]);
            let key = Key::Str(prefix.clone());
            let value = if lengths {
                Php::Int(i64::try_from(prefix.len()).expect("a prefix shorter than i64::MAX bytes"))
            } else {
                Php::Array(list.clone())
            };
            let at = (by_first.iter().position(|(k, _)| *k == first)).unwrap_or_else(|| {
                by_first.push((first, vec![]));
                by_first.len() - 1
            });
            by_first[at].1.retain(|(k, _)| *k != key);
            by_first[at].1.push((key.clone(), value));
            if lengths {
                dirs.retain(|(k, _)| *k != key);
                dirs.push((key, Php::Array(list)));
            }
        }
        let by_first = (by_first.into_iter())
            .map(|(k, items)| (k, Php::Array(items)))
            .collect();
        (by_first, dirs, fallback)
    };
    let (lengths4, dirs4, fallback4) = group(psr4, true);
    let (prefixes0, _, fallback0) = group(psr0, false);
    let files = files
        .iter()
        .map(|f| (Key::of(&f.id), Php::Str(f.path.clone())))
        .collect();
    let classes = classes
        .iter()
        .map(|(c, p)| (Key::of(c), Php::Str(p.clone())))
        .collect();
    let props: [(&str, Vec<(Key, Php)>); 7] = [
        ("files", files),
        ("prefixLengthsPsr4", lengths4),
        ("prefixDirsPsr4", dirs4),
        ("fallbackDirsPsr4", fallback4),
        ("prefixesPsr0", prefixes0),
        ("fallbackDirsPsr0", fallback0),
        ("classMap", classes),
    ];

    let rel_vendor = shortest_code(target, vendor, true, true);
    let rel_base = shortest_code(target, base, true, true);
    let abs = |p: &str| {
        let quoted = export_str(&format!("{}/", p.trim_end_matches(['\\', '/'])));
        format!(" => {}", &quoted[..quoted.len() - 1])
    };
    let pairs = [
        (abs(vendor), format!(" => {rel_vendor} . '/")),
        (
            abs(&format!("phar://{vendor}")),
            format!(" => 'phar://' . {rel_vendor} . '/"),
        ),
        (abs(base), format!(" => {rel_base} . '/")),
        (
            abs(&format!("phar://{base}")),
            format!(" => 'phar://' . {rel_base} . '/"),
        ),
    ];
    let pair_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut out = format!(
        "<?php\n\n// autoload_static.php @generated by Composer\n\nnamespace Composer\\Autoload;\n\nclass ComposerStaticInit{suffix}\n{{\n"
    );
    let mut init = String::new();
    for (prop, value) in props {
        if value.is_empty() {
            continue;
        }
        let text = strtr(&var_export(&Php::Array(value)), &pair_refs);
        let mut indented = String::new();
        for line in text.split('\n') {
            let spaces = LEADING
                .find(line.as_bytes())
                .ok()
                .flatten()
                .map_or(0, |m| m.end());
            let line = format!(
                "    {}{}{}",
                &line[..spaces],
                &line[..spaces],
                &line[spaces..]
            );
            if !indented.is_empty() {
                indented.push('\n');
            }
            indented.push_str(line.trim_end_matches(' '));
        }
        write!(
            out,
            "    public static ${prop} = {};\n\n",
            indented.trim_start()
        )
        .unwrap();
        if prop != "files" {
            writeln!(
                init,
                "            $loader->{prop} = ComposerStaticInit{suffix}::${prop};"
            )
            .unwrap();
        }
    }
    write!(out, "    public static function getInitializer(ClassLoader $loader)\n    {{\n        return \\Closure::bind(function () use ($loader) {{\n{init}\n        }}, null, ClassLoader::class);\n    }}\n}}\n").unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(manifests: &[Map<String, Value>]) -> Vec<String> {
        let packages = (manifests.iter())
            .map(|json| Pkg {
                name: json["name"].as_str().unwrap().to_string(),
                json,
                path: None,
                root: false,
                dev: false,
            })
            .collect();
        sort_packages(packages)
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    /// Weights worked by hand from Composer's `PackageSorter`, which weighs the packages in the order given: c = 0
    /// (no users); b weighs its user a, whose users are c (0) and b (0, being weighed), so a = -2 and b = -3.
    #[test]
    fn sorts_by_weight_through_a_cycle() {
        let m = |v: Value| v.as_object().unwrap().clone();
        let manifests = [
            m(json!({"name": "c", "require": {"a": "*"}})),
            m(json!({"name": "b", "require": {"a": "*"}})),
            m(json!({"name": "a", "require": {"b": "*"}})),
        ];
        assert_eq!(names(&manifests), ["b", "a", "c"]);
    }

    /// A chain of 100,000 packages, each required by the next, sorts on a test thread's 2 MiB stack: package k
    /// weighs -(n - 1 - k), so the chain comes out in order.
    #[test]
    fn a_deep_chain_does_not_overflow_the_stack() {
        let n = 100_000;
        let manifests: Vec<Map<String, Value>> = (0..n)
            .rev()
            .map(|k| {
                let mut m = json!({"name": format!("p/{k}")});
                if k > 0 {
                    m["require"] = json!({ format!("p/{}", k - 1): "*" });
                }
                m.as_object().unwrap().clone()
            })
            .collect();
        let sorted = names(&manifests);
        assert_eq!(sorted.len(), n);
        assert!(
            (sorted.iter())
                .enumerate()
                .all(|(k, name)| *name == format!("p/{k}"))
        );
    }
}
