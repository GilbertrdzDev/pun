#![cfg(unix)]
//! The result cache of `pun format`, `lint` and `check`: a file's result is reused only while nothing it depends on
//! has changed. Each test's cache directory is its own (`common::clean_env`).

mod common;

use common::{PUN, clean_env, project};
use std::path::Path;
use std::process::Command;

fn pun(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, Option<i32>) {
    let out = clean_env(&mut Command::new(PUN), &["PATH", "HOME"])
        .current_dir(dir)
        .env("PUN_AGENT", "0")
        .env("NO_COLOR", "1")
        .envs(env.iter().copied())
        .args(args)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    (text.into_owned(), out.status.code())
}

fn code(dir: &Path, args: &[&str]) -> Option<i32> {
    pun(dir, args, &[]).1
}

const EMPTY: &str = "[format]\npreset = \"empty\"\n";
const LARAVEL: &str = "[format]\npreset = \"laravel\"\n";

/// A file with no rule to break is clean under the empty preset; under laravel `array( 1 )` needs `[1]`. The cached
/// result of the first config must not answer for the second, nor for new content.
#[test]
fn a_clean_file_is_checked_again_when_the_config_or_its_content_changes() {
    let d = project(
        "cache",
        "key",
        &[("pun.toml", EMPTY), ("a.php", "<?php\n$a = array( 1 );\n")],
    );
    assert_eq!(code(&d, &["format", "--test"]), Some(0));
    assert_eq!(code(&d, &["format", "--test"]), Some(0));
    std::fs::write(d.join("pun.toml"), LARAVEL).unwrap();
    assert_eq!(code(&d, &["format", "--test"]), Some(1));

    std::fs::write(d.join("a.php"), "<?php\n\n$a = [1];\n").unwrap();
    assert_eq!(code(&d, &["format", "--test"]), Some(0));
    assert_eq!(code(&d, &["format", "--test"]), Some(0));
    std::fs::write(d.join("a.php"), "<?php\n\n$a = array(1);\n").unwrap();
    assert_eq!(code(&d, &["format", "--test"]), Some(1));
    // The same content under `lint` (no lint rule fires) is another command's result: `check` still formats.
    std::fs::write(d.join("a.php"), "<?php\n\n$a = [1];\n").unwrap();
    assert_eq!(code(&d, &["format", "--test"]), Some(0));
    std::fs::write(d.join("a.php"), "<?php\n\n$a = array(1);\n").unwrap();
    assert_eq!(code(&d, &["lint"]), Some(0));
    assert_eq!(code(&d, &["check"]), Some(1));
}

/// A cached warning is still a warning: the second run prints it again without failing, and `--deny-warnings` still
/// fails on it.
#[test]
fn a_cached_warning_stays_a_warning() {
    let d = project(
        "cache",
        "warning",
        &[
            (
                "pun.toml",
                "[lint.rules]\n\"security/no-global\" = \"warn\"\n",
            ),
            ("a.php", "<?php\n\nfunction f()\n{\n    global $y;\n}\n"),
        ],
    );
    for _ in 0..2 {
        let (out, code) = pun(&d, &["lint"], &[]);
        assert!(out.contains("global"), "{out}");
        assert_eq!(code, Some(0));
    }
    assert_eq!(code(&d, &["lint", "--deny-warnings"]), Some(1));
}

/// The output without its timings (the last word of a line, such as `0.06s`).
fn untimed(out: &str) -> String {
    let line = |l: &'_ str| match l.rsplit_once(' ') {
        Some((head, t))
            if t.strip_suffix('s')
                .is_some_and(|n| n.parse::<f64>().is_ok()) =>
        {
            head.to_string()
        }
        _ => l.to_string(),
    };
    out.lines().map(line).collect::<Vec<_>>().join("\n")
}

/// A file with findings comes back from the cache as its check reported it: the problems with their level, help,
/// source and fix, and the formatting diff.
#[test]
fn a_cached_result_reports_what_the_check_found() {
    let d = project(
        "cache",
        "found",
        &[
            (
                "pun.toml",
                "[format]\npreset = \"laravel\"\n\n[lint.rules]\n\"style/no-escaped-quotes\" = \"error\"\n\"style/require-todo-reference\" = \"warn\"\n",
            ),
            (
                "a.php",
                "<?php\n\n// TODO tidy\n$a = 'it\\'s';\n$b = array( 1 );\n",
            ),
            (
                "b.php",
                "<?php\n\n// TODO tidy\n$a = 'it\\'s';\n$b = array( 1 );\n",
            ),
        ],
    );
    // `--fix --test` reports the problems left in the fixed code, which the cache keeps besides the file's own.
    for (args, shown) in [
        (&["lint"][..], "help: Add a user tag"),
        (&["lint"][..], "2 fixable"),
        (&["lint", "--fix", "--test"][..], "help: Add a user tag"),
        (&["check", "--test", "-v"][..], "+$b = [1];"),
        (&["format", "--test"][..], "need formatting"),
    ] {
        let first = pun(&d, args, &[]);
        assert!(first.0.contains(shown), "{}", first.0);
        assert_eq!(first.1, Some(1));
        let second = pun(&d, args, &[]);
        assert_eq!(untimed(&second.0), untimed(&first.0));
        assert_eq!(second.1, first.1);
    }
}

/// `style/require-parent-visibility` wants a method's visibility to be its parent's, so Child.php's result depends
/// on Base.php: Child stays the same, and is reported once Base's method turns protected.
#[test]
fn a_result_that_reads_another_file_follows_that_file() {
    let base = |visibility: &str| {
        format!(
            "<?php\n\nnamespace App;\n\nclass Base\n{{\n    {visibility} function run()\n    {{\n    }}\n}}\n"
        )
    };
    let d = project(
        "cache",
        "project",
        &[
            (
                "pun.toml",
                "[lint.rules]\n\"style/require-parent-visibility\" = \"error\"\n",
            ),
            (
                "composer.json",
                "{\"autoload\": {\"psr-4\": {\"App\\\\\": \"src/\"}, \"files\": [\"helpers.php\"]}}\n",
            ),
            ("helpers.php", "<?php\n"),
            ("src/Base.php", &base("public")),
            (
                "src/Child.php",
                "<?php\n\nnamespace App;\n\nclass Child extends Base\n{\n    public function run()\n    {\n    }\n}\n",
            ),
        ],
    );
    assert_eq!(code(&d, &["lint"]), Some(0));
    assert_eq!(code(&d, &["lint"]), Some(0));
    std::fs::write(d.join("src/Base.php"), base("protected")).unwrap();
    let (out, code) = pun(&d, &["lint"], &[]);
    assert!(out.contains("src/Child.php"), "{out}");
    assert_eq!(code, Some(1));
}

/// The same, with Base in a Composer `path` package: `vendor/acme/base` links to `packages/base`, outside the linted
/// paths, and editing it changes no Composer file.
#[test]
fn a_result_follows_a_linked_package() {
    let base = |visibility: &str| {
        format!(
            "<?php\n\nnamespace Pkg;\n\nclass Base\n{{\n    {visibility} function run()\n    {{\n    }}\n}}\n"
        )
    };
    let d = project(
        "cache",
        "linked",
        &[
            (
                "pun.toml",
                "[files]\npaths = [\"src\"]\n\n[lint.rules]\n\"style/require-parent-visibility\" = \"error\"\n",
            ),
            ("composer.json", "{}\n"),
            (
                "vendor/composer/autoload_classmap.php",
                "<?php\n\nreturn array(\n    'Pkg\\\\Base' => $vendorDir . '/acme/base/src/Base.php',\n);\n",
            ),
            ("packages/base/src/Base.php", &base("public")),
            (
                "src/Child.php",
                "<?php\n\nnamespace App;\n\nclass Child extends \\Pkg\\Base\n{\n    public function run()\n    {\n    }\n}\n",
            ),
        ],
    );
    std::fs::create_dir_all(d.join("vendor/acme")).unwrap();
    std::os::unix::fs::symlink("../../packages/base", d.join("vendor/acme/base")).unwrap();
    assert_eq!(code(&d, &["lint"]), Some(0));
    assert_eq!(code(&d, &["lint"]), Some(0));
    std::fs::write(d.join("packages/base/src/Base.php"), base("protected")).unwrap();
    let (out, code) = pun(&d, &["lint"], &[]);
    assert!(out.contains("src/Child.php"), "{out}");
    assert_eq!(code, Some(1));
}

/// `--no-cache` and `PUN_NO_CACHE=1` neither read nor write the cache; a plain run writes it.
#[test]
fn no_cache_leaves_the_cache_alone() {
    let d = project(
        "cache",
        "off",
        &[("pun.toml", LARAVEL), ("a.php", "<?php\n\n$a = [1];\n")],
    );
    let results = common::test_cache_dir().join("results");
    let _ = std::fs::remove_dir_all(&results);
    assert_eq!(code(&d, &["format", "--test", "--no-cache"]), Some(0));
    assert_eq!(code(&d, &["lint", "--no-cache"]), Some(0));
    assert_eq!(pun(&d, &["check"], &[("PUN_NO_CACHE", "1")]).1, Some(0));
    assert!(!results.exists());
    assert_eq!(code(&d, &["format", "--test"]), Some(0));
    assert_eq!(std::fs::read_dir(&results).unwrap().count(), 1);
}
