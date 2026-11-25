#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool reports on stdout and stderr"
)]

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run oracle (pint) and `pun migrate` + `pun check --fix` on each corpus project and diff the output.
    Format {
        /// Only these corpus projects (repeatable). Default: all in corpus/manifest.toml.
        #[arg(long)]
        corpus: Vec<String>,
        /// Enable only this fixer (options taken from the project's effective config).
        #[arg(long)]
        fixer: Option<String>,
        /// Run both tools with this preset (`--preset`) instead of the project's own; the scoreboard is left alone.
        #[arg(long)]
        preset: Option<String>,
        /// Number of unified diffs to print per project.
        #[arg(long, default_value_t = 3)]
        diffs: usize,
    },
}

#[derive(Deserialize)]
struct Manifest {
    project: Vec<Project>,
}

#[derive(Deserialize)]
struct Project {
    name: String,
}

struct Outcome {
    name: String,
    files: usize,
    identical: usize,
    differing: Vec<String>,
    oracle_changed: usize,
    pun_changed: usize,
}

/// The run's scratch copies under the temp dir, removed however the run ends (an error after `copy_tree` too).
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "long; to be split (code audit: functions over 100 lines)"
)]
fn main() -> Result<ExitCode> {
    // PUN_HARNESS_ROOT: run against another checkout layout (corpus/, .oracle/, scripts/, docs/).
    let root = std::env::var_os("PUN_HARNESS_ROOT")
        .map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            PathBuf::from,
        )
        .canonicalize()?;
    let Cmd::Format {
        corpus,
        fixer,
        preset,
        diffs,
    } = Cli::parse().cmd;
    let manifest: Manifest =
        toml::from_str(&fs::read_to_string(root.join("corpus/manifest.toml"))?)?;
    let pint = root.join(".oracle/pint/builds/pint");
    if !pint.exists() {
        bail!("{} missing: run scripts/oracle.sh", pint.display());
    }
    let pun =
        std::env::var_os("PUN_BIN").map_or_else(|| root.join("target/debug/pun"), PathBuf::from);
    if !pun.exists() {
        eprintln!("note: {} missing, treating pun as identity", pun.display());
    }

    let scratch = Scratch(private_dir(&std::env::temp_dir())?);
    let tmp = &scratch.0;
    let mut outcomes = vec![];
    for p in manifest
        .project
        .iter()
        .filter(|p| corpus.is_empty() || corpus.contains(&p.name))
    {
        // One directory under corpus/ and under the scratch directory, which is removed after the comparison.
        if !matches!(
            Path::new(&p.name).components().collect::<Vec<_>>()[..],
            [std::path::Component::Normal(_)]
        ) {
            bail!("corpus/manifest.toml: not a corpus name: {}", p.name);
        }
        let src = root.join("corpus").join(&p.name);
        if !src.join(".git").exists() {
            bail!("{} missing: run scripts/corpus.sh", src.display());
        }
        let (oracle_dir, pun_dir) = (
            tmp.join(&p.name).join("oracle"),
            tmp.join(&p.name).join("pun"),
        );
        copy_tree(&src, &oracle_dir)?;
        copy_tree(&src, &pun_dir)?;

        let mut args: Vec<String> = vec![];
        if let Some(name) = &fixer {
            let cfg = tmp.join(&p.name).join("single-fixer.json");
            fs::write(&cfg, single_fixer_config(&root, &src, name)?)?;
            args = vec!["--config".into(), cfg.display().to_string()];
        }
        if let Some(p) = &preset {
            args.extend(["--preset".into(), p.clone()]);
        }
        run(
            Command::new("php")
                .arg(&pint)
                .arg("--parallel")
                .args(&args)
                .arg(format!(
                    "--cache-file={}",
                    tmp.join(&p.name).join("pint.cache").display()
                ))
                .env("TMPDIR", tmp.join(&p.name))
                .current_dir(&oracle_dir),
            "pint",
        )?;
        if pun.exists() {
            let pun_cmd = |sub: &[&str]| {
                let mut c = Command::new(&pun);
                // Parity of the fixers, not of the result cache, which would fill the user's with scratch copies.
                c.env("PUN_IGNORE_PHP_CONSTRAINT", "1")
                    .env("PUN_NO_CACHE", "1")
                    .args(sub)
                    .current_dir(&pun_dir);
                c
            };
            // Pint reads no composer.lock. Hidden from migrate, it turns on no extension, whose lint rules
            // `check --fix` would apply on top of what pint.json asks for; put back for the comparison.
            let (lock, hidden) = (
                pun_dir.join("composer.lock"),
                tmp.join(&p.name).join("composer.lock"),
            );
            let had_lock = fs::rename(&lock, &hidden).is_ok();
            let mut migrate_args = args.clone();
            if fixer.is_none() && !src.join("pint.json").exists() {
                let defaults = tmp.join(&p.name).join("pint-defaults.json");
                fs::write(&defaults, "{}\n")?;
                migrate_args.splice(0..0, ["--config".into(), defaults.display().to_string()]);
            }
            run(
                pun_cmd(&["migrate", "--from", "pint"]).args(&migrate_args),
                "pun migrate",
            )?;
            if had_lock {
                fs::rename(&hidden, &lock)?;
            }
            run(&mut pun_cmd(&["check", "--fix"]), "pun")?;
        }

        let o = compare(&p.name, &src, &oracle_dir, &pun_dir)?;
        println!(
            "{}: {} files, {} identical, {} differing (pint changed {}, pun changed {})",
            o.name,
            o.files,
            o.identical,
            o.differing.len(),
            o.oracle_changed,
            o.pun_changed
        );
        for rel in o.differing.iter().take(diffs) {
            let a = String::from_utf8_lossy(&fs::read(oracle_dir.join(rel)).unwrap_or_default())
                .into_owned();
            let b = String::from_utf8_lossy(&fs::read(pun_dir.join(rel)).unwrap_or_default())
                .into_owned();
            print!(
                "{}",
                similar::TextDiff::from_lines(&a, &b)
                    .unified_diff()
                    .header(&format!("oracle/{rel}"), &format!("pun/{rel}"))
            );
        }
        outcomes.push(o);
        fs::remove_dir_all(tmp.join(&p.name)).ok();
    }
    drop(scratch);

    if fixer.is_none() && preset.is_none() {
        write_scoreboard(&root.join("context/reports/scoreboard.md"), &outcomes)?;
    }
    Ok(if outcomes.iter().any(|o| !o.differing.is_empty()) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// A new directory under `parent` that only this user can open. Creating it fails if the name exists, so it is never a
/// directory another user sharing the temporary directory made and filled with links.
fn private_dir(parent: &Path) -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .subsec_nanos();
    let dir = parent.join(format!("pun-harness-{}-{nanos}", std::process::id()));
    #[cfg(unix)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    #[cfg(windows)]
    let builder = fs::DirBuilder::new();
    builder
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("spawning {what}"))?;
    if !out.status.success() {
        eprintln!(
            "warning: {what} exited {}\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// pint.json with the project's finder options, preset "empty", and one rule configured as
/// in the project's effective rule list (or `true` if the project doesn't enable it).
fn single_fixer_config(root: &Path, src: &Path, name: &str) -> Result<String> {
    let pint_json = src.join("pint.json");
    let mut cmd = Command::new("php");
    cmd.arg(root.join("scripts/effective-rules.php"));
    let mut cfg = serde_json::Map::new();
    if pint_json.exists() {
        cmd.arg(&pint_json);
        let orig: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&fs::read(&pint_json)?)?;
        for key in ["exclude", "notPath", "notName", "in"] {
            if let Some(v) = orig.get(key) {
                cfg.insert(key.into(), v.clone());
            }
        }
    }
    let out = cmd.output()?;
    let eff: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("effective-rules.php output")?;
    let options = eff["fixers"]
        .as_array()
        .and_then(|fs| fs.iter().find(|f| f["name"] == name))
        .map(|f| f["options"].clone())
        .filter(|o| !o.is_null())
        .unwrap_or(serde_json::Value::Bool(true));
    cfg.insert("preset".into(), "empty".into());
    cfg.insert("rules".into(), serde_json::json!({ name: options }));
    Ok(serde_json::to_string_pretty(&cfg)?)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let ty = entry.file_type()?;
        if name == "node_modules" {
            symlink_or_copy(&from, &to)?;
        } else if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else if ty.is_symlink() {
            symlink_or_copy(&fs::read_link(&from)?, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn symlink_or_copy(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)?;
    }
    #[cfg(windows)]
    {
        let linked = if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        if linked.is_err() {
            if target.is_dir() {
                copy_tree(target, link)?;
            } else {
                fs::copy(target, link)?;
            }
        }
    }
    Ok(())
}

fn list_files(dir: &Path, rel: &str, out: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let ty = entry.file_type()?;
        if ty.is_dir() {
            list_files(&entry.path(), &rel, out)?;
        } else if ty.is_file() {
            out.insert(rel);
        }
    }
    Ok(())
}

fn compare(name: &str, src: &Path, oracle: &Path, pun: &Path) -> Result<Outcome> {
    let (mut by_oracle, mut by_pun) = (BTreeSet::new(), BTreeSet::new());
    list_files(oracle, "", &mut by_oracle)?;
    list_files(pun, "", &mut by_pun)?;
    by_pun.remove("pun.toml");
    let mut o = Outcome {
        name: name.into(),
        files: by_oracle.union(&by_pun).count(),
        identical: 0,
        differing: vec![],
        oracle_changed: 0,
        pun_changed: 0,
    };
    for rel in by_oracle.union(&by_pun) {
        let (oracle_out, pun_out, source) = (
            fs::read(oracle.join(rel)).ok(),
            fs::read(pun.join(rel)).ok(),
            fs::read(src.join(rel)).ok(),
        );
        o.oracle_changed += usize::from(oracle_out != source);
        o.pun_changed += usize::from(pun_out != source);
        if oracle_out == pun_out {
            o.identical += 1;
        } else {
            o.differing.push(rel.clone());
        }
    }
    Ok(o)
}

/// Rewrites the rows for the projects that ran, keeps the rest.
fn write_scoreboard(path: &Path, outcomes: &[Outcome]) -> Result<()> {
    let old = fs::read_to_string(path).unwrap_or_default();
    let (table, trailer) = old.split_at(old.find("\n## ").unwrap_or(old.len()));
    let mut rows: Vec<String> = table
        .lines()
        .filter(|l| {
            l.starts_with("| ")
                && !l.starts_with("| project")
                && !outcomes
                    .iter()
                    .any(|o| l.starts_with(&format!("| {} |", o.name)))
        })
        .map(String::from)
        .collect();
    for o in outcomes {
        rows.push(format!(
            "| {} | {} | {} | {} | {} | {} |",
            o.name,
            o.files,
            o.identical,
            o.differing.len(),
            o.oracle_changed,
            o.pun_changed
        ));
    }
    rows.sort();
    let md = format!(
        "# Scoreboard\n\nGenerated by `cargo run -p punphp-harness -- format`. Oracle: see context/reference/oracle.md.\n\
         `differing` must reach 0 on every project.\n\n\
         | project | files | identical | differing | pint changed | pun changed |\n\
         |---|---|---|---|---|---|\n{}\n{trailer}",
        rows.join("\n")
    );
    fs::write(path, md)?;
    Ok(())
}
