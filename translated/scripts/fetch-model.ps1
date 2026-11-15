# Fetches the translator's small model (the CPU mode) without Git LFS.
#
#   .\scripts\fetch-model.ps1                 -> translator\vendor\models\pt-BR\
#   .\scripts\fetch-model.ps1 -Out <folder>
#
# data\models\pt-BR\ is the Argos Translate package `translate-en_pb` 1.9 (OPUS-MT, CC-BY 4.0) without its
# stanza\ folder; its two big files (model\model.bin, sentencepiece.model) are stored in Git LFS. Where LFS is
# not available (CI: LFS is off on the GitHub repo), this downloads the same package from Argos' public index
# (https://github.com/argosopentech/argospm-index), checks it, and writes the folder exactly as an LFS checkout
# has it: the small files come from data\models\pt-BR\, the two big ones from the package. Their SHA-256 below
# are the LFS object ids of the files in git, so the result is byte-identical to `git lfs pull`.

param([string]$Out = (Join-Path (Split-Path -Parent $PSScriptRoot) 'translator\vendor\models\pt-BR'))
$ErrorActionPreference = 'Stop'
$url = 'https://argos-net.com/v1/translate-en_pb-1_9.argosmodel'
$sha256 = '1d1cd5e9540c6b38c258bed002a42d3b311b8a189acb74feaa311ef30d175c5b'
$prefix = 'translate-en_pb-1_9/'
# Path inside the model folder -> SHA-256 (= the oid in its LFS pointer in data\models\pt-BR\).
$big = [ordered]@{
    'model/model.bin'     = 'a68dd89c9d226e09cef948a01ded22bede8858326af89644f1b715efe39e7018'
    'sentencepiece.model' = '7072c1746f72ca1335eadba63288f7fd938f6107ee1f83494babdae3bba47192'
}

$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root 'data\models\pt-BR'

function Test-Model([string]$dir) {
    foreach ($f in $big.Keys) {
        $p = Join-Path $dir $f
        if (-not (Test-Path $p) -or (Get-FileHash $p -Algorithm SHA256).Hash.ToLower() -ne $big[$f]) { return $false }
    }
    return $true
}

if (Test-Model $Out) {
    Write-Host "small model already in $Out"
    exit 0
}

$work = Join-Path $env:TEMP "gw2t-model-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory -Force $work | Out-Null
try {
    $pkg = Join-Path $work 'model.argosmodel'
    curl.exe -sSfL -o $pkg $url
    if ($LASTEXITCODE) { throw "download failed: $url" }
    $got = (Get-FileHash $pkg -Algorithm SHA256).Hash.ToLower()
    if ($got -ne $sha256) { throw "sha256 mismatch for ${url}: $got" }

    if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
    New-Item -ItemType Directory -Force (Split-Path -Parent $Out) | Out-Null
    # Small files (config, vocabulary, README, metadata) as they are in git.
    Copy-Item $src $Out -Recurse
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($pkg)
    try {
        foreach ($f in $big.Keys) {
            $entry = $zip.Entries | Where-Object FullName -EQ "$prefix$f"
            if (-not $entry) { throw "$f not in the package" }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $Out $f), $true)
        }
    } finally { $zip.Dispose() }
    if (-not (Test-Model $Out)) { throw "the files in $url do not match the LFS objects of data\models\pt-BR" }
    Write-Host "small model -> $Out"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
