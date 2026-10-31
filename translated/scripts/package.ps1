# Builds the GW2 Translated release package in dist\.
#
#   .\scripts\package.ps1            dist\GW2Translated-v<version>\ folder and the .zip
#   .\scripts\package.ps1 -NoBuild   uses the already compiled binaries
#
# Contents (the installer expects exactly this):
#   GW2Translated-Installer.exe
#   version.dll
#   GW2Translated\languages\<language>\dictionary.jsonl
#   GW2Translated\translator\gw2translated-translator.exe
#   GW2Translated\translator\ctranslate2.dll, libiomp5md.dll        (official CTranslate2 build)
#   GW2Translated\translator\msvcp140.dll, vcruntime140.dll, vcruntime140_1.dll   (VC++ runtime, app-local)
#   GW2Translated\translator\directml\onnxruntime.dll, DirectML.dll + licenses     (DirectML mode)
#   GW2Translated\translator\models\<language>\...
#
# On-demand packs (not in the base package; downloaded when the player picks the GPU):
#   dist\GW2Translated-model-<language>-big-v<version>.zip   big model for the CUDA and DirectML modes
#   cuBLAS (cublas64_12.dll + cublasLt64_12.dll) comes from NVIDIA's own redistributable.
#
# Needs translator\vendor\ (scripts\fetch-ctranslate2.ps1 and fetch-directml.ps1, run
# automatically unless -NoBuild) and, for the big-model pack, data\models\<language>-big\
# (scripts\convert-big-model.ps1).

param([switch]$NoBuild)
$ErrorActionPreference = 'Stop'

# Next to the translator: the official CTranslate2 build (copied to target\release by
# translator\build.rs) and the VC++ runtime it links against (app-local, so players
# don't need the Visual C++ Redistributable).
$translatorDlls = 'ctranslate2.dll', 'libiomp5md.dll'
$vcRuntime = 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $NoBuild) {
    & "$PSScriptRoot\fetch-ctranslate2.ps1"
    & "$PSScriptRoot\fetch-directml.ps1"
    cargo build --release -p gw2translated -p gw2translated-translator -p gw2translated-installer
    if ($LASTEXITCODE) { throw "cargo build failed" }
}

$name = "GW2Translated-v$version"
$out = Join-Path $root "dist\$name"
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
$data = Join-Path $out 'GW2Translated'
New-Item -ItemType Directory -Force "$data\translator\models" | Out-Null

Copy-Item target\release\GW2Translated-Installer.exe, target\release\version.dll $out
Copy-Item target\release\gw2translated-translator.exe "$data\translator\"
foreach ($dll in $translatorDlls) { Copy-Item "target\release\$dll" "$data\translator\" }
Copy-Item target\release\directml "$data\translator\directml" -Recurse
# VC++ runtime: from Visual Studio's redistributable folder (app-local deployment), else System32.
$vs = & (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe') -latest -products * -property installationPath
$crt = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT" -Directory -ErrorAction SilentlyContinue | Select-Object -Last 1
foreach ($dll in $vcRuntime) {
    $src = if ($crt -and (Test-Path "$($crt.FullName)\$dll")) { "$($crt.FullName)\$dll" } else { "$env:SystemRoot\System32\$dll" }
    Copy-Item $src "$data\translator\"
}
foreach ($lang in Get-ChildItem data\languages -Directory) {
    $dst = "$data\languages\$($lang.Name)"
    New-Item -ItemType Directory -Force $dst | Out-Null
    Copy-Item "$($lang.FullName)\dictionary.jsonl" $dst
}
$bigModels = @()
foreach ($model in Get-ChildItem data\models -Directory) {
    if ($model.Name -like '*-big') { $bigModels += $model; continue }
    if ((Get-Item "$($model.FullName)\model\model.bin").Length -lt 1MB) {
        throw "data\models\$($model.Name) looks like a Git LFS pointer: run 'git lfs pull'"
    }
    Copy-Item $model.FullName "$data\translator\models\$($model.Name)" -Recurse
}

$zip = "$out.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path "$out\*" -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
"$hash  $name.zip" | Set-Content -NoNewline -Encoding ascii "$zip.sha256"
Write-Host "package: $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"
Write-Host "sha256: $hash"

# On-demand big-model packs: the zip holds the model folder (model\, *.spm, gw2translated.json).
foreach ($model in $bigModels) {
    foreach ($graph in 'encoder.onnx', 'decoder.onnx') {
        if (-not (Test-Path "$($model.FullName)\model\$graph")) {
            throw "data\models\$($model.Name)\model\$graph is missing: run scripts\convert-big-model.ps1 -GraphsOnly"
        }
    }
    $packZip = Join-Path $root "dist\GW2Translated-model-$($model.Name)-v$version.zip"
    if (Test-Path $packZip) { Remove-Item $packZip -Force }
    Compress-Archive -Path "$($model.FullName)\*" -DestinationPath $packZip -CompressionLevel Optimal
    $packHash = (Get-FileHash $packZip -Algorithm SHA256).Hash.ToLower()
    "$packHash  $(Split-Path -Leaf $packZip)" | Set-Content -NoNewline -Encoding ascii "$packZip.sha256"
    Write-Host "model pack: $packZip ($([math]::Round((Get-Item $packZip).Length / 1MB, 1)) MB)"
}
