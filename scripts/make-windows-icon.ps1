# Packs the PNG exports in assets/icons into assets/icons/openxcloud.ico (PNG-compressed
# ICO entries, Windows Vista and later). Rerun after regenerating the PNGs.
$ErrorActionPreference = 'Stop'
$icons = Join-Path $PSScriptRoot '..\assets\icons'
# 16/32 for title bars and lists, 64 (the 32@2x export) for 200% scaling, 128/256 for
# Explorer's large views.
$sources = 'openxcloud-16.png', 'openxcloud-32.png', 'openxcloud-32@2x.png', 'openxcloud-128.png', 'openxcloud-256.png'

$images = foreach ($name in $sources) {
    $bytes = [IO.File]::ReadAllBytes((Join-Path $icons $name))
    # PNG signature, then the IHDR chunk: big-endian width and height at offsets 16 and 20.
    $width = [int]$bytes[16] * 16777216 + [int]$bytes[17] * 65536 + [int]$bytes[18] * 256 + [int]$bytes[19]
    $height = [int]$bytes[20] * 16777216 + [int]$bytes[21] * 65536 + [int]$bytes[22] * 256 + [int]$bytes[23]
    if ($width -ne $height -or $width -gt 256) { throw "$name is ${width}x${height}; ICO entries must be square and at most 256px" }
    [pscustomobject]@{ Size = $width; Bytes = $bytes }
}

$out = New-Object IO.MemoryStream
$writer = New-Object IO.BinaryWriter $out
# ICONDIR: reserved, type 1 (icon), image count.
$writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($image in $images) {
    # ICONDIRENTRY: width/height (0 means 256), colour count, reserved, planes, bit count,
    # data size, data offset.
    $dim = if ($image.Size -eq 256) { 0 } else { $image.Size }
    $writer.Write([byte]$dim); $writer.Write([byte]$dim); $writer.Write([byte]0); $writer.Write([byte]0)
    $writer.Write([uint16]1); $writer.Write([uint16]32)
    $writer.Write([uint32]$image.Bytes.Length); $writer.Write([uint32]$offset)
    $offset += $image.Bytes.Length
}
foreach ($image in $images) { $writer.Write($image.Bytes) }
$writer.Flush()
$target = Join-Path $icons 'openxcloud.ico'
[IO.File]::WriteAllBytes($target, $out.ToArray())
Write-Host "wrote $target ($($images.Size -join ', ') px)"
