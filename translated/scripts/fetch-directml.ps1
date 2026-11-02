# Fetches the runtime of the translator's DirectML mode.
#
#   .\scripts\fetch-directml.ps1
#
# Downloads the pinned NuGet packages (sha256 verified) and extracts, into
# translator\vendor\directml\:
#   onnxruntime.dll   ONNX Runtime with the DirectML provider (MIT)
#   DirectML.dll      Microsoft's redistributable DirectML (the copy in Windows is too old)
#   *-LICENSE.txt     their licenses, shipped with the DLLs
# translator\build.rs copies them to target\<profile>\directml\.

$ErrorActionPreference = 'Stop'
$packages = @(
    @{ Id = 'microsoft.ml.onnxruntime.directml'; Version = '1.24.4'; Sha256 = '57e9f11b73437bef7a309496135d4c1f96b1a8e9ddba60013fa27bfc1d788681'
       Dll = 'runtimes/win-x64/native/onnxruntime.dll'; License = 'LICENSE'; Name = 'onnxruntime' },
    @{ Id = 'microsoft.ai.directml'; Version = '1.15.4'; Sha256 = '4e7cb7ddce8cf837a7a75dc029209b520ca0101470fcdf275c1f49736a3615b9'
       Dll = 'bin/x64-win/DirectML.dll'; License = 'LICENSE.txt'; Name = 'DirectML' }
)

$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root 'translator\vendor\directml'
if ((Test-Path "$out\onnxruntime.dll") -and (Test-Path "$out\DirectML.dll")) {
    Write-Host "DirectML runtime already in $out"
    exit 0
}
New-Item -ItemType Directory -Force $out | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem

foreach ($p in $packages) {
    $nupkg = Join-Path $env:TEMP "gw2t-$($p.Id)-$($p.Version)-$([guid]::NewGuid().ToString('N').Substring(0, 8)).zip"
    try {
        curl.exe -sSfL -o $nupkg "https://api.nuget.org/v3-flatcontainer/$($p.Id)/$($p.Version)/$($p.Id).$($p.Version).nupkg"
        if ($LASTEXITCODE) { throw "download failed: $($p.Id)" }
        $got = (Get-FileHash $nupkg -Algorithm SHA256).Hash.ToLower()
        if ($got -ne $p.Sha256) { throw "sha256 mismatch for $($p.Id): $got" }
        $zip = [System.IO.Compression.ZipFile]::OpenRead($nupkg)
        try {
            $dll = $zip.Entries | Where-Object FullName -EQ $p.Dll
            $license = $zip.Entries | Where-Object FullName -EQ $p.License
            if (-not $dll -or -not $license) { throw "$($p.Dll) or $($p.License) not in $($p.Id)" }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($dll, (Join-Path $out (Split-Path -Leaf $p.Dll)), $true)
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($license, (Join-Path $out "$($p.Name)-LICENSE.txt"), $true)
        } finally { $zip.Dispose() }
    } finally {
        Remove-Item $nupkg -ErrorAction SilentlyContinue
    }
}
Write-Host "DirectML runtime -> $out"
