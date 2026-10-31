# Converts the big OPUS-MT model (Helsinki-NLP/opus-mt-tc-big-en-pt, CC-BY 4.0)
# to CTranslate2 float16, for the translator's GPU modes.
#
#   .\scripts\convert-big-model.ps1                  -> data\models\pt-BR-big\
#   .\scripts\convert-big-model.ps1 -Out <folder>
#   .\scripts\convert-big-model.ps1 -GraphsOnly      keeps model.bin, rewrites the ONNX graphs
#
# Result (the layout the translator expects for --model-big):
#   model\model.bin, model\config.json, model\shared_vocabulary.json
#   model\encoder.onnx, model\decoder.onnx   DirectML: graphs of half a MB that use the
#                                            weights in model.bin (scripts\export_onnx.py)
#   source.spm, target.spm          SentencePiece models (English in, Portuguese out)
#   gw2translated.json              {"source_prefix": ">>pob<<"}: target-language token
#                                   that the multilingual model needs at the start
#   README.md                       license/attribution
#
# Needs uv (https://docs.astral.sh/uv/). Everything temporary (Python env, the
# downloaded PyTorch weights) lives under %TEMP% and is deleted at the end.

param([string]$Out = (Join-Path (Split-Path -Parent $PSScriptRoot) 'data\models\pt-BR-big'), [switch]$GraphsOnly)
$ErrorActionPreference = 'Stop'
$repo = 'Helsinki-NLP/opus-mt-tc-big-en-pt'
$work = Join-Path $env:TEMP "gw2t-convert-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory -Force "$work\hf" | Out-Null
try {
    # curl.exe -4: Python's HTTP stack falls over on machines with broken IPv6.
    foreach ($f in 'config.json', 'generation_config.json', 'model.safetensors', 'source.spm', 'target.spm',
        'vocab.json', 'tokenizer_config.json', 'special_tokens_map.json', 'README.md') {
        curl.exe -sSfL -4 -o "$work\hf\$f" "https://huggingface.co/$repo/resolve/main/$f"
        if ($LASTEXITCODE) { throw "download of $f failed" }
    }
    $env:UV_CACHE_DIR = "$work\uvcache"
    uv venv -q --python 3.12 "$work\venv"
    if ($LASTEXITCODE) { throw 'uv venv failed' }
    $py = "$work\venv\Scripts\python.exe"
    uv pip install -q --python $py --index-url https://download.pytorch.org/whl/cpu --extra-index-url https://pypi.org/simple `
        --index-strategy unsafe-best-match ctranslate2==4.8.4 torch 'transformers<5' sentencepiece onnx
    if ($LASTEXITCODE) { throw 'uv pip install failed' }

    if (-not $GraphsOnly) {
        if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
        New-Item -ItemType Directory -Force $Out | Out-Null
        & "$work\venv\Scripts\ct2-transformers-converter.exe" --model "$work\hf" --output_dir "$Out\model" --quantization float16
        if ($LASTEXITCODE) { throw 'conversion failed' }
    }
    & $py -W ignore "$PSScriptRoot\export_onnx.py" "$work\hf" "$Out\model"
    if ($LASTEXITCODE) { throw 'ONNX export failed' }
    Copy-Item "$work\hf\source.spm", "$work\hf\target.spm" $Out
    '{"source_prefix": ">>pob<<"}' | Set-Content -NoNewline -Encoding utf8NoBOM "$Out\gw2translated.json"
    @"
# English - Portuguese (Brazil), big model (GPU mode)

Converted to CTranslate2 (float16) from [$repo](https://huggingface.co/$repo).

Authors: Jörg Tiedemann and Santhosh Thottingal
Title: "OPUS-MT — Building open translation services for the World"
Book Title: Proceedings of the 22nd Annual Conference of the European Association for Machine Translation (EAMT)
Year: 2020

The original model is licensed CC-BY 4.0.
"@ | Set-Content -Encoding utf8NoBOM "$Out\README.md"
    $mb = [math]::Round(((Get-ChildItem $Out -Recurse -File | Measure-Object Length -Sum).Sum) / 1MB, 1)
    Write-Host "big model: $Out ($mb MB)"
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
