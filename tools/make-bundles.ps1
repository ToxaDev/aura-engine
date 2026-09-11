<#
.SYNOPSIS
    Builds the ready-to-run bundles: the app with its FIR filters already in it.

.DESCRIPTION
    A first-time user should be able to download one file, unzip it, drop a
    track on the window and hear the result. Without this, they download a 6 MB
    app, hit "missing filter", work out which of five packs they need, download
    a gigabyte of it and extract it into the right subfolder. This script builds
    the one-file version of that.

    Three tiers, chosen so the ladder is obvious:

      1M   Starter    every FS multiplier, so nothing in the interface is a
                      dead end. Small enough to try on a whim.
      10M  Standard   FS8 only. The setting most people should actually use.
      30M  Reference  FS8 only. The maximum the engine designs for.

    Each bundle carries both phase types, because Hybrid-Phase renders the
    minimum-phase branch in full and half a pair would fail. Each carries both
    the 44.1 and 48 kHz families, so neither kind of source is a surprise.

    THE EXECUTABLE IS TAKEN FROM THE PUBLISHED RELEASE, NOT FROM A LOCAL BUILD.
    The repo's .cargo/config.toml compiles with `target-cpu=native`, which is
    right for a private build and wrong for one handed to strangers: it would
    fault on any machine whose CPU lacks an instruction this one has. The
    release workflow builds with `target-cpu=x86-64-v3` for exactly that
    reason, so the bundles must reuse its binary rather than make their own.
    Pass -AppZip to override with a zip you built the same way.

.EXAMPLE
    # See what would be built, with sizes, without writing anything
    .\tools\make-bundles.ps1 -Tag v1.2.0 -ListOnly

.EXAMPLE
    # Build the three bundles into .\dist
    .\tools\make-bundles.ps1 -Tag v1.2.0

.EXAMPLE
    # Build and attach them to the release
    .\tools\make-bundles.ps1 -Tag v1.2.0 -Upload
#>

[CmdletBinding()]
param(
    # Release tag the bundles belong to. Also the version in their filenames,
    # and where -Upload attaches them.
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v\d+\.\d+\.\d+')]
    [string]$Tag,

    # Which tiers to build.
    [ValidateSet('1M', '5M', '10M', '30M')]
    [string[]]$Tiers = @('1M', '10M', '30M'),

    # Folder holding the generated fir_*.npy blobs.
    [string]$FilterDir,

    # Where the finished zips go.
    [string]$OutDir,

    # A locally built portable zip to take aura-engine.exe from, instead of
    # downloading the released one. Only use a zip built with the release
    # workflow's RUSTFLAGS — see the note above.
    [string]$AppZip,

    [string]$Repo = 'ToxaDev/aura-engine',

    # Print the plan and the sizes, write nothing.
    [switch]$ListOnly,

    # Attach the finished zips to the release.
    [switch]$Upload,

    # Overwrite zips that are already in the output folder.
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression.FileSystem

$RepoRoot = Split-Path -Parent $PSScriptRoot
if (-not $FilterDir) { $FilterDir = Join-Path $RepoRoot 'fir-optimizer\output' }
if (-not $OutDir)    { $OutDir    = Join-Path $RepoRoot 'dist' }

# Absolute from here on. A relative -OutDir would otherwise reach the
# free-space check with no drive qualifier and take it down.
function Resolve-AbsolutePath([string]$path) {
    if ([System.IO.Path]::IsPathRooted($path)) { return [System.IO.Path]::GetFullPath($path) }
    return [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path $path))
}
$FilterDir = Resolve-AbsolutePath $FilterDir
$OutDir    = Resolve-AbsolutePath $OutDir

# ── What goes in each tier ────────────────────────────────────────────────
# FS8 is 44 100x8 and 48 000x8. The starter tier gets the whole rate ladder
# because at 8 MB a blob it costs ~96 MB to make every slider position real.
$Fs8Rates = @(352800, 384000)
$AllRates = @(88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000)

$TierSpec = [ordered]@{
    '1M'  = @{ Name = 'Starter';   Rates = $AllRates; Blurb = 'every FS multiplier (FS2-FS16)' }
    '5M'  = @{ Name = 'Balanced';  Rates = $Fs8Rates; Blurb = 'FS8 (352.8 / 384 kHz)' }
    '10M' = @{ Name = 'Standard';  Rates = $Fs8Rates; Blurb = 'FS8 (352.8 / 384 kHz)' }
    '30M' = @{ Name = 'Reference'; Rates = $Fs8Rates; Blurb = 'FS8 (352.8 / 384 kHz)' }
}

# Decimal megabytes, not mebibytes: this is the number GitHub prints beside
# the asset on the release page, and a README that disagrees with it by 7%
# reads like a mistake.
function Format-Size([long]$bytes) {
    if ($bytes -ge 1000000000) { return ('{0:N2} GB' -f ($bytes / 1000000000)) }
    if ($bytes -ge 1000000)    { return ('{0:N0} MB' -f ($bytes / 1000000)) }
    return ('{0:N0} KB' -f ($bytes / 1000))
}

# The text file a user reads after unzipping. It has one job: tell them the
# filters are already there and which one the app will open on, so they stop
# looking for a step they do not have to take.
function New-BundleReadme([string]$Tier, [string]$Label, [string]$Blurb, [string]$Tag) {
    $rates = 'FS8 only - 352.8 kHz from 44.1 kHz sources, 384 kHz from 48 kHz ones'
    if ($Tier -eq '1M') { $rates = 'every FS multiplier - FS2, FS4, FS8 and FS16, both rate families' }
    return @(
        "AuraEngine $Tag - $Label bundle ($Tier taps)",
        'https://github.com/ToxaDev/aura-engine',
        'Docs: https://toxadev.github.io/aura-engine/',
        '',
        'THE FILTERS ARE ALREADY HERE',
        "  This is the complete package: the app plus its $Tier-tap FIR filters,",
        '  in fir-optimizer\output\. There is no second download and no setup',
        '  step. Run aura-engine.exe and drop audio files on the window.',
        '',
        "  Included: $rates,",
        '  in both linear and minimum phase - the second is what Hybrid-Phase',
        '  renders its transient branch from.',
        '',
        "  The app opens on $Tier taps at FS8 because that is what it finds next",
        '  to itself. Slider positions with no filter behind them are struck',
        '  through, and selecting one names the download that would fill it in.',
        '',
        'ADDING MORE',
        '  Unzip another bundle into this same folder and the app offers both tap',
        '  counts - the filter files merge, and only one copy of the app is used.',
        '  Individual filter packs are on the Releases page. Or generate any',
        '  combination yourself with fir-optimizer/optimize.py --all-ratios.',
        '  AURA_FILTER_DIR points the app at filters kept somewhere else.',
        '',
        'REQUIREMENTS',
        '  * Windows 10/11 x64, CPU with AVX2 (any Intel/AMD from ~2013).',
        '  * WebView2 runtime - preinstalled on Windows 11, and on Windows 10',
        '    through Edge. Nothing else: decoding and FLAC encoding are built in,',
        '    and no external tool is started during a conversion.',
        '  * Optional: a Vulkan GPU for the accelerated double-single path. The',
        '    app falls back to the CPU reference path on its own.',
        '',
        'WHAT YOU GET OUT',
        '  A 24-bit FLAC next to each source file, named like',
        "    Track [AE - 44.1k->352.8k - Kaiser $Tier - f64 - AA - HP].flac",
        '  A VERIFIED badge means the written file was re-decoded and matched the',
        '  internal f64 buffer. The console window is the audit log; it is meant',
        '  to be there.',
        '',
        'LICENSE',
        '  PolyForm Noncommercial 1.0.0 - free for noncommercial use.',
        '  Commercial licensing: auraengine.dev@gmail.com',
        '',
        'CONTACT',
        '  Something broken     https://github.com/ToxaDev/aura-engine/issues',
        '  Questions and ideas  https://github.com/ToxaDev/aura-engine/discussions',
        '  Anything private     auraengine.dev@gmail.com'
    )
}

# The release body. Generated rather than written by hand so the sizes and
# hashes in it are the ones that were actually built.
function New-ReleaseNotes($Results, [string]$Tag, [string]$Repo) {
    $base = "https://github.com/$Repo/releases/download/$Tag"
    $lines = New-Object System.Collections.ArrayList
    $null = $lines.Add('## Download')
    $null = $lines.Add('')
    $null = $lines.Add('**Ready to run - the filters are already inside.** Unzip, run `aura-engine.exe`, drop a track on it. The app opens on the filter that came with it.')
    $null = $lines.Add('')
    $null = $lines.Add('| Bundle | Filter | Output rates | Size | |')
    $null = $lines.Add('|---|---|---|---|---|')
    foreach ($r in $Results) {
        $null = $lines.Add(('| **{0}** | {1} taps | {2} | {3} | [Download]({4}/{5}) |' -f `
            $r.Label, $r.Tier, $r.Blurb, (Format-Size $r.Size), $base, $r.Zip))
    }
    $null = $lines.Add('')
    $null = $lines.Add('Unzip more than one into the same folder and the app offers every tap count you have.')
    $null = $lines.Add('')
    $null = $lines.Add('Already have the filters, or want to build from source? The plain app zip is below, and the individual filter packs are on the [1.0.0 release](https://github.com/' + $Repo + '/releases/tag/v1.0.0).')
    $null = $lines.Add('')
    $null = $lines.Add('### Checksums (SHA-256)')
    $null = $lines.Add('')
    $null = $lines.Add('```')
    foreach ($r in $Results) { $null = $lines.Add("$($r.Hash)  $($r.Zip)") }
    $null = $lines.Add('```')
    return ($lines -join "`r`n")
}

function Get-TierFiles([string]$tier) {
    $spec = $TierSpec[$tier]
    $files = New-Object System.Collections.ArrayList
    foreach ($rate in $spec.Rates) {
        foreach ($phase in @('linear_phase', 'minimum_phase')) {
            $name = "fir_${tier}_${rate}_${phase}.npy"
            $null = $files.Add([pscustomobject]@{
                Name = $name
                Path = Join-Path $FilterDir $name
            })
        }
    }
    return $files
}

# ── Plan ──────────────────────────────────────────────────────────────────
Write-Host ''
Write-Host "AuraEngine bundle builder - $Tag" -ForegroundColor Cyan
Write-Host "  filters : $FilterDir"
Write-Host "  output  : $OutDir"
Write-Host ''

$plan = New-Object System.Collections.ArrayList
$missingAny = $false
foreach ($tier in $Tiers) {
    $files = Get-TierFiles $tier
    $missing = @($files | Where-Object { -not (Test-Path -LiteralPath $_.Path) })
    $bytes = 0
    foreach ($f in $files) {
        if (Test-Path -LiteralPath $f.Path) { $bytes += (Get-Item -LiteralPath $f.Path).Length }
    }
    $spec = $TierSpec[$tier]
    $null = $plan.Add([pscustomobject]@{
        Tier    = $tier
        Label   = $spec.Name
        Blurb   = $spec.Blurb
        Files   = $files
        Missing = $missing
        Bytes   = $bytes
        Zip     = "aura-engine-$Tag-bundle-$tier-windows-x64.zip"
    })

    $note = ''
    if ($missing.Count -gt 0) { $note = "  <-- $($missing.Count) FILE(S) MISSING"; $missingAny = $true }
    Write-Host ("  {0,-4} {1,-10} {2,-32} {3,3} files  {4,10}{5}" -f `
        $tier, $spec.Name, $spec.Blurb, $files.Count, (Format-Size $bytes), $note)
}
Write-Host ''

if ($missingAny) {
    Write-Host 'Missing blobs:' -ForegroundColor Red
    foreach ($p in $plan) {
        foreach ($m in $p.Missing) { Write-Host "  $($m.Name)" -ForegroundColor Red }
    }
    Write-Host ''
    Write-Host 'Generate them first:  python fir-optimizer\optimize.py --all-ratios' -ForegroundColor Yellow
    throw 'Refusing to build an incomplete bundle.'
}

$totalBytes = ($plan | Measure-Object -Property Bytes -Sum).Sum
Write-Host ("  total payload: {0}" -f (Format-Size $totalBytes))

if ($ListOnly) {
    Write-Host ''
    Write-Host 'Nothing written (-ListOnly).' -ForegroundColor Yellow
    return
}

# Zip entries are stored, not deflated (see below), so the output is very close
# to the payload size. Refuse early rather than half-way through a 1 GB write.
# A UNC target reports no free space; that is a reason to skip the check, not
# to fail it.
$qualifier = (Split-Path -Qualifier $OutDir -ErrorAction SilentlyContinue)
$free = $null
if ($qualifier) {
    $drive = Get-PSDrive -Name $qualifier.TrimEnd(':') -ErrorAction SilentlyContinue
    if ($drive) { $free = $drive.Free }
}
if ($null -ne $free -and $free -lt ($totalBytes * 1.1)) {
    throw ("Not enough free space on ${qualifier} - need about {0}, have {1}." -f `
        (Format-Size ($totalBytes * 1.1)), (Format-Size $free))
}

# ── The executable ────────────────────────────────────────────────────────
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("aura-bundles-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$null = New-Item -ItemType Directory -Force -Path $work

try {
    if (-not $AppZip) {
        $AppZip = Join-Path $work "aura-engine-$Tag-windows-x64.zip"
        Write-Host ''
        Write-Host "Downloading the released binary ($Repo $Tag)..." -ForegroundColor Cyan
        # Deliberately the published artifact: it is the one built with
        # target-cpu=x86-64-v3 and the one users already have.
        # Exactly the plain app zip. A looser glob would also match the
        # bundles this script uploads, so a second run against the same tag
        # would build bundles out of a previous bundle.
        $appAsset = "aura-engine-$Tag-windows-x64.zip"
        & gh release download $Tag --repo $Repo --pattern $appAsset --dir $work
        if ($LASTEXITCODE -ne 0) { throw "gh release download failed: $Tag has no $appAsset." }
        $found = Get-ChildItem -Path $work -Filter $appAsset | Select-Object -First 1
        if (-not $found) { throw "No portable zip attached to $Tag." }
        $AppZip = $found.FullName
    }

    Write-Host "  app zip: $AppZip"
    $unpacked = Join-Path $work 'app'
    $null = New-Item -ItemType Directory -Force -Path $unpacked
    Expand-Archive -LiteralPath $AppZip -DestinationPath $unpacked -Force

    $exe = Get-ChildItem -Path $unpacked -Filter 'aura-engine.exe' -Recurse | Select-Object -First 1
    if (-not $exe) { throw "aura-engine.exe not found inside $AppZip." }
    Write-Host ("  exe    : {0} ({1})" -f $exe.Name, (Format-Size $exe.Length))

    # The published zip already carries the licence that release went out
    # under, so take it from there in preference to the working tree — which,
    # in a private checkout, may not have one at all. Shipping a bundle with
    # no licence file is not an option: it is the only thing in the package
    # that says what a stranger may do with it.
    $license = $null
    $inZip = Get-ChildItem -Path $unpacked -Filter 'LICENSE*' -Recurse -File | Select-Object -First 1
    if ($inZip) { $license = $inZip.FullName }
    elseif (Test-Path -LiteralPath (Join-Path $RepoRoot 'LICENSE')) {
        $license = Join-Path $RepoRoot 'LICENSE'
    }
    else {
        throw ('No LICENSE found - neither inside ' + [System.IO.Path]::GetFileName($AppZip) +
               ' nor at ' + (Join-Path $RepoRoot 'LICENSE') + '.')
    }
    Write-Host ("  licence: {0}" -f (Split-Path -Leaf $license))

    $null = New-Item -ItemType Directory -Force -Path $OutDir
    $results = New-Object System.Collections.ArrayList

    foreach ($p in $plan) {
        $zipPath = Join-Path $OutDir $p.Zip
        if ((Test-Path -LiteralPath $zipPath) -and -not $Force) {
            throw "$($p.Zip) already exists. Pass -Force to overwrite."
        }
        if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }

        $root = [System.IO.Path]::GetFileNameWithoutExtension($p.Zip)
        Write-Host ''
        Write-Host "Building $($p.Zip)" -ForegroundColor Green

        # Stored, not deflated. These blobs are float64 coefficients with no
        # redundancy to squeeze: the existing packs compress by 0.002%, and
        # deflating a gigabyte of them costs minutes of CPU for nothing. It
        # also keeps extraction fast on the user's side, which is the part
        # that actually matters here.
        $store = [System.IO.Compression.CompressionLevel]::NoCompression
        $zip = [System.IO.Compression.ZipFile]::Open($zipPath, 'Create')
        try {
            $null = [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $zip, $exe.FullName, "$root/aura-engine.exe",
                [System.IO.Compression.CompressionLevel]::Optimal)

            if ($license) {
                $null = [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zip, $license, "$root/LICENSE.txt",
                    [System.IO.Compression.CompressionLevel]::Optimal)
            }

            $readme = New-BundleReadme -Tier $p.Tier -Label $p.Label -Blurb $p.Blurb -Tag $Tag
            $entry = $zip.CreateEntry("$root/README.txt", [System.IO.Compression.CompressionLevel]::Optimal)
            $writer = New-Object System.IO.StreamWriter($entry.Open(), (New-Object System.Text.UTF8Encoding($false)))
            try { $writer.Write(($readme -join "`r`n")) } finally { $writer.Dispose() }

            $i = 0
            foreach ($f in $p.Files) {
                $i++
                Write-Progress -Activity $p.Zip -Status $f.Name -PercentComplete (100 * $i / $p.Files.Count)
                $null = [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zip, $f.Path, "$root/fir-optimizer/output/$($f.Name)", $store)
            }
            Write-Progress -Activity $p.Zip -Completed
        }
        finally { $zip.Dispose() }

        $size = (Get-Item -LiteralPath $zipPath).Length
        $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLower()
        Set-Content -Path "$zipPath.sha256" -Value "$hash  $($p.Zip)" -Encoding ascii
        Write-Host ("  {0}  {1}" -f (Format-Size $size), $hash)

        $null = $results.Add([pscustomobject]@{
            Tier = $p.Tier; Label = $p.Label; Blurb = $p.Blurb
            Zip = $p.Zip; Size = $size; Hash = $hash
        })
    }

    # ── Checksums and release notes ───────────────────────────────────────
    $sums = Join-Path $OutDir 'SHA256SUMS-bundles.txt'
    Set-Content -Path $sums -Encoding ascii -Value ($results | ForEach-Object { "$($_.Hash)  $($_.Zip)" })

    # UTF-8 without a BOM: Windows PowerShell's -Encoding utf8 writes one, and
    # it survives into the release body as a stray character above the heading.
    $notes = Join-Path $OutDir "RELEASE-NOTES-$Tag.md"
    [System.IO.File]::WriteAllText(
        $notes,
        (New-ReleaseNotes -Results $results -Tag $Tag -Repo $Repo),
        (New-Object System.Text.UTF8Encoding($false)))

    Write-Host ''
    Write-Host 'Built:' -ForegroundColor Cyan
    foreach ($r in $results) { Write-Host ("  {0,-52} {1,10}" -f $r.Zip, (Format-Size $r.Size)) }
    Write-Host "  $sums"
    Write-Host "  $notes"

    if ($Upload) {
        Write-Host ''
        Write-Host "Uploading to $Repo $Tag..." -ForegroundColor Cyan
        $assets = @()
        foreach ($r in $results) {
            $assets += (Join-Path $OutDir $r.Zip)
            $assets += (Join-Path $OutDir "$($r.Zip).sha256")
        }
        $assets += $sums
        & gh release upload $Tag @assets --repo $Repo --clobber
        if ($LASTEXITCODE -ne 0) { throw 'gh release upload failed.' }
        Write-Host 'Uploaded.' -ForegroundColor Green
    }
    else {
        Write-Host ''
        Write-Host 'Not uploaded. Re-run with -Upload, or attach them by hand.' -ForegroundColor Yellow
    }
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
