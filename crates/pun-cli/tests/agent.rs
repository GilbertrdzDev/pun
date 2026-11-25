#![cfg(unix)]
//! Agent mode (context/PLAN.md "Agent-optimised output"): detection priority and the exact lines of every command.
//! Expected rule IDs and summaries come from rules/mapping.toml, the fixers each input triggers from Pint
//! (`php .oracle/pint/builds/pint --format=agent` on the same files), the env list from the agent-detector crate
//! 0.2.1's src/agents.rs plus laravel/agent-detector's `AgentDetector::AGENT_ENV_VARS` (380e6e4). The run time in the summary line is replaced by `T`.
mod common;

use common::{PUN, read};
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

fn run(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, Option<i32>) {
    let out = common::clean_env(&mut Command::new(PUN), &["PATH"])
        .args(args)
        .current_dir(dir)
        .env("COLUMNS", "80")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let out_s = String::from_utf8(out.stdout).unwrap();
    let (first, rest) = out_s.split_once('\n').unwrap_or((&out_s, ""));
    let first = match first.strip_suffix("s)") {
        Some(head) if first.starts_with("pun ") => {
            let i = head.rfind(['(', ' ']).unwrap();
            format!("{}Ts)", &head[..=i])
        }
        _ => first.to_string(),
    };
    let sep = if out_s.contains('\n') { "\n" } else { "" };
    (format!("{first}{sep}{rest}"), out.status.code())
}

fn agent(dir: &Path, args: &[&str]) -> (String, Option<i32>) {
    run(dir, args, &[("PUN_AGENT", "1")])
}

const TOML: &str = "[format]\npreset = \"empty\"\n\n[format.rules]\n\"arrays/syntax\" = true\n\n[lint.rules]\n\"style/no-alias-function\" = \"error\"\n\"dead-code/no-self-assignment\" = { fix = \"off\" }\n";
const ARRAY: &str = "<?php\n$a = array(1);\n";
const ALIAS: &str = "<?php\n$n = sizeof(array());\n";
const SELF: &str = "<?php\n$a = 1;\n$a = $a;\n";
const CLEAN: &str = "<?php\n$c = [1];\n";

const ALIAS_LINE: &str = "app/B.php lint/style/no-alias-function [fix:unsafe] Master functions shall be used instead of aliases.";

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one test walks every case of the behaviour it checks"
)]
fn detection_priority() {
    let d = common::project("agent", "t", &[("pun.toml", TOML), ("app/A.php", ARRAY)]);
    let is_agent = |args: &[&str], env: &[(&str, &str)]| {
        let mut a = vec!["format", "--test"];
        a.extend(args);
        run(&d, &a, env).0.starts_with("pun format: fail")
    };
    for var in [
        "CURSOR_AGENT",
        "GEMINI_CLI",
        "CODEX_SANDBOX",
        "CODEX_CI",
        "CODEX_THREAD_ID",
        "ANTIGRAVITY_AGENT",
        "AUGMENT_AGENT",
        "OPENCODE_CLIENT",
        "CLAUDECODE",
        "CLAUDE_CODE",
        "CLINE_ACTIVE",
        "GOOSE_TERMINAL",
        "OPENCLAW_SHELL",
        "PI_CODING_AGENT",
        "ROO_ACTIVE",
        "TRAE_AI_SHELL_ID",
        "REPL_ID",
        "COPILOT_MODEL",
        "COPILOT_ALLOW_ALL",
        "COPILOT_GITHUB_TOKEN",
        "ASTRBOT_CLI",
        "AUTOHAND_CLI",
        "BASH_AGENT_HOME",
        "CLAWHUB_WORKDIR",
        "AI_AGENT",
        "AGENT",
        "GROK_PLUGIN_ROOT",
        "GROK_PLUGIN_DATA",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "OPENCODE",
        "AMP_CURRENT_THREAD_ID",
        "COPILOT_CLI",
        "ANTIGRAVITY_CLI_ALIAS",
        "MATTERHORN_SESSION_ID",
        "JUNIE_DATA",
        "JUNIE_SHIM_PATH",
        "KIRO_AGENT_PATH",
        "KIMI_PLUGIN_ROOT",
        "ZCODE_ENV",
    ] {
        assert!(is_agent(&[], &[(var, "1")]), "{var}");
        assert!(!is_agent(&[], &[(var, " ")]), "{var}");
    }
    assert!(!is_agent(&[], &[]));
    assert!(!is_agent(&[], &[("CURSOR_TRACE_ID", "1")]));
    assert!(is_agent(
        &[],
        &[("CURSOR_TRACE_ID", "1"), ("CURSOR_AGENT", "1")]
    ));
    assert!(is_agent(
        &[],
        &[("CURSOR_TRACE_ID", "1"), ("CLINE_ACTIVE", "1")]
    ));
    assert!(is_agent(&[], &[("AI_AGENT", "cursor")]));
    assert!(is_agent(
        &[],
        &[("CURSOR_EXTENSION_HOST_ROLE", "agent-exec")]
    ));
    assert!(!is_agent(&[], &[("CURSOR_EXTENSION_HOST_ROLE", "ui")]));
    assert!(!is_agent(&[], &[("AIDER", "1")]));
    assert!(!is_agent(&[], &[("CLAUDECODE", "1"), ("PAO_DISABLE", "1")]));
    assert!(!is_agent(
        &[],
        &[("PAO_FORCE", "true"), ("PAO_DISABLE", "on")]
    ));
    assert!(is_agent(&[], &[("CLAUDECODE", "1"), ("PAO_DISABLE", "0")]));
    assert!(is_agent(&[], &[("PAO_FORCE", "YES")]));
    assert!(!is_agent(&[], &[("PAO_FORCE", "off")]));
    assert!(!is_agent(&[], &[("PUN_AGENT", "0"), ("PAO_FORCE", "1")]));
    assert!(!is_agent(
        &[],
        &[("PUN_AGENT", "false"), ("CLAUDECODE", "1")]
    ));
    assert!(is_agent(&[], &[("PUN_AGENT", "1"), ("PAO_DISABLE", "1")]));
    assert!(is_agent(&[], &[("PUN_AGENT", ""), ("CLAUDECODE", "1")]));
    assert!(is_agent(
        &[],
        &[("PUN_AGENT", "maybe"), ("CLAUDECODE", "1")]
    ));
    assert!(is_agent(&["--agent"], &[("PUN_AGENT", "0")]));
    assert!(!is_agent(&["--no-agent"], &[("PUN_AGENT", "1")]));
    assert!(!is_agent(&["--no-agent"], &[("CLAUDECODE", "1")]));
    assert_eq!(
        run(&d, &["format", "--agent", "--no-agent"], &[]).1,
        Some(2)
    );
    let timeless = |(out, code): (String, Option<i32>)| {
        let head = out.strip_suffix("s\n").unwrap_or(&out);
        let head = head.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        (head.to_string(), code)
    };
    assert_eq!(
        timeless(run(
            &d,
            &["format", "--test", "--no-agent"],
            &[("CLAUDECODE", "1")]
        )),
        timeless(run(&d, &["format", "--test"], &[]))
    );
}

#[test]
fn format() {
    let d = common::project(
        "agent",
        "t",
        &[
            ("pun.toml", TOML),
            ("app/A.php", ARRAY),
            ("app/C.php", CLEAN),
        ],
    );
    assert_eq!(
        agent(&d, &["format", "--test"]),
        (
            "pun format: fail, 1 file needs formatting (2 checked, Ts)\napp/A.php format/arrays/syntax\n".into(),
            Some(1)
        )
    );
    assert_eq!(
        agent(&d, &["format", "--test", "-v"]).0,
        "pun format: fail, 1 file needs formatting (2 checked, Ts)\napp/A.php format/arrays/syntax\n--- app/A.php\n+++ app/A.php\n@@ -1,2 +1,2 @@\n <?php\n-$a = array(1);\n+$a = [1];\n"
    );
    let (json, _) = agent(&d, &["format", "--test", "--format=json"]);
    assert!(json.starts_with(r#"{"about":"PHP CS Fixer"#), "{json}");
    assert_eq!(
        agent(&d, &["format", "--test", "--format=agent"]).0,
        r#"{"tool":"pint","result":"fail","files":[{"path":"app\/A.php","fixers":["array_syntax"]}]}"#
    );
    assert_eq!(
        agent(&d, &["format", "--repair", "-q"]),
        (String::new(), Some(1)),
        "Pint under a coding agent prints nothing with -q, and exits 1 after fixing with --repair"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("app/A.php")).unwrap(),
        CLEAN.replace("$c", "$a")
    );
    assert_eq!(
        agent(&d, &["format"]),
        ("pun format: pass (2 checked, Ts)\n".into(), Some(0))
    );
}

#[test]
fn caps_long_lists() {
    let mut files = vec![("pun.toml".to_string(), TOML.to_string())];
    for n in 1..=23 {
        files.push((format!("app/F{n:02}.php"), ARRAY.into()));
    }
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let d = common::project("agent", "t", &files);
    let line = |n: usize| format!("app/F{n:02}.php format/arrays/syntax\n");
    let head = "pun format: fail, 23 files need formatting (23 checked, Ts)\n";
    let first: String = (1..=20).map(line).collect();
    assert_eq!(
        agent(&d, &["format", "--test"]).0,
        format!("{head}{first}+3 more; run with -v for all\n")
    );
    let all: String = (1..=23)
        .fold(String::new(), |mut out, n| { let _ = write!(out, "{}--- app/F{n:02}.php\n+++ app/F{n:02}.php\n@@ -1,2 +1,2 @@\n <?php\n-$a = array(1);\n+$a = [1];\n",
                line(n)); out });
    assert_eq!(
        agent(&d, &["format", "--test", "-v"]).0,
        format!("{head}{all}")
    );
}

#[test]
fn lint_and_check() {
    let d = common::project(
        "agent",
        "t",
        &[
            ("pun.toml", TOML),
            ("app/A.php", ARRAY),
            ("app/B.php", ALIAS),
            ("app/S.php", SELF),
        ],
    );
    let lint = format!(
        "{ALIAS_LINE}\napp/S.php lint/dead-code/no-self-assignment [fix:safe] Detects and removes self-assignments where a variable or property is assigned to itself.\n"
    );
    assert_eq!(
        agent(&d, &["lint"]),
        (
            format!("pun lint: fail, 2 files with fixes (3 checked, Ts)\n{lint}"),
            Some(1)
        )
    );
    assert_eq!(
        agent(&d, &["check"]),
        (
            format!(
                "pun check: fail, 3 files with fixes (3 checked, Ts)\napp/A.php format/arrays/syntax\napp/B.php format/arrays/syntax\n{lint}"
            ),
            Some(1)
        )
    );
    let remaining = "app/S.php lint/dead-code/no-self-assignment [fix:safe] [remaining] Detects and removes self-assignments where a variable or property is assigned to itself.\n";
    assert_eq!(
        agent(&d, &["lint", "--fix"]),
        (
            format!(
                "pun lint: fail, 1 file fixed, 1 file with remaining fixes (3 checked, Ts)\n{ALIAS_LINE}\n{remaining}"
            ),
            Some(1)
        )
    );
    assert_eq!(
        std::fs::read_to_string(d.join("app/B.php")).unwrap(),
        "<?php\n$n = count(array());\n"
    );
    assert_eq!(
        read(&d, "app/S.php"),
        SELF,
        "remaining fixes are not written"
    );
    assert_eq!(
        agent(&d, &["lint", "--fix"]),
        (
            format!("pun lint: fail, 1 file with remaining fixes (3 checked, Ts)\n{remaining}"),
            Some(1)
        )
    );
    assert_eq!(
        agent(&d, &["check", "--fix"]),
        (
            format!(
                "pun check: fail, 2 files fixed, 1 file with remaining fixes (3 checked, Ts)\napp/A.php format/arrays/syntax\napp/B.php format/arrays/syntax\n{remaining}"
            ),
            Some(1)
        )
    );
    let (out, code) = run(&d, &["lint", "--fix"], &[("LANG", "en_US.UTF-8")]);
    assert_eq!(code, Some(1), "{out}");
    let (lines, summary) = out.trim_end().rsplit_once('\n').unwrap();
    assert_eq!(
        lines,
        " ✖ app/S.php                  lint/dead-code/no-self-assignment"
    );
    assert!(
        summary.starts_with("✖ 1 error · 3 files in ") && summary.ends_with('s'),
        "{out}"
    );
    std::fs::write(d.join("app/S.php"), CLEAN).unwrap();
    assert_eq!(
        agent(&d, &["lint", "--fix"]),
        ("pun lint: pass (3 checked, Ts)\n".to_string(), Some(0))
    );
}

#[test]
fn errors() {
    let d = common::project(
        "agent",
        "t",
        &[
            ("pun.toml", TOML),
            ("app/A.php", ARRAY),
            ("app/D.php", "<?php\n$a = (;\n"),
        ],
    );
    assert_eq!(
        agent(&d, &["format", "--test"]),
        (
            "pun format: fail, 1 error, 1 file needs formatting (2 checked, Ts)\napp/A.php format/arrays/syntax\napp/D.php:2 syntax-error Parse error: syntax error, unexpected token \";\" on line 2.\n".into(),
            Some(1)
        )
    );
    std::fs::write(d.join("app/D.php"), CLEAN).unwrap();
    let locked = d.join("app/A.php");
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_err() {
        let got = agent(&d, &["format"]);
        assert_eq!(
            got,
            (
                format!(
                    "pun format: error: Failed to read content from \"{}\".\n",
                    locked.display()
                ),
                Some(2)
            )
        );
    }
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    assert_eq!(
        agent(&d, &["format", "app/nope.php"]),
        (
            "pun format: error: The path \"app/nope.php\" is not readable.\n".into(),
            Some(2)
        )
    );
    let empty = common::project("agent", "t", &[("app/A.php", ARRAY)]);
    for cmd in ["format", "lint", "config"] {
        assert_eq!(
            agent(&empty, &[cmd]),
            (
                format!(
                    "pun {cmd}: error: No pun.toml found. Run `pun migrate`, or `pun init` for a new project.\n"
                ),
                Some(2)
            )
        );
    }
}

#[test]
fn git_selection() {
    let d = common::project(
        "agent",
        "t",
        &[
            ("pun.toml", TOML),
            ("app/A.php", CLEAN),
            ("app/C.php", CLEAN),
        ],
    );
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&d)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "{args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    git(&["checkout", "-qb", "topic"]);
    std::fs::write(d.join("app/A.php"), ARRAY).unwrap();
    let want = "pun format: fail, 1 file needs formatting (1 checked, Ts)\napp/A.php format/arrays/syntax\n";
    assert_eq!(agent(&d, &["format", "--dirty", "--test"]).0, want);
    git(&["commit", "-qam", "array"]);
    assert_eq!(
        agent(&d, &["format", "--dirty", "--test"]).0,
        "pun format: pass (0 checked, Ts)\n"
    );
    assert_eq!(agent(&d, &["format", "--diff=main", "--test"]).0, want);
}

#[test]
fn config() {
    let d = common::project(
        "agent",
        "t",
        &[
            ("pun.toml", TOML),
            ("composer.json", r#"{"require":{"php":"^8.5"}}"#),
        ],
    );
    let (human, _) = run(&d, &["config"], &[]);
    assert_eq!(agent(&d, &["config"]), (human, Some(0)));
    let out = common::clean_env(&mut Command::new(PUN), &["PATH"])
        .arg("config")
        .current_dir(&d)
        .env("PUN_AGENT", "1")
        .output()
        .unwrap();
    let summary = String::from_utf8(out.stderr).unwrap();
    assert!(
        summary.starts_with(
            "pun config: ok, preset empty, PHP 8.5, 1 format rule, 2 lint rules (from composer.json, pun.toml, "
        ),
        "{summary}"
    );
    assert_eq!(
        agent(&d, &["config", "--format=json"]),
        run(&d, &["config", "--format=json"], &[])
    );
}

#[test]
fn migrate() {
    let d = common::project(
        "agent",
        "t",
        &[
            (
                "pint.json",
                r#"{"preset":"empty","rules":{"array_syntax":true,"no_alias_functions":true}}"#,
            ),
            ("app/A.php", "<?php\n// @php-cs-fixer-ignore array_syntax\n"),
            ("app/U.php", "<?php\n// @php-cs-fixer-ignore Acme/rule\n"),
        ],
    );
    let summary = "pun.toml (preset empty, 1 format rule, 1 lint rule), ignore comments";
    let (out, code) = agent(&d, &["migrate", "--dry-run"]);
    assert_eq!(code, Some(0));
    assert!(
        out.starts_with(&format!(
            "pun migrate: dry-run, {} to rewrite in 1 file (from pint.json, Ts)\npint.json migrate\napp/U.php ignore-not-migrated `@php-cs-fixer-ignore Acme/rule` left as is (no Pun rule for it)\nsend a PR for what is skipped: https://github.com/shipfastlabs/pun\n--- pun.toml\n+++ pun.toml\n",
            summary.replacen("pun.toml", "pun.toml to write", 1)
        )),
        "{out}"
    );
    assert!(!d.join("pun.toml").exists());
    assert_eq!(
        agent(&d, &["migrate"]),
        (
            format!(
                "pun migrate: ok, {} rewritten in 1 file (from pint.json, Ts)\npint.json migrated\napp/U.php ignore-not-migrated `@php-cs-fixer-ignore Acme/rule` left as is (no Pun rule for it)\nsend a PR for what is skipped: https://github.com/shipfastlabs/pun\n",
                summary.replacen("pun.toml", "pun.toml written", 1)
            ),
            Some(0)
        )
    );
    assert_eq!(
        agent(&d, &["migrate"]).0,
        format!(
            "pun migrate: ok, {} rewritten in 0 files (from pint.json, Ts)\npint.json migrated\napp/U.php ignore-not-migrated `@php-cs-fixer-ignore Acme/rule` left as is (no Pun rule for it)\nsend a PR for what is skipped: https://github.com/shipfastlabs/pun\n",
            summary.replacen("pun.toml", "pun.toml up to date", 1)
        )
    );
    std::fs::write(d.join("pint.json"), r#"{"preset":"psr12"}"#).unwrap();
    assert_eq!(
        agent(&d, &["migrate"]),
        (
            "pun migrate: error: pun.toml exists and differs from what migrate writes; move it away first (`pun migrate --dry-run` shows the difference)\n".into(),
            Some(2)
        )
    );
    std::fs::remove_file(d.join("pun.toml")).unwrap();
    std::fs::write(
        d.join("pint.json"),
        r#"{"preset":"empty","rules":{"Acme/x":true}}"#,
    )
    .unwrap();
    assert_eq!(
        agent(&d, &["migrate"]),
        (
            "pun migrate: ok, pun.toml written (preset empty, 0 format rules, 0 lint rules), ignore comments rewritten in 0 files (from pint.json, Ts)\npint.json migrated\npint.json unsupported Acme/x: not a PHP-CS-Fixer or Pint rule Pun knows (a custom fixer or rule set)\napp/U.php ignore-not-migrated `@php-cs-fixer-ignore Acme/rule` left as is (no Pun rule for it)\nsend a PR for what is skipped: https://github.com/shipfastlabs/pun\n".into(),
            Some(0)
        )
    );
    assert_eq!(
        std::fs::read_to_string(d.join("pun.toml")).unwrap(),
        "# Written by `pun migrate` from pint.json.\n[format]\npreset = \"empty\"\n"
    );
}

#[test]
fn migrate_without_a_config() {
    let d = common::project("agent", "t", &[("app/A.php", CLEAN)]);
    let nothing = "no pint.json, .php-cs-fixer.php, phpstan.neon, rector.php or mago.toml found";
    assert_eq!(
        agent(&d, &["migrate"]),
        (
            format!(
                "pun migrate: ok, starter pun.toml written (preset laravel, 115 format rules, 15 lint rules), ignore comments rewritten in 0 files ({nothing}, Ts)\n"
            ),
            Some(0)
        )
    );
    let toml = std::fs::read_to_string(d.join("pun.toml")).unwrap();
    assert!(
        toml.starts_with(&format!(
            "# Written by `pun migrate`: {nothing}; starter config.\n[format]\n"
        )),
        "{toml}"
    );
    assert_eq!(
        run(&d, &["migrate"], &[]),
        (
            format!(
                "N{}.\n ok starter pun.toml is already up to date\n",
                &nothing[1..]
            ),
            Some(0)
        )
    );
    std::fs::remove_file(d.join("pun.toml")).unwrap();
    assert_eq!(
        run(&d, &["migrate"], &[]).0,
        format!(
            "N{}.\n ok Wrote starter pun.toml (preset laravel)\n",
            &nothing[1..]
        )
    );
}

#[test]
fn prettier_preflight() {
    if Command::new("node").arg("-v").output().is_err() {
        return;
    }
    let toml = TOML.replace(
        "\"arrays/syntax\" = true\n",
        "\"arrays/syntax\" = true\n\"laravel/blade\" = true\n",
    );
    let d = common::project("agent", "t", &[("pun.toml", &toml), ("app/A.php", CLEAN)]);
    assert_eq!(
        agent(&d, &["format"]),
        (
            "pun format: error: The rules enabled in your pint configuration require the following prettier dependencies to be installed: prettier, prettier-plugin-blade, prettier-plugin-tailwindcss.\n".into(),
            Some(2)
        )
    );
}

#[test]
fn analyse_lines() {
    let toml = "[analyse]\npaths = [\"src\"]\n";
    let errors =
        "<?php\n\nfunction f(): void\n{\n    undefined_fn();\n    nope(); // @pun-ignore\n}\n";
    let d = common::project(
        "agent",
        "analyse",
        &[("pun.toml", toml), ("src/a.php", errors)],
    );
    assert_eq!(
        agent(&d, &["analyse"]),
        (
            "pun analyse: fail, 3 errors in 1 file (1 checked, Ts)\nsrc/a.php:5 analyse/undefined/function Function undefined_fn not found.\nsrc/a.php:6 analyse/undefined/function Function nope not found.\nsrc/a.php invalid-ignore-comment `@pun-ignore` needs one or more rule IDs\n".into(),
            Some(1)
        )
    );
    assert_eq!(
        agent(&d, &["analyse", "--generate-baseline"]),
        (
            "pun analyse: ok, baseline pun-baseline.toml written with 2 errors (Ts)\n".into(),
            Some(0)
        )
    );
    std::fs::write(
        d.join("pun.toml"),
        format!("{toml}baseline = \"pun-baseline.toml\"\n"),
    )
    .unwrap();
    std::fs::write(d.join("src/a.php"), "<?php\n\nfunction f(): void\n{\n}\n").unwrap();
    assert_eq!(
        agent(&d, &["analyse"]),
        (
            "pun analyse: pass, 2 baseline entries no longer matched (1 checked, Ts)\n".into(),
            Some(0)
        )
    );
}

#[test]
fn analyse_caps_long_lists() {
    let calls: String = (1..=21).fold(String::new(), |mut out, n| {
        let _ = writeln!(out, "    nope{n}();");
        out
    });
    let d = common::project(
        "agent",
        "analyse-cap",
        &[
            ("pun.toml", "[analyse]\npaths = [\"src\"]\n"),
            (
                "src/a.php",
                &format!("<?php\n\nfunction f(): void\n{{\n{calls}}}\n"),
            ),
        ],
    );
    let line = |n: usize| {
        format!(
            "src/a.php:{} analyse/undefined/function Function nope{n} not found.\n",
            n + 4
        )
    };
    let head = "pun analyse: fail, 21 errors in 1 file (1 checked, Ts)\n";
    let first: String = (1..=20).map(line).collect();
    assert_eq!(
        agent(&d, &["analyse"]).0,
        format!("{head}{first}+1 more; run with -v for all\n")
    );
    assert_eq!(
        agent(&d, &["analyse", "-v"]).0,
        format!("{head}{first}{}", line(21))
    );
}
