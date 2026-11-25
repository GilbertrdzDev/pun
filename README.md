<p align="center"><img src="docs/public/og.png" width="600" alt="Pun: ultrafast unified PHP tooling"></p>

<p align="center">
<a href="https://packagist.org/packages/shipfastlabs/pun"><img src="https://img.shields.io/packagist/dt/shipfastlabs/pun" alt="Total Downloads"></a>
<a href="https://packagist.org/packages/shipfastlabs/pun"><img src="https://img.shields.io/packagist/v/shipfastlabs/pun" alt="Latest Stable Version"></a>
<a href="https://packagist.org/packages/shipfastlabs/pun"><img src="https://img.shields.io/packagist/l/shipfastlabs/pun" alt="License"></a>
</p>

## Introduction

Pun is one native binary for projects that target PHP 8.3 or later. It formats, lints, and analyses your code, and installs your Composer packages from `composer.json` and `composer.lock`. Its own settings live in one `pun.toml`. It formats code byte for byte like [Laravel Pint](https://github.com/laravel/pint), and `pun migrate` converts your existing Pint, PHP-CS-Fixer, Rector, and PHPStan configs.

Install the binary on Linux or macOS (x86_64 or arm64):

```sh
curl -fsSL https://punphp.com/install | sh
```

### Windows

Windows x86_64 builds are published with this fork. PHP 8.4 or later must be on `PATH` for `pun install`, `pun update`, `pun require`, and Composer scripts. `pun format`, `pun lint`, and `pun analyse` do not need PHP.

The per-user installer (`pun-setup-x86_64.exe`, from the GitHub release) installs to `%LOCALAPPDATA%\Programs\pun`, adds that directory to your user `PATH`, and removes it on uninstall. It does not need an administrator. If `php` is not on `PATH`, the installer warns and continues.

PowerShell, from a normal (non-admin) prompt:

```powershell
irm https://raw.githubusercontent.com/GilbertrdzDev/pun/main/install.ps1 | iex
```

That downloads `pun-windows-x86_64.zip` from the latest release, checks its SHA-256, and installs to the same directory. Or download the zip, put `pun.exe` on your `PATH`, and confirm the hash with `Get-FileHash .\pun.exe -Algorithm SHA256`.

`pun self-update` downloads the Windows zip, checks SHA-256, and replaces the running `pun.exe` (Windows renames the running file aside first). A copy installed by Composer is updated with `composer update`, not `self-update`.

Known limitations on Windows:

- Composer scripts run with `cmd /C`, so they need to be `cmd` commands (or `@php` / `@composer`). A script written for `sh` will not run.
- Creating a symlink needs Developer Mode or an elevated process. Otherwise Pun copies the target (path repositories, archive symlinks).
- There are no Unix permission bits. The package cache does not share files with hardlinks across projects the way it does on Linux; hardlinks are used when Windows allows them.
- `pun x` waits for the child process instead of replacing the current one.
- The terminal width comes from the console. Set `$env:COLUMNS` when it does not.
- The binary is not code-signed. If SmartScreen blocks it, `Unblock-File` the `pun.exe` you downloaded.

Or add it to a project with Composer:

```sh
composer require --dev shipfastlabs/pun

vendor/bin/pun migrate   # or: vendor/bin/pun init
vendor/bin/pun check --fix
```

## Official Documentation

The documentation is on the [Pun website](https://punphp.com/docs).

## Contributing

Thank you for considering contributing to Pun! [AGENTS.md](AGENTS.md) explains how to build Pun (Rust 1.100 or later), which checks to run, and which upstream tools Pun is tested against. If Pun is missing a rule you rely on, open an issue or send a pull request.

## Security Vulnerabilities

To report a security vulnerability, follow [our security policy](https://github.com/shipfastlabs/pun/security/policy).

## Credits

Pun would not be possible without [Mago](https://github.com/carthage-software/mago), [PHP-CS-Fixer](https://github.com/PHP-CS-Fixer/PHP-CS-Fixer), [Laravel Pint](https://github.com/laravel/pint), [PHPStan](https://github.com/phpstan/phpstan), [Larastan](https://github.com/larastan/larastan), [Rector](https://github.com/rectorphp/rector) and [Composer](https://github.com/composer/composer). Pun uses Mago's parser and analysis libraries directly and ports behavior, rules and test fixtures from these projects. Thank you to everyone behind them.

## License

Pun is open-sourced software licensed under the [MIT license](LICENSE). The licenses of ported code are in [licenses/THIRD_PARTY_NOTICES.md](licenses/THIRD_PARTY_NOTICES.md), and those of the bundled Rust crates in [licenses/THIRD_PARTY_LICENSES.md](licenses/THIRD_PARTY_LICENSES.md).
