<#
.SYNOPSIS
Builds the Windows packages into dist\: OpenXCloud-windows-x64-setup.exe (Inno Setup)
and OpenXCloud-windows-x64.zip (portable folder). See PACKAGING.md.

.DESCRIPTION
Builds the app with the static MSVC runtime, stages it with the FFmpeg DLLs it imports
(found by reading PE import tables, transitively), rejects any other non-system DLL
dependency, records the FFmpeg build and licences, starts the staged exe once to prove
every DLL resolves, then zips the folder and compiles the installer.

.PARAMETER FfmpegDir
FFmpeg shared build with bin\, lib\, include\ (default: $env:FFMPEG_DIR).

.PARAMETER Iscc
Inno Setup 6 compiler (default: $env:ISCC, PATH, then the usual install folders).
#>
param(
    [string]$FfmpegDir = $env:FFMPEG_DIR,
    [string]$Iscc = $env:ISCC
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repo

if (-not $FfmpegDir -or -not (Test-Path (Join-Path $FfmpegDir 'bin'))) {
    throw 'Set FFMPEG_DIR (or -FfmpegDir) to an FFmpeg 9 shared build with bin\, lib\ and include\.'
}
$FfmpegDir = (Resolve-Path $FfmpegDir).Path
if (-not $Iscc) {
    $found = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($found) { $Iscc = $found.Source }
}
if (-not $Iscc) {
    $candidates = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    $Iscc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Iscc) { throw 'Inno Setup 6 not found: install it or pass -Iscc path\to\ISCC.exe.' }

# --- Build -----------------------------------------------------------------------------
# Static CRT: no vcruntime140.dll / VC++ redistributable needed on the user's machine.
# A separate target dir keeps these flags from invalidating the normal dev build cache.
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target\windows-package' }
# opus is built by CMake with /MD regardless; its objects then ask for msvcrt.lib, which
# conflicts with the static CRT (LNK4098). Every symbol resolves from the static CRT, so
# drop that default library instead of linking two runtimes.
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static -C link-arg=/NODEFAULTLIB:msvcrt.lib'
$env:FFMPEG_DIR = $FfmpegDir
cargo build --locked --release -p xcloud-app --all-features --target-dir $targetDir
if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
$exe = Join-Path $targetDir 'release\openxcloud.exe'

$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object { $_.name -eq 'xcloud-app' }).version
# Windows file versions are four numbers; drop any prerelease/build suffix.
$numericVersion = (($version -split '[-+]')[0]) + '.0'

# --- PE imports ------------------------------------------------------------------------
function Get-PeImports([string]$Path) {
    $b = [IO.File]::ReadAllBytes($Path)
    $pe = [BitConverter]::ToInt32($b, 0x3C)
    if ([BitConverter]::ToUInt32($b, $pe) -ne 0x4550) { throw "$Path is not a PE file" }
    $sections = [BitConverter]::ToUInt16($b, $pe + 6)
    $optSize = [BitConverter]::ToUInt16($b, $pe + 20)
    $opt = $pe + 24
    if ([BitConverter]::ToUInt16($b, $opt) -ne 0x20B) { throw "$Path is not a 64-bit PE file" }
    $sectionTable = $opt + $optSize
    $toOffset = {
        param([uint32]$rva)
        for ($i = 0; $i -lt $sections; $i++) {
            $s = $sectionTable + 40 * $i
            $va = [BitConverter]::ToUInt32($b, $s + 12)
            $size = [Math]::Max([BitConverter]::ToUInt32($b, $s + 8), [BitConverter]::ToUInt32($b, $s + 16))
            if ($rva -ge $va -and $rva -lt $va + $size) {
                return [BitConverter]::ToUInt32($b, $s + 20) + ($rva - $va)
            }
        }
        throw "RVA $rva of $Path is outside every section"
    }
    $readName = {
        param([uint32]$rva)
        $o = & $toOffset $rva
        $end = [Array]::IndexOf($b, [byte]0, [int]$o)
        [Text.Encoding]::ASCII.GetString($b, $o, $end - $o)
    }
    $names = New-Object System.Collections.Generic.List[string]
    # Data directories start 112 bytes into a PE32+ optional header: 1 = imports,
    # 13 = delay-load imports (needed just as much, only later).
    foreach ($dir in @(@{ Index = 1; Size = 20; NameAt = 12 }, @{ Index = 13; Size = 32; NameAt = 4 })) {
        $rva = [BitConverter]::ToUInt32($b, $opt + 112 + 8 * $dir.Index)
        if ($rva -eq 0) { continue }
        $o = & $toOffset $rva
        while ($true) {
            $nameRva = [BitConverter]::ToUInt32($b, $o + $dir.NameAt)
            if ($nameRva -eq 0) { break }
            $names.Add((& $readName $nameRva).ToLowerInvariant())
            $o += $dir.Size
        }
    }
    $names | Sort-Object -Unique
}

# DLLs every supported Windows install ships. Anything else must be bundled; the VC++
# runtime (vcruntime140.dll, msvcp140.dll) must not appear at all (static CRT).
$systemDlls = @(
    'advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'cfgmgr32.dll', 'comctl32.dll',
    'comdlg32.dll', 'crypt32.dll', 'd3d11.dll', 'd3d12.dll', 'd3dcompiler_47.dll', 'dbghelp.dll',
    'dcomp.dll', 'dwmapi.dll', 'dxgi.dll', 'gdi32.dll', 'hid.dll', 'imm32.dll', 'iphlpapi.dll',
    'kernel32.dll', 'mfplat.dll', 'msvcrt.dll', 'ncrypt.dll', 'ntdll.dll', 'ole32.dll',
    'oleaut32.dll', 'opengl32.dll', 'powrprof.dll', 'propsys.dll', 'psapi.dll', 'secur32.dll',
    'setupapi.dll', 'shell32.dll', 'shlwapi.dll', 'user32.dll', 'userenv.dll', 'uxtheme.dll',
    'version.dll', 'winmm.dll', 'ws2_32.dll', 'xinput1_4.dll', 'avrt.dll', 'ksuser.dll',
    'mmdevapi.dll', 'dnsapi.dll', 'normaliz.dll', 'rpcrt4.dll', 'winhttp.dll', 'wtsapi32.dll',
    # winit/egui (COM, DirectWrite, UI Automation for accesskit) and FFmpeg (GDI text
    # and capture helpers in avcodec/avdevice).
    'combase.dll', 'dwrite.dll', 'uiautomationcore.dll', 'd2d1.dll', 'msimg32.dll', 'usp10.dll',
    'avicap32.dll'
)
function Test-SystemDll([string]$Name) {
    $Name -like 'api-ms-win-*' -or $Name -like 'ext-ms-*' -or $systemDlls -contains $Name
}

# --- Stage -----------------------------------------------------------------------------
$dist = Join-Path $repo 'dist'
$stageRoot = Join-Path $dist 'stage'
$stage = Join-Path $stageRoot 'OpenXCloud'
if (Test-Path $stageRoot) { Remove-Item -Recurse -Force $stageRoot }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe $stage

$ffmpegBin = Join-Path $FfmpegDir 'bin'
$queue = New-Object System.Collections.Generic.Queue[string]
$queue.Enqueue((Join-Path $stage 'openxcloud.exe'))
$seen = @{}
$missing = New-Object System.Collections.Generic.List[string]
while ($queue.Count -gt 0) {
    $file = $queue.Dequeue()
    foreach ($dll in Get-PeImports $file) {
        if ($seen.ContainsKey($dll)) { continue }
        $seen[$dll] = $true
        $bundled = Join-Path $ffmpegBin $dll
        if (Test-Path $bundled) {
            Copy-Item $bundled $stage
            $queue.Enqueue((Join-Path $stage $dll))
        } elseif (-not (Test-SystemDll $dll)) {
            $missing.Add("$dll (imported by $([IO.Path]::GetFileName($file)))")
        }
    }
}
if ($missing.Count -gt 0) {
    throw "Unbundled non-system DLL dependencies:`n  $($missing -join "`n  ")"
}
$bundledDlls = Get-ChildItem $stage -Filter *.dll | ForEach-Object Name
Write-Host "bundled: $($bundledDlls -join ', ')"

# Licences and the record of the exact FFmpeg build (THIRD_PARTY_NOTICES.md asks for it).
Copy-Item (Join-Path $repo 'LICENSE') (Join-Path $stage 'LICENSE.txt')
Copy-Item (Join-Path $repo 'THIRD_PARTY_NOTICES.md') $stage
$licenses = Join-Path $stage 'licenses'
New-Item -ItemType Directory -Force $licenses | Out-Null
Copy-Item (Join-Path $FfmpegDir 'LICENSE.txt') (Join-Path $licenses 'FFmpeg-LICENSE.txt')
$ffmpegExe = Join-Path $ffmpegBin 'ffmpeg.exe'
$ffmpegVersion = if (Test-Path $ffmpegExe) { (& $ffmpegExe -hide_banner -version) -join "`n" } else { '(ffmpeg.exe not in the build; version unknown)' }
# The licence follows FFmpeg's configure switches (see its LICENSE.md).
$gpl = $ffmpegVersion -match '--enable-gpl'
$v3 = $ffmpegVersion -match '--enable-version3'
$ffmpegLicense = '{0} General Public License, version {1} or later' -f `
    $(if ($gpl) { 'GNU' } else { 'GNU Lesser' }), $(if ($v3) { '3' } elseif ($gpl) { '2' } else { '2.1' })
@"
OpenXCloud bundles these FFmpeg libraries, dynamically linked, unmodified:
  $($bundledDlls -join ', ')

FFmpeg is free software; this build is licensed under the $ffmpegLicense
(see FFmpeg-LICENSE.txt). You may replace the DLLs with your own compatible build.
Source code: https://ffmpeg.org/download.html; the exact revision is the git hash in the
version line below. Windows build scripts: https://github.com/BtbN/FFmpeg-Builds

Archive: $(if ($env:FFMPEG_ARCHIVE_URL) { $env:FFMPEG_ARCHIVE_URL } else { '(local build, not recorded)' })
SHA-256: $(if ($env:FFMPEG_ARCHIVE_SHA256) { $env:FFMPEG_ARCHIVE_SHA256 } else { '(not recorded)' })

$ffmpegVersion
"@ | Set-Content -Encoding utf8 (Join-Path $licenses 'FFmpeg-BUILD.txt')

# A missing DLL fails process creation (0xC0000135), so one start proves the import set.
$probe = Start-Process -FilePath (Join-Path $stage 'openxcloud.exe') -ArgumentList '--version' `
    -WorkingDirectory $stage -Wait -PassThru -WindowStyle Hidden
if ($probe.ExitCode -ne 0) { throw "staged openxcloud.exe --version exited with $($probe.ExitCode)" }

# --- Packages --------------------------------------------------------------------------
$zip = Join-Path $dist 'OpenXCloud-windows-x64.zip'
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal

$setupName = 'OpenXCloud-windows-x64-setup'
& $Iscc /Qp "/DAppVersion=$version" "/DNumericVersion=$numericVersion" "/DStageDir=$stage" `
    "/DRepoDir=$repo" "/DOutputDir=$dist" "/DOutputName=$setupName" `
    (Join-Path $repo 'packaging\windows\openxcloud.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }

Get-Item $zip, (Join-Path $dist "$setupName.exe") | ForEach-Object {
    '{0}  {1:N1} MB' -f $_.Name, ($_.Length / 1MB)
}
