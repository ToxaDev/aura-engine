<#
.SYNOPSIS
    Builds the ready-to-run bundles: the app with its FIR filters already in it.

.DESCRIPTION
    A first-time user should be able to download one file, unzip it, drop a
    track on the window and hear the result. Without this, they download a 6 MB
    app, hit "missing filter", work out which of five packs they need, download
    a gigabyte of it and extract it into the right subfolder. This script builds
    the one-file version of that.

    Four bundles, one per output rate, each under 2 GiB so a single release
    asset can hold it:

      lite  5k and 1M taps at every FS multiplier (FS2-FS32). The whole
            rate ladder at a small size.
      x8    5k, 1M, 5M, 10M, 30M at FS8 + 5k, 1M, 5M, 10M at FS4.
      x16   5k-30M at FS16 + 5k-10M at FS8.
      x32   5k-30M at FS32 + 5k-10M at FS16.

    What goes in is read from the signed filters manifest (the one published
    with the `filters` release): a cell is every file of one (taps, rate,
    phase), so branch files (.b0.*) come with their parent. Every file's size
    and SHA-256 are checked against the manifest before it is zipped.
    Longer filters (100M and up) are only in-app downloads.

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
    # Build the four bundles into .\dist
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
    [ValidateSet('lite', 'x8', 'x16', 'x32')]
    [string[]]$Tiers = @('lite', 'x8', 'x16', 'x32'),

    # The signed filters-manifest.json (names, sizes and SHA-256 of every
    # file). Default: downloaded from the `filters` release of -Repo.
    [string]$Manifest,

    # Folder holding the generated filter files (names as in the manifest).
    [string]$FilterDir,

    # Optional inventory (JSON: file name -> full path) for files that are not
    # all in one folder. Wins over -FilterDir for the names it lists.
    [string]$Inventory,

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
    [switch]$Force,

    # The public checkout, so the run can report whether its download links
    # still point at the previous tag. Reported only, never edited. By default
    # the AuraEngine-Public folder beside this repository (run from the public
    # checkout itself, that is the checkout).
    [string]$PublicRepo = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression.FileSystem

$RepoRoot = Split-Path -Parent $PSScriptRoot
if (-not $FilterDir) { $FilterDir = Join-Path $RepoRoot 'fir-optimizer\output' }
if (-not $OutDir)    { $OutDir    = Join-Path $RepoRoot 'dist' }
if (-not $PublicRepo) { $PublicRepo = Join-Path (Split-Path -Parent $RepoRoot) 'AuraEngine-Public' }

# Absolute from here on. A relative -OutDir would otherwise reach the
# free-space check with no drive qualifier and take it down.
function Resolve-AbsolutePath([string]$path) {
    if ([System.IO.Path]::IsPathRooted($path)) { return [System.IO.Path]::GetFullPath($path) }
    return [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path $path))
}
$FilterDir = Resolve-AbsolutePath $FilterDir
# The manifest comes from the public `filters` release unless -Manifest names a
# local copy (its .sig is looked for beside it). Either way it is checked
# against its signature when tools\sign_update.py is at hand.
$ManifestTmp = $null
if ($Manifest) {
    $Manifest = Resolve-AbsolutePath $Manifest
    if (-not (Test-Path -LiteralPath $Manifest)) { throw "Manifest not found: $Manifest." }
}
else {
    $ManifestTmp = Join-Path ([System.IO.Path]::GetTempPath()) ("aura-manifest-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    $null = New-Item -ItemType Directory -Force -Path $ManifestTmp
    & gh release download filters --repo $Repo --pattern 'filters-manifest.json*' --dir $ManifestTmp
    if ($LASTEXITCODE -ne 0) { throw "gh release download failed: no filters manifest in $Repo." }
    $Manifest = Join-Path $ManifestTmp 'filters-manifest.json'
}
$ManifestSig = "$Manifest.sig"
$signer = Join-Path $PSScriptRoot 'sign_update.py'
if ((Test-Path -LiteralPath $signer) -and (Test-Path -LiteralPath $ManifestSig)) {
    & python -I $signer verify-filters $Manifest $ManifestSig
    if ($LASTEXITCODE -ne 0) { throw 'The filters manifest does not match its signature.' }
}
else {
    Write-Warning 'Manifest signature not checked (tools\sign_update.py or the .sig is missing).'
}
$ManifestFiles = @((Get-Content -LiteralPath $Manifest -Raw -Encoding UTF8 | ConvertFrom-Json).files)
if ($ManifestTmp) { Remove-Item -LiteralPath $ManifestTmp -Recurse -Force -ErrorAction SilentlyContinue }
$InvMap = @{}
if ($Inventory) {
    $invObj = Get-Content -LiteralPath (Resolve-AbsolutePath $Inventory) -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($pr in $invObj.PSObject.Properties) { $InvMap[$pr.Name] = $pr.Value }
}
$OutDir    = Resolve-AbsolutePath $OutDir

# ── What goes in each tier ────────────────────────────────────────────────
# Cells are (taps, FS multiplier). The rate is 44100*N and 48000*N; both phases.
$AllTaps = @('5k', '1M', '5M', '10M', '30M')
$LowTaps = @('5k', '1M', '5M', '10M')
function Get-Cells([string[]]$taps, [int[]]$mults) {
    foreach ($t in $taps) { foreach ($m in $mults) { , @($t, $m) } }
}
$TierSpec = [ordered]@{
    'lite' = @{ Name = 'Lite'; Blurb = '5k and 1M taps, every FS multiplier (FS2-FS32)'
                Cells = @(Get-Cells @('5k', '1M') @(2, 4, 8, 16, 32)) }
    'x8'   = @{ Name = 'x8';   Blurb = 'FS8 (352.8 / 384 kHz): 5k-30M taps; FS4: 5k-10M'
                Cells = @((Get-Cells $AllTaps @(8)) + (Get-Cells $LowTaps @(4))) }
    'x16'  = @{ Name = 'x16';  Blurb = 'FS16 (705.6 / 768 kHz): 5k-30M taps; FS8: 5k-10M'
                Cells = @((Get-Cells $AllTaps @(16)) + (Get-Cells $LowTaps @(8))) }
    'x32'  = @{ Name = 'x32';  Blurb = 'FS32 (1411.2 / 1536 kHz): 5k-30M taps; FS16: 5k-10M'
                Cells = @((Get-Cells $AllTaps @(32)) + (Get-Cells $LowTaps @(16))) }
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
    $rates = $Blurb + ', both rate families'
    # The output name's middle dot and arrow, by code point: this file stays ASCII.
    $dot = [char]0x00B7
    $arrow = [char]0x2192
    return @(
        "AuraEngine $Tag - $Label bundle",
        'https://github.com/ToxaDev/aura-engine',
        'Docs: https://aura-engine.app/',
        '',
        'THE FILTERS ARE ALREADY HERE',
        "  This is the complete package: the app plus its FIR filters,",
        '  in fir-optimizer\output\. There is no second download and no setup',
        '  step. Run aura-engine.exe and drop audio files on the window: they',
        '  join the list. The play button on a row plays the file through the',
        '  chain it would be converted with; C converts that row, and Convert',
        '  all every row not converted yet. F / R at the head of the list',
        '  switches between your files and internet radio.',
        '',
        "  Included: $rates,",
        '  in both linear and minimum phase - the second is what Hybrid-Phase',
        '  renders its transient branch from.',
        '',
        '  Slider positions with no filter behind them are struck through; the',
        '  app downloads any of them on request, and every filter file is also',
        '  on the Releases page, tag "filters".',
        '',
        'ADDING MORE',
        '  Unzip another bundle into this same folder and the app offers both tap',
        '  counts - the filter files merge, and only one copy of the app is used.',
        '  Longer filters (100M taps and up) are only in-app downloads. Or generate any',
        '  combination yourself with fir-optimizer/optimize.py --all-ratios.',
        '  AURA_FILTER_DIR points the app at filters kept somewhere else.',
        '',
        'REQUIREMENTS',
        '  * Windows 10/11 x64, CPU with AVX2 (any Intel/AMD from ~2013).',
        '  * WebView2 runtime - preinstalled on Windows 11; on Windows 10 the app',
        '    offers the download if it is missing. Nothing else: decoding (HE-AAC',
        '    and Opus included) and FLAC encoding are built in, and no external',
        '    tool is started during a conversion.',
        '  * To play: the output device is opened in WASAPI exclusive mode at the',
        '    output rate, so nothing else plays through it meanwhile, and it must',
        '    accept that rate (FS8 is 352.8 / 384 kHz, FS16 705.6 / 768 kHz, FS32 1411.2 / 1536 kHz; otherwise pick a lower FS',
        '    multiplier).',
        '  * For internet radio: a connection a stream plays on.',
        '  * Optional: a Vulkan GPU for the accelerated double-single path. The',
        '    app falls back to the CPU reference path on its own. Playback moves',
        '    to a discrete card only when the CPU cannot keep up.',
        '',
        'WHAT YOU GET OUT',
        '  A 24-bit FLAC next to each source file, named like',
        "    Track [AE $dot 44.1k${arrow}352.8k $dot Kaiser 1M $dot f64 $dot SUB15${dot}AA${dot}PFR${dot}TFS].flac",
        '  - the stages that ran, in the order they ran. A VERIFIED badge means',
        '  the written file was re-decoded and matched the internal f64 buffer.',
        '  The console window with the audit log starts minimized; session logs',
        '  are kept in %LOCALAPPDATA%\AuraEngine\logs.',
        '',
        'UPDATES',
        '  The app looks for a newer release at start and offers it. The',
        '  download is checked against its signature before anything is',
        '  written, and nothing is updated while a conversion runs.',
        '',
        'LICENSE',
        '  PolyForm Noncommercial 1.0.0 - free for noncommercial use.',
        '  Commercial licensing: auraengine.dev@gmail.com',
        '  THIRD-PARTY-NOTICES.txt lists the libraries the program is built',
        '  with and their licences.',
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
    $null = $lines.Add('**Ready to run - the filters are already inside.** Unzip, run `aura-engine.exe`, drop a track on it.')
    $null = $lines.Add('')
    $null = $lines.Add('| Bundle | Contents | Size | |')
    $null = $lines.Add('|---|---|---|---|')
    foreach ($r in $Results) {
        $null = $lines.Add(('| **{0}** | {1} | {2} | [Download]({3}/{4}) |' -f `
            $r.Label, $r.Blurb, (Format-Size $r.Size), $base, $r.Zip))
    }
    $null = $lines.Add('')
    $null = $lines.Add('Unzip more than one into the same folder and the app offers everything you have. Longer filters (100M taps and up) download from inside the app.')
    $null = $lines.Add('')
    $null = $lines.Add('Already have the filters, or want to build from source? The plain app zip is below, and every filter file is on the [filters release](https://github.com/' + $Repo + '/releases/tag/filters).')
    $null = $lines.Add('')
    $null = $lines.Add('### Checksums (SHA-256)')
    $null = $lines.Add('')
    $null = $lines.Add('```')
    foreach ($r in $Results) { $null = $lines.Add("$($r.Hash)  $($r.Zip)") }
    $null = $lines.Add('```')
    return ($lines -join "`r`n")
}

function Get-TierFiles([string]$tier) {
    $files = New-Object System.Collections.ArrayList
    $seen = @{}
    foreach ($c in $TierSpec[$tier].Cells) {
        $taps = $c[0]
        foreach ($rate in @((44100 * $c[1]), (48000 * $c[1]))) {
            foreach ($phase in @('linear', 'minimum')) {
                $cell = @($ManifestFiles | Where-Object { $_.taps -eq $taps -and $_.rate -eq $rate -and $_.phase -eq $phase })
                if ($cell.Count -eq 0) { throw "Manifest has no files for cell $taps / $rate / $phase." }
                foreach ($m in $cell) {
                    if ($seen.ContainsKey($m.name)) { continue }
                    $seen[$m.name] = $true
                    $path = Join-Path $FilterDir $m.name
                    if ($InvMap.ContainsKey($m.name)) { $path = $InvMap[$m.name] }
                    $null = $files.Add([pscustomobject]@{
                        Name = $m.name; Path = $path; Size = [long]$m.size; Sha = $m.sha256
                    })
                }
            }
        }
    }
    return $files
}

# Size and SHA-256 against the manifest, once per file per run (the bundles
# overlap). A mismatch stops the build: the app would reject that file anyway.
$VerifiedNames = @{}
function Test-TierFiles($files) {
    foreach ($f in $files) {
        if ($VerifiedNames.ContainsKey($f.Name)) { continue }
        $len = (Get-Item -LiteralPath $f.Path).Length
        if ($len -ne $f.Size) { throw "$($f.Name): size $len, manifest says $($f.Size)." }
        $h = (Get-FileHash -LiteralPath $f.Path -Algorithm SHA256).Hash.ToLower()
        if ($h -ne $f.Sha) { throw "$($f.Name): SHA-256 differs from the manifest." }
        $VerifiedNames[$f.Name] = $true
    }
}

# ── Plan ──────────────────────────────────────────────────────────────────
Write-Host ''
Write-Host "AuraEngine bundle builder - $Tag" -ForegroundColor Cyan
Write-Host "  manifest: $Manifest"
Write-Host "  filters : $FilterDir"
Write-Host "  output  : $OutDir"
Write-Host ''

$plan = New-Object System.Collections.ArrayList
$missingAny = $false
foreach ($tier in $Tiers) {
    $files = Get-TierFiles $tier
    $missing = @($files | Where-Object { -not (Test-Path -LiteralPath $_.Path) })
    $bytes = ($files | Measure-Object -Property Size -Sum).Sum
    if ($bytes -ge 2147483648) { throw "Bundle $tier is $bytes bytes, over the 2 GiB asset limit." }
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
    Write-Host ("  {0,-4} {1,-6} {2,-58} {3,3} files  {4,10}{5}" -f `
        $tier, $spec.Name, $spec.Blurb, $files.Count, (Format-Size $bytes), $note)
}
Write-Host ''

if ($missingAny) {
    Write-Host 'Missing blobs:' -ForegroundColor Red
    foreach ($p in $plan) {
        foreach ($m in $p.Missing) { Write-Host "  $($m.Name)" -ForegroundColor Red }
    }
    Write-Host ''
    Write-Host 'Generate them first, or point -FilterDir / -Inventory at where they are.' -ForegroundColor Yellow
    throw 'Refusing to build an incomplete bundle.'
}

if (-not $ListOnly) {
    foreach ($p in $plan) { Write-Host "Verifying $($p.Tier) against the manifest..."; Test-TierFiles $p.Files }
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
    # Beside the exe only: the ASIO driver's folder has a licence of its own.
    $inZip = Get-ChildItem -Path $exe.DirectoryName -Filter 'LICENSE*' -File | Select-Object -First 1
    if ($inZip) { $license = $inZip.FullName }
    elseif (Test-Path -LiteralPath (Join-Path $RepoRoot 'LICENSE')) {
        $license = Join-Path $RepoRoot 'LICENSE'
    }
    else {
        throw ('No LICENSE found - neither inside ' + [System.IO.Path]::GetFileName($AppZip) +
               ' nor at ' + (Join-Path $RepoRoot 'LICENSE') + '.')
    }
    Write-Host ("  licence: {0}" -f (Split-Path -Leaf $license))

    # The third-party notices go beside the licence, from the published zip
    # first, then the working tree. A release without them yet still gets
    # its bundles, with a warning.
    $notices = $null
    $noticesInZip = Get-ChildItem -Path $unpacked -Filter 'THIRD-PARTY-NOTICES*' -Recurse -File | Select-Object -First 1
    if ($noticesInZip) { $notices = $noticesInZip.FullName }
    else {
        foreach ($n in @('THIRD-PARTY-NOTICES.txt', 'THIRD-PARTY-NOTICES')) {
            if (Test-Path -LiteralPath (Join-Path $RepoRoot $n)) { $notices = Join-Path $RepoRoot $n; break }
        }
    }
    if ($notices) { Write-Host ("  notices: {0}" -f (Split-Path -Leaf $notices)) }
    else {
        Write-Warning ('No THIRD-PARTY-NOTICES found - neither inside ' + [System.IO.Path]::GetFileName($AppZip) +
                       ' nor at ' + $RepoRoot + '; the bundles go out without it.')
    }

    # The ASIO driver (from 1.6.0): the app zip's asio\ folder goes into each
    # bundle as it is, beside the exe where the app looks for it.
    $asioFiles = @()
    $asioDir = Join-Path $exe.DirectoryName 'asio'
    if (Test-Path -LiteralPath $asioDir) {
        $asioFiles = @(Get-ChildItem -LiteralPath $asioDir -File)
        Write-Host ("  asio   : {0}" -f (($asioFiles | ForEach-Object Name) -join ', '))
    }
    else {
        Write-Warning ('No asio\ folder inside ' + [System.IO.Path]::GetFileName($AppZip) +
                       '; the bundles go out without the ASIO driver (the app fetches it when installed).')
    }

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
            if ($notices) {
                $null = [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zip, $notices, "$root/THIRD-PARTY-NOTICES.txt",
                    [System.IO.Compression.CompressionLevel]::Optimal)
            }
            foreach ($a in $asioFiles) {
                $null = [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $zip, $a.FullName, "$root/asio/$($a.Name)",
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

    # ── The step that keeps getting missed ────────────────────────────────
    # The download links in README.md and docs/index.html carry the tag twice
    # each -- once in the release path, once in the asset filename -- and
    # nothing else in this pipeline touches them. At v1.2.5 the README was
    # still on v1.2.4 and the Pages site was still on v1.2.0: five releases
    # stale, so everyone arriving from the website downloaded a two-month-old
    # build. This only reports; it does not edit. `aura-filters-*` links are
    # ignored on purpose -- those packs were published once, on v1.0.0, and
    # do not move.
    $linkFiles = @(
        (Join-Path $PublicRepo 'README.md'),
        (Join-Path $PublicRepo 'docs\index.html')
    )
    $stale = @()
    foreach ($lf in $linkFiles) {
        if (-not (Test-Path -LiteralPath $lf)) { continue }
        $n = 0
        foreach ($line in (Get-Content -LiteralPath $lf)) {
            if ($line -match 'releases/download/(v[\d.]+)/aura-engine-' -and $Matches[1] -ne $Tag) { $n++ }
        }
        if ($n -gt 0) { $stale += [pscustomobject]@{ File = $lf; Count = $n } }
    }

    Write-Host ''
    if ($stale.Count -gt 0) {
        Write-Host 'DOWNLOAD LINKS ARE STALE - the release is not finished:' -ForegroundColor Red
        foreach ($x in $stale) {
            Write-Host ("  {0}  ({1} link(s) not on {2})" -f $x.File, $x.Count, $Tag) -ForegroundColor Red
        }
        Write-Host "  Point every aura-engine-* link at $Tag, then commit and push the public repo." -ForegroundColor Red
        Write-Host '  Leave every aura-filters-* link on v1.0.0.' -ForegroundColor Red
    }
    elseif ($linkFiles | Where-Object { Test-Path -LiteralPath $_ }) {
        Write-Host "Download links in README.md and docs/index.html are on $Tag." -ForegroundColor Green
    }
    else {
        Write-Host "Public checkout not found at $PublicRepo - check the download links by hand." -ForegroundColor Yellow
    }
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
