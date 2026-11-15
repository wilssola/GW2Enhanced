# Fetches cuBLAS, the only CUDA library the translator's CUDA mode loads.
#
#   .\scripts\fetch-cublas.ps1
#
# The official CTranslate2 build (scripts\fetch-ctranslate2.ps1) has the CUDA runtime linked in and loads cuBLAS
# only when a CUDA device is requested (CUDA_DYNAMIC_LOADING): it needs cublas64_12.dll, which needs
# cublasLt64_12.dll, and nothing else (checked with dumpbin /dependents). They go in the package's
# GW2Translated\translator\gpu\ (paths.rs gpu_runtime_dir, passed to the translator as --gpu-runtime).
#
# Downloads NVIDIA's redistributable archive of libcublas for Windows (pinned, sha256 from NVIDIA's
# redistrib_12.8.1.json) and extracts, into translator\vendor\cublas\:
#   cublas64_12.dll, cublasLt64_12.dll   redistributable under the CUDA EULA (Attachment A)
#   cuBLAS-LICENSE.txt                    that EULA, shipped with the DLLs
# 12.8 is the first cuBLAS with kernels for the RTX 50 series (Blackwell); any 12.x works with CTranslate2 4.8
# (it only calls cublasGemmEx/cublasSgemm and friends) and with any driver that runs CUDA 12.

$ErrorActionPreference = 'Stop'
$version = '12.8.4.1'
$archive = "libcublas-windows-x86_64-$version-archive"
$url = "https://developer.download.nvidia.com/compute/cuda/redist/libcublas/windows-x86_64/$archive.zip"
$sha256 = '57a470112cec7e112c95253dde8b3c7184d795dbd92b0bde77a4cb7f8c94c8aa'
$files = [ordered]@{ "bin/cublas64_12.dll" = 'cublas64_12.dll'; "bin/cublasLt64_12.dll" = 'cublasLt64_12.dll'; 'LICENSE' = 'cuBLAS-LICENSE.txt' }

$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root 'translator\vendor\cublas'
if (($files.Values | ForEach-Object { Test-Path (Join-Path $out $_) }) -notcontains $false) {
    Write-Host "cuBLAS already in $out"
    exit 0
}

$work = Join-Path $env:TEMP "gw2t-cublas-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory -Force $work | Out-Null
try {
    $zipPath = Join-Path $work "$archive.zip"
    curl.exe -sSfL -o $zipPath $url
    if ($LASTEXITCODE) { throw "download failed: $url" }
    $got = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
    if ($got -ne $sha256) { throw "sha256 mismatch for ${archive}.zip: $got" }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    New-Item -ItemType Directory -Force $out | Out-Null
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        foreach ($f in $files.Keys) {
            $entry = $zip.Entries | Where-Object FullName -EQ "$archive/$f"
            if (-not $entry) { throw "$f not in $archive.zip" }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $out $files[$f]), $true)
        }
    } finally { $zip.Dispose() }
    Write-Host "cuBLAS $version -> $out"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
