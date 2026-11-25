use super::filters::{Advisory, ListEntry};
use super::package::{Origin, Package, load_with, stability_value};
use super::pool::{Found, Loaded, Repository, Stabilities};
use crate::constraint::Constraint;
use crate::error::{Context, Result, bail};
use crate::fetch::{Fetched, Fetcher, redact};
use crate::version::{branch_alias, is_platform, normalize, stability_of};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub const PACKAGIST: &str = "https://repo.packagist.org";

pub struct ComposerRepository<'a> {
    fetcher: &'a Fetcher,
    url: String,
    cache: Option<PathBuf>,
    metadata_url: String,
    notify: Option<String>,
    /// Whether the root advertises policy data in its per-package files (`filter.metadata` and
    /// `security-advisories.metadata`): Composer then reads those files for every package in the pool, not only the
    /// ones this repository supplied.
    filter_metadata: bool,
    advisory_metadata: bool,
    files: HashMap<String, Option<File>>,
}

#[derive(Deserialize)]
struct File {
    packages: Option<Box<RawValue>>,
    minified: Option<String>,
    #[serde(rename = "security-advisories")]
    advisories: Option<Value>,
    filter: Option<Value>,
}

#[expect(
    clippy::option_option,
    reason = "None: the cached file is not JSON; Some(None): it has no stamp"
)]
fn last_modified(text: &[u8]) -> Option<Option<String>> {
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(rename = "last-modified")]
        modified: Option<Value>,
    }
    let stamp: Stamp = serde_json::from_slice(text).ok()?;
    Some(stamp.modified.and_then(|m| m.as_str().map(String::from)))
}

fn object(text: &[u8], url: &str) -> Result<Map<String, Value>> {
    match serde_json::from_slice(text)
        .with_context(|| format!("{} is not valid JSON", redact(url)))?
    {
        Value::Object(m) => Ok(m),
        _ => bail!("{} does not hold a JSON object", redact(url)),
    }
}

/// A downloaded metadata file as the cache keeps it: the object with its `last-modified` stamp, for the next
/// request's If-Modified-Since. The stamp is appended to the text, so the document is not built and written again;
/// a body that is not a plain object, or already has the key, goes through the document.
fn stamped(body: Vec<u8>, modified: Option<String>, url: &str) -> Result<Vec<u8>> {
    #[derive(Deserialize)]
    struct Top {
        #[serde(rename = "last-modified")]
        stamp: Option<serde::de::IgnoredAny>,
    }
    let text = body.trim_ascii();
    let plain = text.first() == Some(&b'{')
        && serde_json::from_slice::<Top>(text).is_ok_and(|t| t.stamp.is_none());
    if !plain {
        let mut data = object(&body, url)?;
        if let Some(m) = modified {
            data.insert("last-modified".into(), m.into());
        }
        return Ok(Value::Object(data).to_string().into_bytes());
    }
    let Some(m) = modified else { return Ok(body) };
    let inner = text[1..text.len() - 1].trim_ascii();
    let mut out = Vec::with_capacity(text.len() + m.len() + 20);
    out.extend_from_slice(&text[..text.len() - 1]);
    if !inner.is_empty() {
        out.push(b',');
    }
    out.extend_from_slice(b"\"last-modified\":");
    out.extend_from_slice(serde_json::to_string(&m)?.as_bytes());
    out.push(b'}');
    Ok(out)
}

fn cache_name(url: &str) -> String {
    url.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn canonical(base: &str, url: &str) -> String {
    if !url.starts_with('/') {
        return url.to_string();
    }
    if url.starts_with("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        return format!("{scheme}:{url}");
    }
    let host_end = base
        .find("://")
        .and_then(|i| base[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(base.len());
    format!("{}{url}", &base[..host_end])
}

impl<'a> ComposerRepository<'a> {
    pub fn open(fetcher: &'a Fetcher, url: &str) -> ComposerRepository<'a> {
        let url = url.trim_end_matches('/').to_string();
        let cache = (fetcher.cache_dir()).map(|c| c.join("repo").join(cache_name(&url)));
        ComposerRepository {
            fetcher,
            url,
            cache,
            metadata_url: String::new(),
            notify: None,
            filter_metadata: false,
            advisory_metadata: false,
            files: HashMap::new(),
        }
    }

    fn root(&mut self) -> Result<()> {
        if !self.metadata_url.is_empty() {
            return Ok(());
        }
        let url = self.url.clone();
        let root = self
            .cached_or_fetch(&format!("{url}/packages.json"), "packages.json", 600)?
            .with_context(|| format!("{} has no packages.json", redact(&url)))?;
        let Some(metadata) = root.get("metadata-url").and_then(Value::as_str) else {
            bail!(
                "{} is a Composer repository without a metadata-url (the v1 format), which Pun does not read",
                redact(&url)
            );
        };
        self.metadata_url = canonical(&url, metadata);
        self.notify = (root.get("notify-batch").or_else(|| root.get("notify")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|n| canonical(&url, n));
        let flag =
            |key: &str| root.get(key).and_then(|f| f.get("metadata")) == Some(&Value::Bool(true));
        self.filter_metadata = flag("filter");
        self.advisory_metadata = flag("security-advisories");
        Ok(())
    }

    /// Fetches the per-package files of `names` not read yet, as Composer's `getFilter` and
    /// `getSecurityAdvisories` do for the packages in the pool: a package locked by a partial update, or supplied by
    /// another repository, is still checked against this repository's lists.
    fn fetch_missing(&mut self, names: &[String]) -> Result<()> {
        let jobs: Vec<(String, String, Option<PathBuf>)> = (names.iter())
            .filter(|n| !is_platform(n) && *n != "__root__" && !self.files.contains_key(*n))
            .map(|name| {
                let url = self.metadata_url.replace("%package%", name);
                let key = format!("provider-{}.json", name.replace('/', "~"));
                (name.clone(), url, self.cache_path(&key))
            })
            .collect();
        for (file, data) in fetch_all(self.fetcher, jobs) {
            self.files.insert(file, data?);
        }
        Ok(())
    }

    /// Reads the root and the files of `names` when the root says its files carry `which` policy data.
    fn policy_files(&mut self, names: &[String], which: fn(&Self) -> bool) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        self.root()?;
        if which(self) {
            self.fetch_missing(names)?;
        }
        Ok(())
    }

    fn cache_path(&self, key: &str) -> Option<PathBuf> {
        let key: String = (key.chars())
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() || ".$~_".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.cache.as_ref().map(|c| c.join(key))
    }

    fn read_cache(&self, key: &str) -> Option<(Vec<u8>, SystemTime)> {
        let path = self.cache_path(key)?;
        let text = std::fs::read(&path).ok()?;
        let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
        Some((text, modified))
    }

    fn cached_or_fetch(
        &self,
        url: &str,
        key: &str,
        max_age: u64,
    ) -> Result<Option<Map<String, Value>>> {
        let cached = self.read_cache(key);
        if let Some((text, at)) = &cached
            && at.elapsed().unwrap_or(Duration::MAX) < Duration::from_secs(max_age)
            && let Ok(data) = object(text, url)
        {
            return Ok(Some(data));
        }
        let text = fetch_file(self.fetcher, url, self.cache_path(key), cached.map(|c| c.0))?;
        text.map(|t| object(&t, url)).transpose()
    }
}

fn fetch_file(
    fetcher: &Fetcher,
    url: &str,
    cache: Option<PathBuf>,
    cached: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let (cached, since) = match cached.and_then(|t| last_modified(&t).map(|m| (t, m))) {
        Some((text, since)) => (Some(text), since),
        None => (None, None),
    };
    let response = match fetcher.metadata(url, since.as_deref()) {
        Ok(r) => r,
        Err(_) if cached.is_some() => return Ok(cached),
        Err(e) => return Err(e).with_context(|| format!("fetching {}", redact(url))),
    };
    match response {
        Fetched::NotModified => {
            let touch = |path: &PathBuf| {
                std::fs::File::options()
                    .append(true)
                    .open(path)
                    .and_then(|f| f.set_modified(SystemTime::now()))
            };
            let _ = cache.as_ref().map(touch);
            Ok(cached)
        }
        Fetched::NotFound => Ok(None),
        Fetched::Body(body, modified) => {
            let text = stamped(body, modified, url)?;
            if let Some(path) = cache {
                // The cache is best effort, as Composer's: a failed write only costs a download next time.
                let _ = crate::fetch::store(&path, &text);
            }
            Ok(Some(text))
        }
    }
}

const PARALLEL: usize = 48;

fn fetch_all(
    fetcher: &Fetcher,
    jobs: Vec<(String, String, Option<PathBuf>)>,
) -> Vec<(String, Result<Option<File>>)> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let jobs: Vec<std::sync::Mutex<Option<_>>> = jobs
        .into_iter()
        .map(|j| std::sync::Mutex::new(Some(j)))
        .collect();
    let results: Vec<std::sync::Mutex<Option<_>>> = (0..jobs.len())
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..jobs.len().min(PARALLEL) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    let (file, url, path) = job
                        .lock()
                        .expect("a job")
                        .take()
                        .expect("each job runs once");
                    let cached = path.as_ref().and_then(|p| std::fs::read(p).ok());
                    let result = fetch_file(fetcher, &url, path, cached).and_then(|text| {
                        (text.map(|t| serde_json::from_slice::<File>(&t)))
                            .transpose()
                            .with_context(|| format!("{} is not valid metadata", redact(&url)))
                    });
                    *results[i].lock().expect("a result") = Some((file, result));
                }
            });
        }
    });
    (results.into_iter())
        .map(|r| r.into_inner().expect("a result").expect("every job ran"))
        .collect()
}

pub fn expand(
    versions: &[Value],
    mut each: impl FnMut(&Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let mut current: Option<Map<String, Value>> = None;
    for v in versions {
        let Value::Object(v) = v else { continue };
        let next = match current.as_mut() {
            None => current.insert(v.clone()),
            Some(c) => {
                for (k, val) in v {
                    if val == "__unset" {
                        c.shift_remove(k);
                    } else {
                        c.insert(k.clone(), val.clone());
                    }
                }
                c
            }
        };
        each(next)?;
    }
    Ok(())
}

/// Whether the pool takes `version` of `name`, or the version a branch's `branch-alias` gives it (`alias`).
fn acceptable(
    c: &Constraint,
    name: &str,
    version: &str,
    alias: Option<String>,
    s: &Stabilities,
) -> bool {
    std::iter::once(version.to_string()).chain(alias).any(|v| {
        s.accepts(&[name], stability_value(stability_of(&v))) && c.matches(&Constraint::exact(&v))
    })
}

/// One version's fields in their order, each still JSON text: most versions in a metadata file fail the constraint,
/// and only their `version`, `version_normalized` and (for a branch) `extra` are read before that.
#[derive(Clone)]
struct RawVersion<'a>(Vec<(Key<'a>, &'a RawValue)>);

/// A field name, borrowed from the metadata unless it has escapes.
type Key<'a> = std::borrow::Cow<'a, str>;

/// A version holds a few dozen fields at most: a list searched in order beats a hash map here.
impl<'a> RawVersion<'a> {
    fn get(&self, key: &str) -> Option<&'a RawValue> {
        self.0.iter().find(|(k, _)| k == key).map(|f| f.1)
    }

    /// A repeated key keeps its first place and its last value, as `serde_json::Map` keeps it.
    fn set(&mut self, key: Key<'a>, value: &'a RawValue) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(field) => field.1 = value,
            None => self.0.push((key, value)),
        }
    }

    fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| k != key);
    }
}

impl<'de> Deserialize<'de> for RawVersion<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = RawVersion<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a version object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = RawVersion(Vec::with_capacity(map.size_hint().unwrap_or(16)));
                while let Some((k, v)) = map.next_entry::<KeyOf<'de>, &'de RawValue>()? {
                    fields.set(k.0, v);
                }
                Ok(fields)
            }
        }
        d.deserialize_map(Fields)
    }
}

/// A `Key` as serde reads it: borrowed when it can be (serde's `borrow` on a `Cow`).
#[derive(Deserialize)]
#[serde(transparent)]
struct KeyOf<'a>(#[serde(borrow)] std::borrow::Cow<'a, str>);

/// Calls `each` with the versions of `name` in a metadata file's `packages`, expanded from Composer's minified format
/// (each version lists only the fields that changed, `__unset` removing one) as `expand` does, in place. Entries that
/// are not objects are skipped. `None` when the file has no list for `name`.
fn raw_versions<'a>(
    packages: &'a RawValue,
    name: &str,
    minified: bool,
    mut each: impl FnMut(&RawVersion<'a>) -> Result<()>,
) -> Option<Result<()>> {
    let all: HashMap<String, &'a RawValue> = serde_json::from_str(packages.get()).ok()?;
    // A name the file has stays found even when its list is not one, so no other repository supplies it: Composer
    // marks the name found before it reads the versions.
    let Ok(listed) = serde_json::from_str::<Vec<&'a RawValue>>(all.get(name)?.get()) else {
        return Some(Ok(()));
    };
    let mut current: Option<RawVersion<'a>> = None;
    for v in listed {
        let Ok(v) = serde_json::from_str::<RawVersion<'a>>(v.get()) else {
            continue;
        };
        let next = match current.as_mut() {
            Some(previous) if minified => {
                for (k, val) in v.0 {
                    if val.get() == "\"__unset\"" {
                        previous.remove(&k);
                    } else {
                        previous.set(k, val);
                    }
                }
                previous
            }
            _ => current.insert(v),
        };
        if let Err(e) = each(next) {
            return Some(Err(e));
        }
    }
    Some(Ok(()))
}

/// A field of a raw version as a string; `None` when it is missing or not a string.
fn raw_str(version: &RawVersion<'_>, key: &str) -> Option<String> {
    serde_json::from_str(version.get(key)?.get()).ok()
}

/// The fields of a version `load` reads.
const LOADED_FIELDS: &[&str] = &[
    "name",
    "version",
    "version_normalized",
    "type",
    "abandoned",
    "require",
    "require-dev",
    "conflict",
    "provide",
    "replace",
    "extra",
];

/// The `fields` of a raw version, parsed, in the version's order.
fn materialize(version: &RawVersion<'_>, fields: &[&str]) -> Result<Map<String, Value>> {
    (version.0.iter())
        .filter(|(k, _)| fields.contains(&k.as_ref()))
        .map(|(k, v)| Ok((k.to_string(), serde_json::from_str(v.get())?)))
        .collect()
}

/// The JSON a package from this repository keeps (`Package::data`): the version's fields without
/// `transport-options`, and the repository's `notification-url` when the version has none, each value as the
/// metadata wrote it.
fn version_text(version: &RawVersion<'_>, notify: Option<&str>) -> Result<std::sync::Arc<str>> {
    let mut fields = vec![];
    for (k, v) in version.0.iter().filter(|(k, _)| k != "transport-options") {
        fields.push(format!("{}:{}", serde_json::to_string(k)?, v.get()));
    }
    if let Some(n) = notify.filter(|_| version.get("notification-url").is_none()) {
        fields.push(format!(
            "\"notification-url\":{}",
            serde_json::to_string(n)?
        ));
    }
    Ok(std::sync::Arc::from(format!("{{{}}}", fields.join(","))))
}

impl Repository for ComposerRepository<'_> {
    fn load(
        &mut self,
        index: usize,
        arena: &mut Vec<Package>,
        batch: &[(String, Constraint)],
        stabilities: &Stabilities,
        loaded: &Loaded,
    ) -> Result<Found> {
        let mut wanted: Vec<(String, String, &Constraint)> = vec![];
        for (name, constraint) in batch {
            if is_platform(name) || name == "__root__" {
                continue;
            }
            wanted.push((name.clone(), name.clone(), constraint));
            if stabilities.accepts(&[name], stability_value("dev")) {
                wanted.push((format!("{name}~dev"), name.clone(), constraint));
            }
        }
        if wanted.is_empty() {
            return Ok(Found {
                names: vec![],
                packages: vec![],
            });
        }
        self.root()?;
        let missing: Vec<&String> = (wanted.iter().map(|w| &w.0))
            .filter(|f| !self.files.contains_key(*f))
            .collect();
        let jobs: Vec<(String, String, Option<PathBuf>)> = (missing.iter())
            .map(|file| {
                let url = self.metadata_url.replace("%package%", file);
                let key = format!("provider-{}.json", file.replace('/', "~"));
                ((*file).clone(), url, self.cache_path(&key))
            })
            .collect();
        let fetched = fetch_all(self.fetcher, jobs);
        for (file, data) in fetched {
            self.files.insert(file, data?);
        }
        let (url, notify) = (&self.url, &self.notify);
        let decoded: Vec<Option<Result<_>>> = (wanted.par_iter())
            .map(|(file, name, constraint)| {
                let data = self.files.get(file)?.as_ref()?;
                let minified = data.minified.as_deref() == Some("composer/2.0");

                let (mut local, mut order) = (vec![], vec![]);
                let mut take = |raw: &RawVersion<'_>| -> Result<()> {
                    let pretty = raw_str(raw, "version").unwrap_or_default();
                    let normalized = match raw_str(raw, "version_normalized") {
                        Some(v) if v != "9999999-dev" => v,
                        _ => normalize(&pretty)?,
                    };
                    let known = loaded.get(name).is_some_and(|l| l.contains(&normalized));
                    let branch = pretty.starts_with("dev-") || pretty.ends_with("-dev");
                    let alias = if branch && !known {
                        branch_alias(&materialize(raw, &["version", "extra"])?).map(|a| a.0)
                    } else {
                        None
                    };
                    if known || !acceptable(constraint, name, &normalized, alias, stabilities) {
                        return Ok(());
                    }
                    // The fields `load` reads; the rest stays text.
                    let read = materialize(raw, LOADED_FIELDS)?;
                    let text = version_text(raw, notify.as_deref())?;
                    let loaded = load_with(&read, text, Origin::Repository(index), &mut local)
                        .with_context(|| {
                            format!("Could not load packages in {} from {file}", redact(url))
                        })?;
                    order.extend(loaded);
                    Ok(())
                };
                let expanded = raw_versions(data.packages.as_ref()?, name, minified, &mut take)?;
                Some(expanded.map(|()| (local, order)))
            })
            .collect();
        let mut names = vec![];
        let mut packages = vec![];
        for ((_, name, _), decoded) in wanted.iter().zip(decoded) {
            let Some(decoded) = decoded else {
                continue;
            };
            if !names.contains(name) {
                names.push(name.clone());
            }
            let (local, order) = decoded?;
            let offset = arena.len();
            arena.extend(local.into_iter().map(|mut p| {
                p.alias_of = p.alias_of.map(|of| of + offset);
                p
            }));
            packages.extend(order.into_iter().map(|p| p + offset));
        }
        Ok(Found { names, packages })
    }

    fn advisories(&mut self, names: &[String]) -> Result<Vec<Advisory>> {
        self.policy_files(names, |r| r.advisory_metadata)?;
        let mut out = vec![];
        for name in names {
            let Some(Some(data)) = self.files.get(name) else {
                continue;
            };
            for a in (data
                .advisories
                .as_ref()
                .and_then(Value::as_array)
                .into_iter()
                .flatten())
            .filter_map(Value::as_object)
            {
                out.push(Advisory::parse(name, a)?);
            }
        }
        Ok(out)
    }

    fn list_entries(&mut self, names: &[String]) -> Result<Vec<ListEntry>> {
        self.policy_files(names, |r| r.filter_metadata)?;
        let mut out = vec![];
        for name in names {
            let Some(Some(data)) = self.files.get(name) else {
                continue;
            };
            for (list, entries) in data
                .filter
                .as_ref()
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                for e in entries
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_object)
                {
                    let mut e = e.clone();
                    e.entry("package").or_insert_with(|| name.clone().into());
                    if e.contains_key("constraint") {
                        out.push(ListEntry::parse(list, &e)?);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expands_as_composer() {
        let versions = [
            json!({"name": "a/a", "version": "2.0", "require": {"php": ">=8"}, "extra": {"x": 1}}),
            json!({"version": "1.0", "extra": "__unset"}),
            json!({"version": "0.9", "require": {"php": ">=7"}}),
        ];
        let mut got = vec![];
        expand(&versions, |v| {
            got.push(Value::Object(v.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            got,
            [
                json!({"name": "a/a", "version": "2.0", "require": {"php": ">=8"}, "extra": {"x": 1}}),
                json!({"name": "a/a", "version": "1.0", "require": {"php": ">=8"}}),
                json!({"name": "a/a", "version": "0.9", "require": {"php": ">=7"}}),
            ]
        );
    }

    /// `raw_versions` reads what `expands_as_composer` expands: the same versions, an entry that is not an object
    /// skipped, and a field name with an escape read as its text.
    #[test]
    fn raw_versions_expand_as_composer() {
        let packages = RawValue::from_string(
            r#"{"a/a": [{"name": "a/a", "version": "2.0", "require": {"php": ">=8"}, "extra": {"x": 1}}, "oops", {"version": "1.0", "extra": "__unset"}, {"version": "0.9", "requir\u0065": {"php": ">=7"}}]}"#
                .to_string(),
        )
        .unwrap();
        let mut got = vec![];
        let expanded = raw_versions(&packages, "a/a", true, |v| {
            let fields: Vec<String> = (v.0.iter())
                .map(|(k, val)| format!("{k}={}", val.get()))
                .collect();
            got.push(fields.join(" "));
            Ok(())
        });
        assert!(matches!(expanded, Some(Ok(()))));
        assert_eq!(
            got,
            [
                r#"name="a/a" version="2.0" require={"php": ">=8"} extra={"x": 1}"#,
                r#"name="a/a" version="1.0" require={"php": ">=8"}"#,
                r#"name="a/a" version="0.9" require={"php": ">=7"}"#,
            ]
        );
        assert!(raw_versions(&packages, "b/b", true, |_| Ok(())).is_none());
    }

    /// The cached copy of a download keeps the body's text and adds the stamp once, as the document would hold it.
    #[test]
    fn stamps_a_download_for_the_cache() {
        let m = || Some("Tue, 01 Sep 2026 10:00:00 GMT".to_string());
        let stamp = |body: &str, m: Option<String>| {
            String::from_utf8(stamped(body.as_bytes().to_vec(), m, "u").unwrap()).unwrap()
        };
        assert_eq!(
            stamp(" {\"packages\": {\"a\\/b\": []}}\n", m()),
            r#"{"packages": {"a\/b": []},"last-modified":"Tue, 01 Sep 2026 10:00:00 GMT"}"#
        );
        assert_eq!(
            stamp("{ }", m()),
            r#"{ "last-modified":"Tue, 01 Sep 2026 10:00:00 GMT"}"#
        );
        assert_eq!(
            stamp(r#"{"last-modified": "old", "x": 1}"#, m()),
            r#"{"last-modified":"Tue, 01 Sep 2026 10:00:00 GMT","x":1}"#
        );
        assert_eq!(stamp(r#"{"x": 1}"#, None), r#"{"x": 1}"#);
        assert_eq!(
            stamp(r#"{"last-modified": "old"}"#, None),
            r#"{"last-modified":"old"}"#
        );
        assert_eq!(
            stamped(b"[1]".to_vec(), m(), "u").unwrap_err().to_string(),
            "u does not hold a JSON object"
        );
    }

    #[test]
    fn names_cache_directories_as_composer() {
        assert_eq!(cache_name(PACKAGIST), "https---repo.packagist.org");
        assert_eq!(
            cache_name("https://repo.example.com:8080/x"),
            "https---repo.example.com-8080-x"
        );
        assert_eq!(
            canonical(PACKAGIST, "/p2/%package%.json"),
            "https://repo.packagist.org/p2/%package%.json"
        );
        assert_eq!(canonical("https://a.b/c", "//cdn.b/p"), "https://cdn.b/p");
        assert_eq!(canonical(PACKAGIST, "https://x/y"), "https://x/y");
    }

    const MODIFIED: &str = "Mon, 02 Nov 2026 10:00:00 GMT";

    type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>, Option<String>)>>>;

    fn serve(
        extra: &'static [(&'static str, &'static str)],
    ) -> (String, Seen, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use std::io::{Read, Write};
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/composer-repo");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Seen::default();
        let down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (log, off) = (seen.clone(), down.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut head = vec![];
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split(' ').nth(1).unwrap_or_default().to_string();
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(": ")?;
                        k.eq_ignore_ascii_case(name).then(|| v.to_string())
                    })
                };
                let since = header("if-modified-since");
                log.lock()
                    .unwrap()
                    .push((path.clone(), since.clone(), header("authorization")));
                let added = extra.iter().find(|(p, _)| *p == path);
                let file = (added.map(|(_, body)| body.as_bytes().to_vec()))
                    .or_else(|| std::fs::read(root.join(path.trim_start_matches('/'))).ok());
                let modified = if added.is_some() {
                    String::new()
                } else {
                    format!("Last-Modified: {MODIFIED}\r\n")
                };
                let status = if off.load(std::sync::atomic::Ordering::Relaxed) {
                    "500 Internal Server Error"
                } else if path.starts_with("/busy/") {
                    "503 Service Unavailable"
                } else if path.starts_with("/forbidden/") {
                    "403 Forbidden"
                } else if file.is_none() {
                    "404 Not Found"
                } else if since.as_deref() == Some(MODIFIED) {
                    "304 Not Modified"
                } else {
                    "200 OK"
                };
                let body = file.filter(|_| status == "200 OK").unwrap_or_default();
                let response = [
                    format!(
                        "HTTP/1.1 {status}\r\n{modified}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes(),
                    body,
                ]
                .concat();
                stream.write_all(&response).unwrap();
            }
        });
        (base, seen, down)
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pun-repo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "a test helper called with temporaries"
    )]
    fn manifest(url: &str, require: Value) -> Map<String, Value> {
        let m = json!({
            "name": "acme/app",
            "require": require,
            "repositories": [{"type": "composer", "url": url}, {"packagist.org": false}],
            "config": {"secure-http": false, "platform": {"php": "8.5.0"}}
        });
        m.as_object().unwrap().clone()
    }

    fn update(fetcher: &Fetcher, manifest: &Map<String, Value>) -> Result<String, String> {
        use super::super::{Options, locker, repos, resolve};
        let options = Options::default();
        let no_env = |_: &str| None;
        let run = || -> Result<String> {
            let repositories =
                repos::open(std::path::Path::new("."), manifest, None, fetcher, &no_env)?;
            let php = json!({"name": "php", "version": "8.5.0"});
            let platform = vec![php.as_object().unwrap().clone()];
            let resolved = resolve(manifest, platform, repositories, None, &options, &no_env)?;
            let resolution = resolved.map_err(|_| crate::error::Error::msg("no solution"))?;
            Ok(locker::lock(&resolution, manifest, &options).to_composer_lock())
        };
        run().map_err(|e| format!("{e:#}"))
    }

    fn resolve_with(
        fetcher: &Fetcher,
        manifest: &Map<String, Value>,
        lock: Option<&Value>,
        allow: &[&str],
    ) -> Result<String, String> {
        use super::super::{Options, locker, repos, resolve};
        let options = Options {
            allow_list: allow.iter().map(|a| (*a).to_string()).collect(),
            ..Options::default()
        };
        let lock = lock.map(|l| crate::lock::Lock::from_composer_lock(&l.to_string()).unwrap());
        let no_env = |_: &str| None;
        let run = || -> Result<String> {
            let repositories =
                repos::open(std::path::Path::new("."), manifest, None, fetcher, &no_env)?;
            let php = json!({"name": "php", "version": "8.5.0"});
            let platform = vec![php.as_object().unwrap().clone()];
            let resolved = resolve(
                manifest,
                platform,
                repositories,
                lock.as_ref(),
                &options,
                &no_env,
            )?;
            let resolution = resolved.map_err(|_| crate::error::Error::msg("no solution"))?;
            Ok(locker::lock(&resolution, manifest, &options).to_composer_lock())
        };
        run().map_err(|e| format!("{e:#}"))
    }

    /// The repository's malware list reaches a package the update keeps from the lock, and one another repository
    /// supplies, as Composer's `FilterListPoolFilter` asks every repository about every package in the pool.
    #[test]
    fn lists_cover_kept_and_foreign_packages() {
        let (base, seen, _) = serve(&[]);
        let fetcher = Fetcher::new(None, crate::fetch::Auth::default(), false);
        let manifest = manifest(
            &base,
            json!({"acme/insecure": "^1.0", "acme/listed": "^1.0"}),
        );
        let lock = |listed: &str| {
            json!({"content-hash": "x", "packages-dev": [], "packages": [
                {"name": "acme/insecure", "version": "1.0.0", "type": "library"},
                {"name": "acme/listed", "version": listed, "type": "library"}
            ]})
        };
        assert_eq!(
            resolve_with(
                &fetcher,
                &manifest,
                Some(&lock("1.0.1")),
                &["acme/insecure"]
            )
            .unwrap_err(),
            "no solution"
        );
        assert!(requests(&seen).contains(&("/p2/acme/listed.json".into(), None)));
        let kept = resolve_with(
            &fetcher,
            &manifest,
            Some(&lock("1.0.0")),
            &["acme/insecure"],
        )
        .unwrap();
        assert!(kept.contains("\"version\": \"1.0.0\""), "{kept}");

        let mut foreign = manifest;
        foreign["repositories"] = json!([
            {"type": "package", "package": [{"name": "acme/listed", "version": "1.0.1"}]},
            {"type": "composer", "url": base}, {"packagist.org": false}
        ]);
        foreign["require"] = json!({"acme/listed": "1.0.1"});
        assert_eq!(
            resolve_with(&fetcher, &foreign, None, &[]).unwrap_err(),
            "no solution"
        );
        requests(&seen);
    }

    fn requests(seen: &Seen) -> Vec<(String, Option<String>)> {
        let all = std::mem::take(&mut *seen.lock().unwrap());
        let mut all: Vec<_> = all.into_iter().map(|(p, s, _)| (p, s)).collect();
        all.sort();
        all
    }

    #[test]
    fn reads_a_composer_repository_as_composer() {
        let (base, seen, down) = serve(&[]);
        let dir = scratch("main");
        let fetcher = Fetcher::new(Some(dir.clone()), crate::fetch::Auth::default(), false);
        let require = json!({
            "php": ">=8.1",
            "acme/lib": "^1.0 || ^2.0",
            "acme/minified": "^2.0",
            "acme/devlib": "^1.0@dev",
            "acme/insecure": "^1.0",
            "acme/listed": "^1.0"
        });
        let manifest = manifest(&base, require);
        let want = include_str!("../../tests/fixtures/composer-repo.lock").replace("BASE", &base);
        let without_hash = |t: &str| {
            t.lines()
                .filter(|l| !l.contains("\"content-hash\""))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let got = update(&fetcher, &manifest).unwrap();
        assert_eq!(without_hash(&got), without_hash(&want));
        let p2 = |name: &str, since: Option<&str>| {
            (format!("/p2/acme/{name}.json"), since.map(String::from))
        };
        let files = [
            "devlib",
            "devlib~dev",
            "insecure",
            "lib",
            "listed",
            "minified",
            "util",
        ];
        let mut first: Vec<_> = files.iter().map(|f| p2(f, None)).collect();
        first.push(("/packages.json".into(), None));
        assert_eq!(requests(&seen), first);
        let cache = dir.join(format!("repo/{}", cache_name(&base)));
        let cached: Value =
            serde_json::from_slice(&std::fs::read(cache.join("provider-acme~lib.json")).unwrap())
                .unwrap();
        assert_eq!(cached["last-modified"], MODIFIED);

        std::fs::write(cache.join("provider-acme~util.json"), "not json").unwrap();
        assert_eq!(update(&fetcher, &manifest).unwrap(), got);
        let again: Vec<_> = (files.iter())
            .map(|f| p2(f, (!["devlib", "util"].contains(f)).then_some(MODIFIED)))
            .collect();
        assert_eq!(requests(&seen), again);

        let root = cache.join("packages.json");
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .append(true)
            .open(&root)
            .unwrap()
            .set_modified(old)
            .unwrap();
        update(&fetcher, &manifest).unwrap();
        assert!(requests(&seen).contains(&("/packages.json".into(), Some(MODIFIED.into()))));
        let touched = std::fs::metadata(&root).unwrap().modified().unwrap();
        assert!(touched.elapsed().unwrap() < Duration::from_secs(600));

        let lib_only = self::manifest(&base, json!({"acme/lib": "^1.0"}));
        #[cfg(unix)]
        let locked = {
            let readonly = dir.join("readonly");
            let repo = readonly.join(format!("repo/{}", cache_name(&base)));
            std::fs::create_dir_all(&repo).unwrap();
            let mode = |m| std::os::unix::fs::PermissionsExt::from_mode(m);
            std::fs::set_permissions(&repo, mode(0o555)).unwrap();
            let unwritable = Fetcher::new(Some(readonly), crate::fetch::Auth::default(), false);
            let locked = update(&unwritable, &lib_only).unwrap();
            std::fs::set_permissions(&repo, mode(0o755)).unwrap();
            assert_eq!(std::fs::read_dir(&repo).unwrap().count(), 0);
            requests(&seen);
            locked
        };
        #[cfg(windows)]
        let locked = update(&fetcher, &lib_only).unwrap();

        down.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(locked.contains("\"name\": \"Acme/Lib\""), "{locked}");
        assert_eq!(update(&fetcher, &lib_only).unwrap(), locked);
        assert_eq!(
            update(&fetcher, &manifest).unwrap_err(),
            format!(
                "fetching {base}/p2/acme/devlib.json: HTTP 500 from {base}/p2/acme/devlib.json"
            )
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one test walks every case of the behaviour it checks"
    )]
    fn repository_errors() {
        const FILES: &[(&str, &str)] = &[
            ("/v1/packages.json", r#"{"packages": []}"#),
            ("/list/packages.json", "[]"),
            ("/text/packages.json", "nope"),
            (
                "/bad/packages.json",
                r#"{"metadata-url": "/bad/%package%.json"}"#,
            ),
            ("/bad/acme/text.json", "{"),
            ("/bad/acme/minified.json", r#"{"minified": 5}"#),
            (
                "/bad/acme/empty.json",
                r#"{"packages": {"acme/other": []}}"#,
            ),
            (
                "/bad/acme/wrong.json",
                r#"{"packages": {"acme/wrong": "oops"}}"#,
            ),
            (
                "/bad/acme/both.json",
                r#"{"packages": {"acme/both": [{"name": "acme/both", "version": "1.0.0"}]}}"#,
            ),
            (
                "/bad/acme/both~dev.json",
                r#"{"packages": {"acme/both": [{"name": "acme/both", "version": "dev-main"}]}}"#,
            ),
            (
                "/bad/acme/version.json",
                r#"{"packages": {"acme/version": [{"name": "acme/version", "version": "not a version"}]}}"#,
            ),
            (
                "/bad/acme/link.json",
                r#"{"packages": {"acme/link": [{"name": "acme/link", "version": "1.0.0", "require": {"x/y": "nope"}}]}}"#,
            ),
        ];
        let (base, seen, _) = serve(FILES);
        let dir = scratch("errors");
        let origin = base.trim_start_matches("http://");
        let auth = format!(r#"{{"bearer": {{"{origin}": "t0k"}}}}"#);
        std::fs::write(dir.join("auth.json"), auth).unwrap();
        let fetcher = Fetcher::new(None, crate::fetch::Auth::load(&dir, None, None), false);
        let fails = |repo: &str, name: &str| {
            let require = Map::from_iter([(name.to_string(), Value::from("*"))]);
            let m = manifest(&format!("{base}/{repo}"), Value::Object(require));
            update(&fetcher, &m).unwrap_err()
        };
        assert_eq!(
            fails("v1", "acme/a"),
            format!(
                "{base}/v1 is a Composer repository without a metadata-url (the v1 format), which Pun does not read"
            )
        );
        assert_eq!(
            fails("none/", "acme/a"),
            format!("{base}/none has no packages.json")
        );
        assert_eq!(
            fails("list", "acme/a"),
            format!("{base}/list/packages.json does not hold a JSON object")
        );
        assert!(
            fails("text", "acme/a")
                .starts_with(&format!("{base}/text/packages.json is not valid JSON: ")),
        );
        assert!(
            fails("bad", "acme/text")
                .starts_with(&format!("{base}/bad/acme/text.json is not valid JSON: ")),
        );
        assert!(fails("bad", "acme/minified").starts_with(&format!(
            "{base}/bad/acme/minified.json is not valid metadata: "
        )),);
        assert_eq!(
            fails("bad", "acme/version"),
            "Invalid version string \"not a version\""
        );
        assert!(
            fails("bad", "acme/link").starts_with(&format!(
                "Could not load packages in {base}/bad from acme/link: Link constraint in acme/link requires > x/y should be a valid version constraint, got \"nope\""
            )),
        );
        assert_eq!(
            fails("forbidden", "acme/a"),
            format!(
                "fetching {base}/forbidden/packages.json: HTTP 403 from {base}/forbidden/packages.json"
            )
        );
        let all = std::mem::take(&mut *seen.lock().unwrap());
        assert!(all.iter().all(|r| r.2.as_deref() == Some("Bearer t0k")));
        assert_eq!(
            fails("busy", "acme/a"),
            format!("fetching {base}/busy/packages.json: HTTP 503 from {base}/busy/packages.json")
        );
        assert_eq!(requests(&seen).len(), 3);
        assert_eq!(fails("bad", "acme/empty"), "no solution");
        let mut m = manifest(&format!("{base}/bad"), json!({"acme/wrong": "*"}));
        m["repositories"] = json!([{"type": "composer", "url": format!("{base}/bad")},
            {"type": "package", "package": {"name": "acme/wrong", "version": "1.0.0"}}, {"packagist.org": false}]);
        assert_eq!(update(&fetcher, &m).unwrap_err(), "no solution");
        let m = manifest(&format!("{base}/bad"), json!({"acme/both": "*@dev"}));
        let locked = update(&fetcher, &m).unwrap();
        assert!(locked.contains("\"version\": \"1.0.0\""), "{locked}");
        requests(&seen);
        let m = manifest(&format!("{base}/none"), json!({"php": ">=8"}));
        assert!(update(&fetcher, &m).unwrap().contains("\"packages\": [],"));
        let m = manifest(&format!("{base}/none"), json!({"ext-absent": "*"}));
        assert_eq!(update(&fetcher, &m).unwrap_err(), "no solution");
        assert_eq!(requests(&seen), []);
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let m = manifest(&format!("http://{closed}"), json!({"acme/a": "*"}));
        let error = update(&fetcher, &m).unwrap_err();
        assert!(
            error.starts_with(&format!("fetching http://{closed}/packages.json: ")),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
