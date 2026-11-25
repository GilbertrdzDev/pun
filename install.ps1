# Installs the latest Pun Windows build into %LOCALAPPDATA%\Programs\pun and adds that
# directory to the user PATH.
#
#   irm https://raw.githubusercontent.com/GilbertrdzDev/pun/main/install.ps1 | iex
#
# PUN_VERSION   optional tag, such as v0.4.3 (default: the latest GitHub release)
# PUN_REPO      override the GitHub repository (default: GilbertrdzDev/pun)

$ErrorActionPreference = 'Stop'

$Repo = if ($env:PUN_REPO) { $env:PUN_REPO } else { 'GilbertrdzDev/pun' }
$Dest = Join-Path $env:LOCALAPPDATA 'Programs\pun'
$Asset = 'pun-windows-x86_64.zip'
$Headers = @{
    'User-Agent' = 'pun-install'
    'Accept'     = 'application/vnd.github+json'
}

function Get-PunRelease {
    if ($env:PUN_VERSION) {
        $tag = $env:PUN_VERSION
        if ($tag -notmatch '^v') { $tag = "v$tag" }
        $uri = "https://api.github.com/repos/$Repo/releases/tags/$tag"
    } else {
        $uri = "https://api.github.com/repos/$Repo/releases/latest"
    }
    Invoke-RestMethod -Uri $uri -Headers $Headers
}

function Add-UserPath([string]$Directory) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $parts = @($userPath -split ';' | Where-Object { $_ -ne '' })
    $already = $false
    foreach ($part in $parts) {
        if ($part.TrimEnd('\') -ieq $Directory.TrimEnd('\')) { $already = $true }
    }
    if (-not $already) {
        $updated = if ([string]::IsNullOrWhiteSpace($userPath)) { $Directory } else { "$userPath;$Directory" }
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    }
    if (-not (($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $Directory.TrimEnd('\') })) {
        $env:Path = if ([string]::IsNullOrWhiteSpace($env:Path)) { $Directory } else { "$env:Path;$Directory" }
    }
}

$release = Get-PunRelease
$zipAsset = $release.assets | Where-Object { $_.name -eq $Asset } | Select-Object -First 1
$shaAsset = $release.assets | Where-Object { $_.name -eq "$Asset.sha256" } | Select-Object -First 1
if (-not $zipAsset) {
    throw "release $($release.tag_name) has no $Asset"
}
if (-not $shaAsset) {
    throw "release $($release.tag_name) has no $Asset.sha256"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("pun-install-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zipPath = Join-Path $tmp $Asset
    $shaPath = Join-Path $tmp "$Asset.sha256"
    Write-Host "pun-install: downloading $($zipAsset.browser_download_url)"
    Invoke-WebRequest -Uri $zipAsset.browser_download_url -OutFile $zipPath -Headers $Headers -UseBasicParsing
    Invoke-WebRequest -Uri $shaAsset.browser_download_url -OutFile $shaPath -Headers $Headers -UseBasicParsing
    $expected = ((Get-Content -Raw $shaPath) -split '\s+')[0].Trim().ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $zipPath).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "SHA-256 mismatch for $Asset (expected $expected, got $actual)"
    }
    Write-Host "pun-install: verified SHA-256"
    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
    $exe = Get-ChildItem -Path $tmp -Filter pun.exe -Recurse -File | Select-Object -First 1
    if (-not $exe) { throw "$Asset has no pun.exe" }
    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    $target = Join-Path $Dest 'pun.exe'
    Copy-Item -LiteralPath $exe.FullName -Destination $target -Force
    Unblock-File -LiteralPath $target
    Add-UserPath $Dest
    Write-Host "pun-install: installed $target ($($release.tag_name))"
    Write-Host "pun-install: open a new terminal, then run: pun --version"
    if (-not (Get-Command php -ErrorAction SilentlyContinue)) {
        Write-Warning "php is not on PATH. Pun needs PHP 8.4 or later for install, update, require, and Composer scripts."
    }
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
