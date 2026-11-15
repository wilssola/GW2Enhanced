# Builds the GW2 Enhanced release zips (the "Downloads" table of README.md) in dist\.
#
#   .\scripts\package.ps1                                   every package, version from enhanced\Cargo.toml
#   .\scripts\package.ps1 -Version 1.2.0                    the release workflow (tag v1.2.0)
#   .\scripts\package.ps1 -Package Graphics,Translation-CPU only these
#   .\scripts\package.ps1 -KeepStaging                      also keeps the unzipped folders in dist\
#
# Writes dist\GW2Enhanced-<Package>-<version>.zip, Package one of:
#   All                    the three mods, the translator with every mode
#   Translation            the translation, the translator with every mode (CPU, CUDA, DirectML)
#   Translation-CPU        the translation, the translator on the processor only (smallest)
#   Translation-CUDA       the translation, CPU + CUDA (NVIDIA)
#   Translation-DirectML   the translation, CPU + DirectML (any DirectX 12 card)
#   Textures               the HD textures runtime (the HD pack itself is a separate download)
#   Graphics               the graphics
#
# Each msimg32.dll is built with GW2E_PACKAGE=<Package> and GW2E_VERSION=<version> in the environment: the in-game
# updater reads them (option_env!) to download the same package again. So every package gets its own build, even
# when the cargo features are the same (only the gw2enhanced crate is rebuilt).
#
# Zip layout (extracted into the game folder, next to Gw2-64.exe):
#   msimg32.dll
#   GW2Enhanced-Installer.exe
#   GW2Translated\languages\<language>\dictionary.jsonl, web.jsonl
#   GW2Translated\translator\gw2translated-translator.exe
#   GW2Translated\translator\ctranslate2.dll, libiomp5md.dll                    CTranslate2 (scripts\fetch-ctranslate2.ps1)
#   GW2Translated\translator\msvcp140.dll, msvcp140_1.dll, vcruntime140.dll, vcruntime140_1.dll   VC++ runtime, app-local
#   GW2Translated\translator\models\<language>\                                small model (CPU)
#   GW2Translated\translator\models\<language>-big\     CUDA, DirectML         big model (scripts\convert-big-model.ps1)
#   GW2Translated\translator\gpu\                       CUDA                   cuBLAS (scripts\fetch-cublas.ps1)
#   GW2Translated\translator\directml\                  DirectML               ONNX Runtime + DirectML (scripts\fetch-directml.ps1)
#   GW2Graphics\luts\*.cube                             Graphics               the example colour-grading LUTs
# Never in a zip: config.ini, logs, collected or locally translated strings (checked before zipping).
#
# Everything the translator needs is downloaded (pinned and sha256-checked) by the scripts in translated\scripts, so
# this runs without Git LFS: the small model comes from data\models\<language>\ when LFS has it, else from Argos'
# public index (fetch-model.ps1, byte-identical to the LFS files). The big model is converted from Hugging Face with
# uv (https://docs.astral.sh/uv/) when data\models\<language>-big\ is not there yet.

param(
    [string]$Version,
    [ValidateSet('All', 'Translation', 'Translation-CPU', 'Translation-CUDA', 'Translation-DirectML', 'Textures', 'Graphics')]
    [string[]]$Package = @('All', 'Translation', 'Translation-CPU', 'Translation-CUDA', 'Translation-DirectML', 'Textures', 'Graphics'),
    [string]$OutDir,
    [switch]$KeepStaging
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }
$translated = Join-Path $root 'translated'
$tscripts = Join-Path $translated 'scripts'
$vendor = Join-Path $translated 'translator\vendor'

if (-not $Version) {
    $Version = (Select-String -Path enhanced\Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') { throw "version '$Version' is not <major>.<minor>.<patch>[-pre] (no leading v)" }

# Package -> cargo features of gw2enhanced, and which translator modes it carries.
$spec = @{
    'All'                  = @{ Features = 'translation,textures,graphics'; Modes = 'cpu', 'cuda', 'directml'; Graphics = $true }
    'Translation'          = @{ Features = 'translation'; Modes = 'cpu', 'cuda', 'directml' }
    'Translation-CPU'      = @{ Features = 'translation'; Modes = @('cpu') }
    'Translation-CUDA'     = @{ Features = 'translation'; Modes = 'cpu', 'cuda' }
    'Translation-DirectML' = @{ Features = 'translation'; Modes = 'cpu', 'directml' }
    'Textures'             = @{ Features = 'textures'; Modes = @() }
    'Graphics'             = @{ Features = 'graphics'; Modes = @(); Graphics = $true }
}
$modes = @($Package | ForEach-Object { $spec[$_].Modes } | Sort-Object -Unique)
$needTranslator = $modes.Count -gt 0
$needCuda = $modes -contains 'cuda'
$needDml = $modes -contains 'directml'

function Invoke-Checked([string]$what, [scriptblock]$block) {
    & $block
    if ($LASTEXITCODE) { throw "$what failed (exit code $LASTEXITCODE)" }
}

# ---- runtimes and models ------------------------------------------------------------------------------------------
$smallModels = @{}   # language -> folder
$bigModels = @{}     # language -> folder
if ($needTranslator) {
    # build.rs of the translator needs both runtimes, whatever the package.
    & "$tscripts\fetch-ctranslate2.ps1"
    & "$tscripts\fetch-directml.ps1"
    if ($needCuda) { & "$tscripts\fetch-cublas.ps1" }

    foreach ($model in Get-ChildItem "$translated\data\models" -Directory | Where-Object Name -NotLike '*-big') {
        $lang = $model.Name
        if ((Get-Item "$($model.FullName)\model\model.bin").Length -ge 1MB) {
            $smallModels[$lang] = $model.FullName
        } elseif ($lang -eq 'pt-BR') {
            # Git LFS pointer (LFS is off in CI): the same files from Argos' public index.
            $dir = Join-Path $vendor "models\$lang"
            & "$tscripts\fetch-model.ps1" -Out $dir
            $smallModels[$lang] = $dir
        } else {
            throw "data\models\$lang is a Git LFS pointer and there is no download for it: run 'git lfs pull'"
        }
    }
    if ($needCuda -or $needDml) {
        $lang = 'pt-BR'
        $dir = Join-Path $translated "data\models\$lang-big"
        $complete = 'model\model.bin', 'model\encoder.onnx', 'model\decoder.onnx', 'source.spm', 'target.spm' |
            ForEach-Object { Test-Path (Join-Path $dir $_) }
        if ($complete -contains $false) {
            if (-not (Get-Command uv -ErrorAction SilentlyContinue)) {
                throw "the CUDA and DirectML modes need the big model in $dir, built by translated\scripts\convert-big-model.ps1, which needs uv (https://docs.astral.sh/uv/)"
            }
            & "$tscripts\convert-big-model.ps1" -Out $dir
        }
        $bigModels[$lang] = $dir
    }
}

# ---- build ------------------------------------------------------------------------------------------------------------
$target = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
if ($LASTEXITCODE) { throw 'cargo metadata failed' }
$release = Join-Path $target 'release'

$tools = @('-p', 'gw2enhanced-installer')
if ($needTranslator) { $tools += '-p', 'gw2translated-translator' }
Invoke-Checked 'cargo build (installer, translator)' { cargo build --release --locked @tools }

if (Test-Path $OutDir) {
    foreach ($p in $Package) { Remove-Item (Join-Path $OutDir "GW2Enhanced-$p-$Version*") -Recurse -Force -ErrorAction SilentlyContinue }
}
New-Item -ItemType Directory -Force $OutDir | Out-Null

$saved = @{ GW2E_PACKAGE = $env:GW2E_PACKAGE; GW2E_VERSION = $env:GW2E_VERSION }
try {
    foreach ($p in $Package) {
        $stage = Join-Path $OutDir "GW2Enhanced-$p-$Version"
        New-Item -ItemType Directory -Force $stage | Out-Null
        $env:GW2E_PACKAGE = $p
        $env:GW2E_VERSION = $Version
        Write-Host "== $p ($($spec[$p].Features)) =="
        Invoke-Checked "cargo build ($p)" { cargo build --release --locked -p gw2enhanced --no-default-features --features $spec[$p].Features }
        Copy-Item "$release\msimg32.dll" $stage
    }
} finally {
    $env:GW2E_PACKAGE = $saved.GW2E_PACKAGE
    $env:GW2E_VERSION = $saved.GW2E_VERSION
}

# ---- staging ----------------------------------------------------------------------------------------------------------
$vcRuntime = 'msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll'
$crt = $null
if ($needTranslator) {
    # VC++ runtime of ctranslate2.dll and onnxruntime.dll (the exe itself has the static CRT): from Visual Studio's
    # redistributable folder (app-local deployment), else System32.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $vs = & $vswhere -latest -products * -property installationPath
    $crt = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT" -Directory -ErrorAction SilentlyContinue |
        Sort-Object FullName | Select-Object -Last 1
}

function Copy-Translation([string]$stage, [string[]]$modes) {
    $data = Join-Path $stage 'GW2Translated'
    $tr = Join-Path $data 'translator'
    New-Item -ItemType Directory -Force "$tr\models" | Out-Null
    foreach ($lang in Get-ChildItem "$translated\data\languages" -Directory) {
        $dst = Join-Path $data "languages\$($lang.Name)"
        New-Item -ItemType Directory -Force $dst | Out-Null
        Copy-Item "$($lang.FullName)\dictionary.jsonl" $dst
        if (Test-Path "$($lang.FullName)\web.jsonl") { Copy-Item "$($lang.FullName)\web.jsonl" $dst }
    }
    Copy-Item "$release\gw2translated-translator.exe" $tr
    Copy-Item "$vendor\ctranslate2-4.8.4\ctranslate2.dll", "$vendor\ctranslate2-4.8.4\libiomp5md.dll" $tr
    foreach ($dll in $vcRuntime) {
        $src = if ($crt -and (Test-Path "$($crt.FullName)\$dll")) { "$($crt.FullName)\$dll" } else { "$env:SystemRoot\System32\$dll" }
        Copy-Item $src $tr
    }
    foreach ($lang in $smallModels.Keys) {
        Copy-Item $smallModels[$lang] "$tr\models\$lang" -Recurse
        # The Argos package's sentence splitter: the translator does not use it.
        Remove-Item "$tr\models\$lang\stanza" -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($modes -contains 'cuda' -or $modes -contains 'directml') {
        foreach ($lang in $bigModels.Keys) {
            New-Item -ItemType Directory -Force "$tr\models\$lang-big" | Out-Null
            Copy-Item "$($bigModels[$lang])\*" "$tr\models\$lang-big" -Recurse
        }
    }
    if ($modes -contains 'cuda') {
        New-Item -ItemType Directory -Force "$tr\gpu" | Out-Null
        Copy-Item "$vendor\cublas\*" "$tr\gpu"
    }
    if ($modes -contains 'directml') {
        New-Item -ItemType Directory -Force "$tr\directml" | Out-Null
        Copy-Item "$vendor\directml\*" "$tr\directml"
    }
}

# Files the player's game folder has but a package never may: settings, logs, collection, local translations.
function Assert-NoUserData([string]$stage) {
    $bad = Get-ChildItem $stage -Recurse -File | Where-Object {
        $rel = $_.FullName.Substring($stage.Length + 1)
        $shippedJsonl = $rel -match '^GW2Translated\\languages\\[^\\]+\\(dictionary|web)\.jsonl$'
        $_.Name -eq 'config.ini' -or $_.Name -like '*.log' -or $_.Name -like '*_log.txt' -or $_.Extension -eq '.pdb' -or
            $_.Name -like 'port*.txt' -or ($_.Extension -eq '.jsonl' -and -not $shippedJsonl)
    }
    if ($bad) { throw "user data in $(Split-Path -Leaf $stage): $(($bad | ForEach-Object FullName) -join ', ')" }
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
foreach ($p in $Package) {
    $stage = Join-Path $OutDir "GW2Enhanced-$p-$Version"
    Copy-Item "$release\GW2Enhanced-Installer.exe" $stage
    if ($spec[$p].Modes.Count -gt 0) { Copy-Translation $stage $spec[$p].Modes }
    if ($spec[$p].Graphics) {
        New-Item -ItemType Directory -Force "$stage\GW2Graphics\luts" | Out-Null
        Copy-Item "$root\graphics\presets\luts\*.cube" "$stage\GW2Graphics\luts"
    }
    Assert-NoUserData $stage
    $zip = "$stage.zip"
    [System.IO.Compression.ZipFile]::CreateFromDirectory($stage, $zip, [System.IO.Compression.CompressionLevel]::Optimal, $false)
    if (-not $KeepStaging) { Remove-Item $stage -Recurse -Force }
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    Write-Host ("{0}  {1}  ({2:N1} MB)" -f $hash, (Split-Path -Leaf $zip), ((Get-Item $zip).Length / 1MB))
}
