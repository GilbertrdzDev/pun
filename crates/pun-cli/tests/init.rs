#![cfg(unix)]
mod common;

use common::pty::{pun, pun_with};
use common::read;
use pun_config::{AnalysePreset, LintPreset, Options};
use std::path::{Path, PathBuf};

const LARAVEL_JSON: &str = r#"{"require":{"php":"^8.4","laravel/framework":"^13.0"},"require-dev":{"pestphp/pest":"^4.0"}}"#;
const LARAVEL_LOCK: &str = r#"{"packages":[{"name":"laravel/framework","version":"v13.2.0"}],"packages-dev":[{"name":"pestphp/pest","version":"v4.1.2"},{"name":"phpunit/phpunit","version":"12.3.4"}]}"#;
const PLAIN_JSON: &str = r#"{"require":{"php":">=8.4"}}"#;
const ENV: &[(&str, &str)] = &[("LANG", "C.UTF-8"), ("NO_COLOR", "1")];

fn laravel(name: &str, extra: &[(&str, &str)]) -> PathBuf {
    let mut files = vec![
        ("composer.json", LARAVEL_JSON),
        ("composer.lock", LARAVEL_LOCK),
    ];
    files.extend_from_slice(extra);
    common::project("init", name, &files)
}

fn plain(name: &str) -> PathBuf {
    common::project("init", name, &[("composer.json", PLAIN_JSON)])
}

fn toml(style: &str, lint: &str, analyse: &str) -> String {
    toml_with(&[], style, lint, analyse)
}

/// What init writes, with `[extensions]` turning on the extensions composer.lock shows.
fn toml_with(extensions: &[&str], style: &str, lint: &str, analyse: &str) -> String {
    let on = extensions.join(" = \"on\"\n");
    let on = if on.is_empty() {
        on
    } else {
        format!("[extensions]\n{on} = \"on\"\n\n")
    };
    format!(
        "# Written by `pun init`.\n{on}[files]\nexclude = [\"tmp\", \"temp\"]\n\n[format]\npreset = \"{style}\"\n\n[lint]\npreset = \"{lint}\"\n\n[analyse]\npreset = \"{analyse}\"\n"
    )
}

const ALL: &[&str] = &["laravel", "pest", "phpunit"];

fn loaded(d: &Path) -> (String, Option<LintPreset>, AnalysePreset) {
    let c = pun_config::load(d, &Options::default()).unwrap();
    (c.preset.to_string(), c.lint.preset, c.analyse.preset)
}

fn tty(d: &Path, keys: &str) -> (String, Option<i32>) {
    let (out, code) = common::pty::pty(d, "init", keys, ENV);
    (common::strip_ansi(&out), code)
}

const NEXT: &str = "\n  ✔ Wrote pun.toml\n\n  Done. Run `pun check` any time; `pun check --fix` before you commit.\n  Add `pun check` to CI and your composer scripts.\n";

#[test]
fn laravel_defaults_without_a_terminal() {
    let d = laravel("laravel", &[]);
    let (out, err, code) = pun_with(&d, &["init"], ENV);
    assert_eq!((err.as_str(), code), ("", Some(0)));
    assert_eq!(
        out,
        format!(
            "\n  Pun · new project setup\n  Found Laravel 13, Pest 4, PHP ^8.4 (composer.json, composer.lock)\n\nCode style: laravel\nLint preset: recommended\nAnalysis preset: recommended\n\n  Extensions on from composer.lock: Laravel, Pest and PHPUnit. See [extensions] in pun.toml.\n{NEXT}"
        )
    );
    assert_eq!(
        read(&d, "pun.toml"),
        toml_with(ALL, "laravel", "recommended", "recommended")
    );
    assert_eq!(
        loaded(&d),
        (
            "laravel".into(),
            Some(LintPreset::Recommended),
            AnalysePreset::Recommended
        )
    );

    let (out, err, code) = pun_with(&d, &["init", "--style", "psr12"], ENV);
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "✖ pun.toml exists; `pun init` doesn't overwrite it. Edit it, or move it away and run `pun init` again.\n",
            Some(2)
        )
    );
    assert!(read(&d, "pun.toml").contains("preset = \"laravel\""));
}

/// A pun.toml symlink to a file that doesn't exist is a pun.toml too: init refuses it rather than create the file it
/// leads to, outside the project.
#[test]
fn a_dangling_pun_toml_link_is_not_followed() {
    let d = plain("dangling");
    let outside = common::scratch("init", "outside").join("made.toml");
    std::os::unix::fs::symlink(&outside, d.join("pun.toml")).unwrap();
    let (out, err, code) = pun_with(&d, &["init"], ENV);
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "✖ pun.toml exists; `pun init` doesn't overwrite it. Edit it, or move it away and run `pun init` again.\n",
            Some(2)
        )
    );
    assert!(!outside.exists());
}

#[test]
fn plain_project_gets_psr12() {
    let d = plain("plain");
    let (out, _, code) = pun(&d, &["init", "--yes"]);
    assert_eq!(code, Some(0));
    assert_eq!(
        out,
        "\n  Pun · new project setup\n  Found PHP >=8.4 (composer.json)\n\nCode style: psr12\nLint preset: recommended\nAnalysis preset: recommended\n\n  ok Wrote pun.toml\n\n  Done. Run `pun check` any time; `pun check --fix` before you commit.\n  Add `pun check` to CI and your composer scripts.\n"
    );
    assert_eq!(
        read(&d, "pun.toml"),
        toml("psr12", "recommended", "recommended")
    );

    let d = common::project("init", "bare", &[]);
    let (out, _, _) = pun(&d, &["init"]);
    assert!(
        out.starts_with("\n  Pun · new project setup\n  No Laravel, Pest, PHPUnit or PHP version found in composer.json or composer.lock\n\nCode style: psr12\n"),
        "{out}"
    );
}

#[test]
fn flags_pick_the_choices() {
    let lock = r#"{"packages-dev":[{"name":"phpunit/phpunit","version":"dev-main"}]}"#;
    let d = laravel("flags", &[("composer.lock", lock)]);
    let args = [
        "init",
        "--style",
        "laravel",
        "--lint",
        "strict",
        "--analyse",
        "max",
    ];
    let (out, _, code) = pun_with(&d, &args, ENV);
    assert_eq!(code, Some(0));
    assert!(
        out.starts_with("\n  Pun · new project setup\n  Found PHPUnit dev-main, PHP ^8.4 (composer.json, composer.lock)\n\nCode style: laravel\nLint preset: strict\nAnalysis preset: max\n\n  Extensions on from composer.lock: PHPUnit. See [extensions] in pun.toml.\n"),
        "{out}"
    );
    assert_eq!(
        read(&d, "pun.toml"),
        toml_with(&["phpunit"], "laravel", "strict", "max")
    );
    assert_eq!(
        loaded(&d),
        (
            "laravel".into(),
            Some(LintPreset::Strict),
            AnalysePreset::Max
        )
    );

    let d = plain("essential");
    let (out, _, _) = pun(&d, &["init", "--lint", "essential", "--analyse", "basic"]);
    assert!(
        out.contains("Lint preset: essential\nAnalysis preset: basic\n"),
        "{out}"
    );
    assert_eq!(read(&d, "pun.toml"), toml("psr12", "essential", "basic"));
    assert_eq!(
        loaded(&d),
        (
            "psr12".into(),
            Some(LintPreset::Essential),
            AnalysePreset::Basic
        )
    );
}

#[test]
fn bad_flags_are_rejected() {
    let d = plain("bad");
    let (out, err, code) = pun_with(&d, &["init", "--lint", "speed"], ENV);
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "✖ --lint: `speed` is not a lint preset; use `essential`, `recommended`, `strict`\n",
            Some(2)
        )
    );
    let (_, err, code) = pun_with(&d, &["init", "--analyse", "level5"], ENV);
    assert_eq!(
        (err.as_str(), code),
        (
            "✖ --analyse level5: expected `basic`, `essential`, `recommended`, `strict`, `max`, or `phpstan/level_0` to `phpstan/level_10` or `phpstan/level_max`\n",
            Some(2)
        )
    );
    let (_, err, code) = pun(&d, &["init", "--style", "empty"]);
    assert_eq!(code, Some(2));
    assert!(
        err.contains("invalid value 'empty' for '--style <STYLE>'"),
        "{err}"
    );
    assert!(!d.join("pun.toml").exists());
}

#[test]
fn old_config_without_a_terminal_points_to_migrate() {
    let d = laravel("old", &[("pint.json", "{}"), ("phpstan.neon", "")]);
    let (out, err, code) = pun_with(&d, &["init", "--yes"], ENV);
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "✖ Found pint.json and phpstan.neon: this project fits `pun migrate`, which turns them into pun.toml. Nothing written.\n",
            Some(2)
        )
    );
    let (out, _, code) = pun_with(&d, &["init"], &[("PUN_AGENT", "1")]);
    assert_eq!(
        (out.as_str(), code),
        (
            "pun init: error: Found pint.json and phpstan.neon: this project fits `pun migrate`, which turns them into pun.toml. Nothing written.\n",
            Some(2)
        )
    );
    assert!(!d.join("pun.toml").exists());
}

#[test]
fn agent_mode() {
    let d = laravel("agent", &[]);
    let (out, _, code) = pun_with(&d, &["init"], &[("PUN_AGENT", "1")]);
    assert_eq!(code, Some(0));
    let (summary, item) = out.split_once('\n').unwrap();
    assert!(
        summary.starts_with(
            "pun init: ok, pun.toml written, style laravel, lint recommended, analyse recommended"
        ),
        "{out}"
    );
    assert_eq!(
        item,
        "next: run `pun check`, `pun check --fix` before committing; add `pun check` to CI and composer scripts\n"
    );
    assert_eq!(
        read(&d, "pun.toml"),
        toml_with(ALL, "laravel", "recommended", "recommended")
    );
}

#[test]
fn terminal_accepts_the_defaults() {
    let d = laravel("tty", &[]);
    let (out, code) = tty(&d, "\\r\\r\\r");
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("  Pun · new project setup\n  Found Laravel 13, Pest 4, PHP ^8.4 (composer.json, composer.lock)\n"),
        "{out}"
    );
    for answer in [
        "? Code style ›",
        "✔ Code style · laravel",
        "✔ Lint preset · recommended",
        "✔ Analysis preset · recommended",
    ] {
        assert!(out.contains(answer), "{answer}: {out}");
    }
    assert!(
        out.ends_with(&format!(
            "\n  Extensions on from composer.lock: Laravel, Pest and PHPUnit. See [extensions] in pun.toml.\n{NEXT}"
        )),
        "{out}"
    );
    assert_eq!(
        read(&d, "pun.toml"),
        toml_with(ALL, "laravel", "recommended", "recommended")
    );

    let d = plain("tty-pick");
    let (out, code) = tty(&d, "\\033[B\\r\\033[B\\r\\033[B\\r");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("✔ Code style · laravel"), "{out}");
    assert_eq!(read(&d, "pun.toml"), toml("laravel", "strict", "strict"));

    let d = plain("tty-ci");
    let (out, code) = common::pty::pty(&d, "init", "", &[("CI", "true"), ENV[0], ENV[1]]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("\nCode style: psr12\nLint preset: recommended\n"),
        "{out}"
    );
}

#[test]
fn terminal_offers_migrate() {
    let d = laravel("tty-migrate", &[("pint.json", r#"{"preset":"psr12"}"#)]);
    let (out, code) = tty(&d, "\\r\\r");
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("✔ Found pint.json. Migrate it instead? · yes"),
        "{out}"
    );
    assert!(
        out.ends_with(" ✔ pint.json  → [format] preset psr12, 0 rules changed, 2 rules → [lint]\n ✔ Wrote pun.toml\n"),
        "{out}"
    );
    assert!(read(&d, "pun.toml").starts_with("# Written by `pun migrate` from pint.json.\n"));

    let d = laravel("tty-no-migrate", &[("pint.json", "{}")]);
    let (out, code) = tty(&d, "n\\r\\r\\r");
    assert_eq!(code, Some(0), "{out}");
    assert!(out.ends_with(NEXT), "{out}");
    assert_eq!(
        read(&d, "pun.toml"),
        toml_with(ALL, "laravel", "recommended", "recommended")
    );
}
