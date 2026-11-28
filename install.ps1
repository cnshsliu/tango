# Install tango from a GitHub release on Windows: pick the package for this CPU, check its
# SHA-256 against the release's SHA256SUMS, unpack it under the tango home, and put `tango`
# (and `tang`) commands into %LOCALAPPDATA%\tango\bin, adding that directory to the user PATH
# once. Node 20 or newer must be on the PATH: the serve is Node, and the launcher runs it from
# there.
#
#   irm https://raw.githubusercontent.com/cnshsliu/tango/main/install.ps1 | iex
#   $env:TANGO_VERSION = '0.2.0'   before the line above: a given release (default: the latest)
#   $env:TANGO_INSTALL_DIR = '…'   another directory for the commands
#   $env:TANGO_HOME = '…'          another tango home (where releases\ goes), as for tango itself
#   $env:TANGO_RELEASES_URL = '…'  download base (default: this repo's GitHub releases)
#   $env:TANGO_NO_SHA = '1'        skip the checksum check (not recommended)
#   $env:GITHUB_TOKEN = '…'        for the latest-release lookup (optional)
param()

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$repo = 'cnshsliu/tango'

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
switch -Regex ($arch) {
  '^Arm64$' { $arch = 'arm64' }
  '^(X64|AMD64)$' { $arch = 'x64' }
  default { Write-Error "tango: unsupported CPU architecture: $arch"; exit 1 }
}
$platform = "windows-$arch"

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
  Write-Error 'tango: node 20 or newer is needed — https://nodejs.org'
  exit 1
}
$nodeMajor = [int](node -p 'process.versions.node.split(".")[0]')
if ($nodeMajor -lt 20) { Write-Error "tango: node 20 or newer is needed, found $(node --version)"; exit 1 }

$base = if ($env:TANGO_RELEASES_URL) { $env:TANGO_RELEASES_URL } else { "https://github.com/$repo/releases/download" }
$version = $env:TANGO_VERSION
if (-not $version) {
  $headers = @{ 'User-Agent' = 'tango-install'; 'X-GitHub-Api-Version' = '2022-11-28' }
  if ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $($env:GITHUB_TOKEN)" }
  $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers $headers
  $version = $latest.tag_name
}
if (-not $version) { Write-Error 'tango: no release found — set TANGO_VERSION'; exit 1 }
$version = $version.TrimStart('v')
$tag = "v$version"

$asset = "tango-$version-$platform.zip"
$url = "$base/$tag/$asset"
$tangoHome = if ($env:TANGO_HOME) { $env:TANGO_HOME } else { Join-Path $env:USERPROFILE '.tango' }
$dest = Join-Path $tangoHome "releases\tango-$version-$platform"
$binDir = if ($env:TANGO_INSTALL_DIR) { $env:TANGO_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'tango\bin' }

$tmp = Join-Path $env:TEMP ("tango-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Write-Host "tango: downloading $url"
  try {
    Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmp $asset) -UseBasicParsing
  } catch {
    Write-Error "tango: no package $asset in release $tag ($url)"
    exit 1
  }
  if ($env:TANGO_NO_SHA -ne '1') {
    try {
      Invoke-WebRequest -Uri "$base/$tag/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') -UseBasicParsing
    } catch {
      Write-Error "tango: SHA256SUMS missing for $tag"
      exit 1
    }
    $line = Get-Content (Join-Path $tmp 'SHA256SUMS') |
      Where-Object { $_ -match "\s\*?$([regex]::Escape($asset))$" } | Select-Object -First 1
    if (-not $line) { Write-Error "tango: $asset is not in SHA256SUMS"; exit 1 }
    $expected = (($line -split '\s+')[0]).ToLower()
    $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
      Write-Error "tango: checksum mismatch for $asset (expected $expected, got $actual)"
      exit 1
    }
  }
  if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
  Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $dest -Force
  # the zip holds one top folder: move its contents up, so the package root is $dest
  $inner = Join-Path $dest "tango-$version-$platform"
  if (Test-Path $inner) {
    Get-ChildItem $inner -Force | Move-Item -Destination $dest -Force
    Remove-Item $inner -Force
  }
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$exe = Join-Path $dest 'tui\target\release\tango-tui.exe'
if (-not (Test-Path $exe)) { Write-Error "tango: the package has no launcher at $exe"; exit 1 }

# a .cmd shim per command name, as npm does it: the launcher finds its files from its own path,
# so it must stay where the package put it
New-Item -ItemType Directory -Path $binDir -Force | Out-Null
foreach ($name in 'tango', 'tang') {
  "@`"$exe`" %*" | Set-Content -Path (Join-Path $binDir "$name.cmd") -Encoding ASCII
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $binDir)) {
  [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$binDir"), 'User')
  Write-Host "tango: added $binDir to your user PATH (open a new terminal for it)"
}
Write-Host "tango $version installed: $binDir\tango.cmd"
