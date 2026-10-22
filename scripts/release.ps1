# Builds a signed release on this PC and publishes it to GitHub Releases.
#
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Version 0.8.0
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Version 0.8.0 -DryRun
#
# The update signing key stays on this PC (%USERPROFILE%\.tauri\fine-file.key);
# GitHub only ever gets the installer, its .sig and latest.json, which the
# installed apps read to offer the update.
#
# -DryRun does everything but publish: it builds and signs, writes latest.json
# and prints it, but makes no tag, pushes nothing and creates no release. It
# also runs with uncommitted changes (a real release needs a clean tree).

param(
    [Parameter(Mandatory = $true)][string]$Version,
    # Release notes; by default the "## [<Version>]" section of CHANGELOG.md.
    [string]$NotesFile,
    [string]$Repo = "toxh13/fine-file",
    [string]$KeyPath = "$env:USERPROFILE\.tauri\fine-file.key",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Fail($msg) { Write-Host "중단: $msg" -ForegroundColor Red; exit 1 }
function Warn($msg) { Write-Host "주의: $msg" -ForegroundColor Yellow }

if ($DryRun) { Write-Host "연습 실행(-DryRun): 빌드하고 서명하지만 태그·푸시·릴리스는 하지 않아요" -ForegroundColor Cyan }

# ---- checks -------------------------------------------------------------------
if ($Version -notmatch '^\d+\.\d+\.\d+$') { Fail "버전은 0.8.0 같은 형식이어야 해요" }
$pkg = (Get-Content package.json -Raw | ConvertFrom-Json).version
$conf = (Get-Content src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
$cargo = (Select-String -Path src-tauri\Cargo.toml -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
# package-lock.json has the version twice, at the top and in packages[""]; Windows PowerShell's
# ConvertFrom-Json cannot read a key that is empty, so node reads it.
$lock = (node -p "const l = require('./package-lock.json'); l.version + ' ' + (l.packages && l.packages[''] || {}).version") -split ' '
if ($pkg -ne $Version -or $conf -ne $Version -or $cargo -ne $Version -or $lock.Count -ne 2 -or $lock[0] -ne $Version -or $lock[1] -ne $Version) {
    Fail "package.json($pkg), package-lock.json($($lock -join ', ')), tauri.conf.json($conf), Cargo.toml($cargo)의 버전이 $Version 와 같아야 해요"
}
if (-not (Test-Path $KeyPath)) { Fail "서명 키가 없어요: $KeyPath" }
# A workflow run by a pushed tag would start on GitHub with the tag below (the old tauri-action
# release.yml did, and fails without the signing key): it must not be in what is committed.
$onTags = @(git ls-files .github/workflows | Where-Object { (git show ":$_" | Out-String) -match '(?m)^\s*tags\s*:' })
if ($onTags.Count -gt 0) {
    $msg = "태그를 올리면 GitHub에서 도는 워크플로가 저장소에 있어요: $($onTags -join ', '). git rm --cached 로 빼고 커밋하세요"
    if ($DryRun) { Warn $msg } else { Fail $msg }
}
if (git status --porcelain) {
    if ($DryRun) { Warn "커밋하지 않은 변경이 있어요. 연습 실행이라 그대로 빌드해요." }
    else { Fail "커밋하지 않은 변경이 있어요. 먼저 커밋하세요." }
}
$tag = "v$Version"
if (git tag --list $tag) {
    if ($DryRun) { Warn "태그 $tag 가 이미 있어요. 실제 릴리스는 이 버전으로 할 수 없어요." }
    else { Fail "태그 $tag 가 이미 있어요" }
}
if (-not $DryRun) {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { Fail "gh(GitHub CLI)가 없어요 (winget install GitHub.cli)" }
    # Logged out, gh says so on stderr, which Windows PowerShell turns into a terminating error
    # under "Stop" once it is redirected: only the exit code is looked at here.
    $ErrorActionPreference = "Continue"
    gh auth status *> $null
    $loggedIn = $LASTEXITCODE -eq 0
    $ErrorActionPreference = "Stop"
    if (-not $loggedIn) { Fail "gh 로그인이 필요해요 (gh auth login)" }
}

# ---- notes --------------------------------------------------------------------
$notes = ""
if ($NotesFile) {
    $notes = Get-Content $NotesFile -Raw -Encoding UTF8
} elseif (Test-Path CHANGELOG.md) {
    $text = Get-Content CHANGELOG.md -Raw -Encoding UTF8
    $m = [regex]::Match($text, "(?ms)^## \[$([regex]::Escape($Version))\][^\n]*\n(.*?)(?=^## \[|\z)")
    if ($m.Success) { $notes = $m.Groups[1].Value.Trim() }
}
if (-not $notes) {
    Warn "CHANGELOG.md에 ## [$Version] 부분이 없어서 릴리스 노트를 버전 이름으로 대신해요"
    $notes = "fine-file $Version"
}

# ---- build (signed) -----------------------------------------------------------
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content $KeyPath -Raw
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
try {
    npm run tauri build
    $built = $LASTEXITCODE -eq 0
} finally {
    Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
}
if (-not $built) { Fail "빌드에 실패했어요" }

$bundle = "src-tauri\target\release\bundle\nsis"
$asset = "fine-file_${Version}_x64-setup.exe"
$setup = Join-Path $bundle $asset
$sig = "$setup.sig"
if (-not (Test-Path $setup)) { Fail "설치파일이 없어요: $setup" }
if (-not (Test-Path $sig)) { Fail "서명 파일이 없어요: $sig (createUpdaterArtifacts 설정을 확인하세요)" }

# ---- latest.json (what installed apps read) -----------------------------------
$latest = [ordered]@{
    version   = $Version
    notes     = $notes
    pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = [ordered]@{
        "windows-x86_64" = [ordered]@{
            signature = (Get-Content $sig -Raw).Trim()
            url       = "https://github.com/$Repo/releases/download/$tag/$asset"
        }
    }
}
$latestPath = Join-Path (Resolve-Path $bundle).Path "latest.json"
$latestJson = $latest | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText($latestPath, $latestJson, [Text.UTF8Encoding]::new($false))
Write-Host "latest.json ($latestPath):" -ForegroundColor Cyan
Write-Host $latestJson

# ---- publish ------------------------------------------------------------------
if ($DryRun) {
    Write-Host "연습 실행 완료: 태그·푸시·릴리스는 하지 않았어요." -ForegroundColor Green
    Write-Host "  설치파일: $setup"
    Write-Host "  서명:     $sig"
    Write-Host "  latest:   $latestPath"
    exit 0
}
$notesPath = Join-Path $env:TEMP "fine-file-notes-$Version.md"
[IO.File]::WriteAllText($notesPath, $notes, [Text.UTF8Encoding]::new($false))
git tag $tag
if ($LASTEXITCODE -ne 0) { Fail "태그 $tag 를 만들지 못했어요" }
git push origin HEAD
if ($LASTEXITCODE -ne 0) { Fail "브랜치를 올리지 못했어요 (태그 $tag 는 이 PC에만 있어요)" }
git push origin $tag
if ($LASTEXITCODE -ne 0) { Fail "태그 $tag 를 올리지 못했어요" }
gh release create $tag $setup $sig $latestPath --repo $Repo --title "fine-file $Version" --notes-file $notesPath
if ($LASTEXITCODE -ne 0) { Fail "릴리스를 만들지 못했어요 (태그 $tag 는 이미 올라갔어요)" }
Write-Host "완료: https://github.com/$Repo/releases/tag/$tag" -ForegroundColor Green
