$ErrorActionPreference = 'Stop'
$pin = @(Get-Content -LiteralPath 'release-pin')
if ($pin[1] -eq 'unpublished') { throw 'herdr-wt install: no prebuilt release published yet' }
if ($pin[1] -cnotmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
    throw 'herdr-wt install: invalid release pin'
}
if ($pin[2] -cnotmatch '^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+$') {
    throw 'herdr-wt install: invalid release repository'
}
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') { throw "herdr-wt install: unsupported Windows architecture: $arch" }
$asset = 'herdr-wt-windows-x86_64.zip'
$digest = @($pin | Where-Object { $_ -cmatch "^[0-9a-f]{64}  $([regex]::Escape($asset))$" })
if ($digest.Count -ne 1) { throw "herdr-wt install: missing or invalid SHA-256 for $asset" }
$expected = $digest[0].Substring(0, 64)

$scratch = Join-Path $PWD ('.herdr-wt-install-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $zip = Join-Path $scratch 'package.zip'
    if ($env:HERDR_WT_TEST_ASSET_DIR) {
        Copy-Item -LiteralPath (Join-Path $env:HERDR_WT_TEST_ASSET_DIR $asset) -Destination $zip
    } else {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $url = "https://github.com/$($pin[2])/releases/download/$($pin[1])/$asset"
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
    }
    if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ne $expected) {
        throw "herdr-wt install: checksum mismatch for $asset"
    }
    Add-Type -AssemblyName 'System.IO.Compression.FileSystem'
    $archive = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $entry = $archive.GetEntry('bin/herdr-wt.exe')
        if (!$entry) { throw 'herdr-wt install: archive has no usable binary' }
        $binary = Join-Path $scratch 'herdr-wt.exe'
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $binary)
    } finally {
        $archive.Dispose()
    }
    New-Item -ItemType Directory -Force -Path 'bin' | Out-Null
    Move-Item -LiteralPath $binary -Destination 'bin/herdr-wt.exe' -Force
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
