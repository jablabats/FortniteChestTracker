param(
    [string]$Dotnet = 'dotnet',
    [string]$UpstreamRoot
)

$ErrorActionPreference = 'Stop'
$revision = '1f387987737cfd606611d5b8d294d5db04af9eab'
if (!$UpstreamRoot) {
    $UpstreamRoot = Join-Path $PSScriptRoot 'native/vendor'
    if (!(Test-Path -LiteralPath (Join-Path $UpstreamRoot '.git'))) {
        git init --quiet $UpstreamRoot
        if ($LASTEXITCODE -ne 0) { throw 'Could not create the replay parser checkout.' }
        git -C $UpstreamRoot fetch --quiet --depth 1 https://github.com/LED-N/Fortnite-ChestCounter.git $revision
        if ($LASTEXITCODE -ne 0) { throw 'Could not fetch the replay parser.' }
        git -C $UpstreamRoot checkout --quiet --detach FETCH_HEAD
        if ($LASTEXITCODE -ne 0) { throw 'Could not check out the replay parser.' }
    }
}
$UpstreamRoot = (Resolve-Path -LiteralPath $UpstreamRoot).Path
$actual = git -C $UpstreamRoot rev-parse HEAD
if ($LASTEXITCODE -ne 0 -or $actual -ne $revision) { throw 'The replay parser checkout is at an unexpected revision.' }
$project = Join-Path $PSScriptRoot 'native/ReplayDecoder.csproj'
$output = Join-Path $PSScriptRoot 'bin'
& $Dotnet publish $project -c Release -r win-x64 --self-contained true -p:PublishSingleFile=true "-p:UpstreamRoot=$UpstreamRoot" -o $output
if ($LASTEXITCODE -ne 0) { throw 'The replay decoder build failed.' }
Copy-Item -LiteralPath (Join-Path $UpstreamRoot 'LICENSE') -Destination (Join-Path $output 'LICENSE-LED-N.txt')
Copy-Item -LiteralPath (Join-Path $UpstreamRoot 'lib/LICENSE-FortniteReplayDecompressor') -Destination $output
