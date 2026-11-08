# Arr installer for Windows: downloads a prebuilt Arr, then sets it up next to your Plex
# server in your web browser. Open PowerShell (Start menu > type "PowerShell") and paste:
#
#   irm https://raw.githubusercontent.com/newtonharry/Arr/main/install.ps1 | iex
#
# Nothing here needs administrator rights. `irm | iex` can't pass options, so they come
# from environment variables, for example: $env:ARR_VERSION = "v0.4.0"; irm ... | iex
#
#   ARR_VERSION       a release tag such as v0.4.0 (default: the latest release)
#   ARR_AUTO_UPDATE   1 to install new versions automatically, 0 to turn that off
#   ARR_PORT          Arr's port on this computer (default 8787)
#   ARR_STATE_DIR     Arr's folder: config, secrets, database (default %LOCALAPPDATA%\Arr)
#   ARR_BIN_DIR       where arr.exe goes (default %LOCALAPPDATA%\Programs\Arr)
#   ARR_NO_SETUP      1 to install the program only
#   ARR_YES           1 to never ask (FFmpeg is installed without asking)

function Install-Arr {
	$ErrorActionPreference = 'Stop'
	$ProgressPreference = 'SilentlyContinue' # the progress bar makes downloads very slow
	[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

	$Repo = 'newtonharry/Arr'
	$Target = 'x86_64-pc-windows-msvc'
	$Version = if ($env:ARR_VERSION) { $env:ARR_VERSION } else { 'latest' }
	$DownloadBase = $env:ARR_DOWNLOAD_BASE
	if (-not $DownloadBase) {
		$DownloadBase = if ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download" }
		else { "https://github.com/$Repo/releases/download/$Version" }
	}
	$BinDir = if ($env:ARR_BIN_DIR) { $env:ARR_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\Arr' }
	$StateDir = if ($env:ARR_STATE_DIR) { $env:ARR_STATE_DIR } else { Join-Path $env:LOCALAPPDATA 'Arr' }
	$Port = if ($env:ARR_PORT) { [int]$env:ARR_PORT } else { 8787 }
	$TorrentPort = if ($env:ARR_TORRENT_PORT) { [int]$env:ARR_TORRENT_PORT } else { 50413 }
	$UploadLimit = if ($env:ARR_UPLOAD_LIMIT_KIB) { [int]$env:ARR_UPLOAD_LIMIT_KIB } else { 0 }
	$AutoUpdate = $env:ARR_AUTO_UPDATE
	$Yes = $env:ARR_YES -eq '1'
	$Config = Join-Path $StateDir 'config.toml'
	$Arr = Join-Path $BinDir 'arr.exe'
	$Background = Join-Path $BinDir 'arr-background.exe'
	$Updater = Join-Path $BinDir 'arr-update.ps1'

	function Step($m) { Write-Host ''; Write-Host "==> $m" -ForegroundColor Cyan }
	function Say($m) { Write-Host $m }
	function Interactive { -not $Yes -and [Environment]::UserInteractive -and -not [Console]::IsInputRedirected }
	function Confirm($question, $default) {
		if (-not (Interactive)) { return $default -eq 'y' }
		$hint = if ($default -eq 'y') { '[Y/n]' } else { '[y/N]' }
		$a = Read-Host "$question $hint"
		if (-not $a) { $a = $default }
		return $a -match '^[Yy]'
	}

	if (-not [Environment]::Is64BitOperatingSystem) { throw 'Arr needs 64-bit Windows.' }
	if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
		Say 'This PC has an ARM processor; Arr runs through Windows 11''s x64 emulation.'
	}

	# ------------------------------------------------------------ install the program

	Step "Downloading Arr ($Version)"
	$asset = "arr-$Target.zip"
	$tmp = Join-Path ([IO.Path]::GetTempPath()) ('arr-' + [guid]::NewGuid().ToString('N'))
	New-Item -ItemType Directory $tmp | Out-Null
	try {
		Invoke-WebRequest -UseBasicParsing "$DownloadBase/$asset" -OutFile "$tmp\$asset"
		Invoke-WebRequest -UseBasicParsing "$DownloadBase/$asset.sha256" -OutFile "$tmp\$asset.sha256"
		$want = ((Get-Content "$tmp\$asset.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
		$got = (Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower()
		if ($got -ne $want) { throw "checksum mismatch for $asset; nothing was installed" }
		Expand-Archive "$tmp\$asset" -DestinationPath $tmp -Force
		Stop-Arr $Background
		New-Item -ItemType Directory -Force $BinDir | Out-Null
		foreach ($exe in 'arr.exe', 'arr-background.exe') {
			Replace-File (Join-Path "$tmp\arr-$Target" $exe) (Join-Path $BinDir $exe)
		}
	} finally {
		Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
	}
	Say "Installed $(& $Arr --version) to $Arr"
	Add-ToPath $BinDir

	if ($env:ARR_NO_SETUP -eq '1') { return }

	# ------------------------------------------------------------ ffprobe

	Step 'Checking for ffprobe (Arr uses it to check every download before import)'
	$ffprobe = Find-FFprobe
	if (-not $ffprobe) {
		if (-not (Get-Command winget.exe -ErrorAction SilentlyContinue)) {
			throw 'FFmpeg is missing. Install it (https://www.gyan.dev/ffmpeg/builds/), then run this again.'
		}
		if (-not (Confirm 'ffprobe is missing. Install FFmpeg now with winget?' 'y')) {
			throw 'Arr needs FFmpeg. Install it, then run this again.'
		}
		& winget.exe install --id Gyan.FFmpeg --exact --silent --accept-package-agreements --accept-source-agreements
		$ffprobe = Find-FFprobe
		if (-not $ffprobe) { throw 'FFmpeg was installed but ffprobe.exe was not found; open a new PowerShell window and run this again.' }
	}
	Say "Found $ffprobe"

	# ------------------------------------------------------------ Arr's folder

	New-Item -ItemType Directory -Force $StateDir | Out-Null
	# Only you, Windows itself and administrators may open it: it holds Arr's secrets.
	$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
	& icacls.exe $StateDir /inheritance:r /grant:r "*${sid}:(OI)(CI)F" '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
	if ($LASTEXITCODE -ne 0) { throw "couldn't make $StateDir private" }

	if (Test-Path $Config) {
		Step "Keeping your config in $StateDir"
		$m = Select-String -Path $Config -Pattern '^listen = "[^"]*:(\d+)"$' | Select-Object -First 1
		if ($m) { $Port = [int]$m.Matches[0].Groups[1].Value }
	} else {
		Step 'Setting up Arr in your web browser'
		& $Arr wizard --state-dir $StateDir --port $Port --torrent-port $TorrentPort `
			--upload-limit-kib $UploadLimit --ffprobe $ffprobe --open
		if ($LASTEXITCODE -ne 0) { throw "setup wasn't finished; run this again to start over" }
		# The page asked about automatic updates; the answer is used once, below.
		$choices = Join-Path $StateDir 'setup-choices'
		if (-not $AutoUpdate -and (Test-Path $choices)) {
			$c = Select-String -Path $choices -Pattern '^auto_update=([01])$' | Select-Object -First 1
			if ($c) { $AutoUpdate = $c.Matches[0].Groups[1].Value }
		}
		Remove-Item $choices -Force -ErrorAction SilentlyContinue
	}
	$downloading = (Get-Content $Config -Raw) -match '\[execution\]\r?\nenabled = true'

	# ------------------------------------------------------------ run it

	Step 'Starting Arr'
	# A task that starts Arr when you log in (as Plex on Windows does), without a window.
	$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
	$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
		-ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
		-StartWhenAvailable -MultipleInstances IgnoreNew
	$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
	Register-ScheduledTask -TaskName 'Arr' -Description 'Arr media manager (installed by install.ps1)' -Force `
		-Action (New-ScheduledTaskAction -Execute $Background -Argument "--config `"$Config`"") `
		-Trigger (New-ScheduledTaskTrigger -AtLogOn -User $user) -Settings $settings -Principal $principal | Out-Null
	Start-ScheduledTask -TaskName 'Arr'
	$api = "http://127.0.0.1:$Port"
	if (-not (Wait-Healthy $api '' 30)) { throw "Arr didn't start; see $(Join-Path $StateDir 'arr.log')" }

	# ------------------------------------------------------------ automatic updates

	if (-not $AutoUpdate) {
		if (Test-Path $Updater) { $AutoUpdate = '1' }
		elseif (-not (Interactive)) { $AutoUpdate = '0' } # only ARR_AUTO_UPDATE=1 turns it on
		elseif (Confirm 'Install new versions of Arr automatically (checked once a day)?' 'y') { $AutoUpdate = '1' }
		else { $AutoUpdate = '0' }
	}
	if ($AutoUpdate -eq '1') {
		Step 'Turning on automatic updates'
		Write-Updater $Updater $Repo $Target $BinDir $StateDir $Port
		$at = (Get-Date).Date.AddHours(4).AddMinutes((Get-Random -Maximum 60))
		$ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
		$taskArgs = "--hidden `"$ps`" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$Updater`""
		Register-ScheduledTask -TaskName 'Arr update' -Description 'Installs new Arr releases (installed by install.ps1)' -Force `
			-Action (New-ScheduledTaskAction -Execute $Background -Argument $taskArgs) `
			-Trigger (New-ScheduledTaskTrigger -Daily -At $at) `
			-Settings (New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew) `
			-Principal $principal | Out-Null
		Say 'Arr checks for a new release once a day and installs it. Turn this off with $env:ARR_AUTO_UPDATE = "0" and running this again.'
	} elseif (Test-Path $Updater) {
		Step 'Turning off automatic updates'
		Unregister-ScheduledTask -TaskName 'Arr update' -Confirm:$false -ErrorAction SilentlyContinue
		Remove-Item $Updater -Force
	}

	# ------------------------------------------------------------ what you already own

	Step 'Learning what you already own from Plex'
	$token = (Get-Content (Join-Path $StateDir 'secrets\api_token') -Raw).Trim()
	$media = 'all'
	if (-not (Select-String -Path $Config -Pattern '^movies = ' -Quiet)) { $media = 'tv' }
	if (-not (Select-String -Path $Config -Pattern '^tv = ' -Quiet)) { $media = 'movies' }
	$boot = $null
	for ($i = 0; $i -lt 500; $i++) {
		$boot = Invoke-RestMethod -Method Post "$api/api/v1/setup/plex/bootstrap" `
			-Headers @{ Authorization = "Bearer $token" } -ContentType 'application/json' `
			-Body (@{ media = $media } | ConvertTo-Json)
		if ($boot.done) { break }
	}
	if (-not $boot.done) { throw "the Plex import didn't finish; run this again to continue it" }
	# "done" also covers a library whose import failed: report it instead of claiming success.
	$failed = @($boot.runs | Where-Object { $_.status -eq 'failed' }) | Select-Object -First 1
	if ($failed) { throw "the Plex import failed: $($failed.last_error); run this again to retry" }
	Say 'Done. Titles you already have are marked owned, and nothing was downloaded.'

	$mcp = @{ mcpServers = @{ arr = @{ command = $Arr; args = @('mcp', '--api-url', "$api/", '--token-file', (Join-Path $StateDir 'secrets\api_token')) } } } |
		ConvertTo-Json -Depth 5
	Say ''
	Say "Arr is running at $api"
	Say "  API token:   $(Join-Path $StateDir 'secrets\api_token')"
	Say "  Config:      $Config"
	Say "  Logs:        $(Join-Path $StateDir 'arr.log')"
	Say "  Downloading: $(if ($downloading) { 'on' } else { 'off (set [execution] enabled = true to turn it on)' })"
	Say ''
	Say 'To use Arr from Claude Desktop or another MCP client on this computer, add:'
	Say ''
	Say $mcp
	Say ''
	Say 'Using Arr from ChatGPT on your phone needs remote access, which the Windows installer'
	Say "doesn't set up yet: https://github.com/$Repo/blob/main/docs/REMOTE.md"
}

# Stops the running Arr (the log-on task and its program) so its files can be replaced.
function Stop-Arr($background) {
	if (Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue) {
		Stop-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
	}
	Get-CimInstance Win32_Process -Filter "Name = 'arr-background.exe'" -ErrorAction SilentlyContinue |
		Where-Object { $_.ExecutablePath -eq $background -and $_.CommandLine -like '*--config*' } |
		ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

# Puts $src at $dst even while $dst runs (Claude Desktop may be running `arr mcp`):
# Windows can rename a running program, just not overwrite it.
function Replace-File($src, $dst) {
	if (Test-Path $dst) {
		$old = "$dst.old-" + [guid]::NewGuid().ToString('N').Substring(0, 8)
		Move-Item $dst $old -Force
		Remove-Item $old -Force -ErrorAction SilentlyContinue # still running: left for next time
	}
	Get-ChildItem "$dst.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
	Copy-Item $src $dst -Force
}

# Adds $dir to your PATH (new terminals see it), keeping %VARIABLES% in the PATH as they are.
function Add-ToPath($dir) {
	$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
	$path = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
	if (($path -split ';') -notcontains $dir) {
		$key.SetValue('Path', (($path.TrimEnd(';') + ";$dir").TrimStart(';')), 'ExpandString')
		# Setting any variable tells open programs (Explorer) that the environment changed.
		[Environment]::SetEnvironmentVariable('ARR_INSTALLER_REFRESH', $null, 'User')
	}
	$key.Close()
	if (($env:Path -split ';') -notcontains $dir) { $env:Path += ";$dir" }
}

function Find-FFprobe {
	$c = Get-Command ffprobe.exe -ErrorAction SilentlyContinue | Select-Object -First 1
	if ($c) {
		# Chocolatey's and Scoop's ffprobe.exe are launchers that start the real one as a
		# second process, which Arr's one-process sandbox refuses: use the real one.
		$p = $c.Source
		if ($p -like '*\chocolatey\bin\ffprobe.exe') {
			$lib = Join-Path (Split-Path (Split-Path $p)) 'lib'
			$real = Get-ChildItem $lib -Directory -Filter 'ffmpeg*' -ErrorAction SilentlyContinue |
				Get-ChildItem -Recurse -Filter ffprobe.exe -ErrorAction SilentlyContinue | Select-Object -First 1
			if ($real) { return $real.FullName }
		} elseif ($p -like '*\scoop\shims\ffprobe.exe') {
			$real = Join-Path (Split-Path (Split-Path $p)) 'apps\ffmpeg\current\bin\ffprobe.exe'
			if (Test-Path $real) { return $real }
		}
		return $p
	}
	# winget's FFmpeg, before this window's PATH knows about it.
	$link = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\ffprobe.exe'
	if (Test-Path $link) { return $link }
	$pkg = Get-ChildItem (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages') -Filter ffprobe.exe -Recurse -ErrorAction SilentlyContinue |
		Select-Object -First 1
	if ($pkg) { return $pkg.FullName }
	return $null
}

# Waits up to $seconds for Arr to answer its health check (as $version, if given).
function Wait-Healthy($api, $version, $seconds) {
	$deadline = (Get-Date).AddSeconds($seconds)
	while ((Get-Date) -lt $deadline) {
		try {
			$h = Invoke-RestMethod "$api/api/v1/health" -TimeoutSec 3
			if (-not $version -or $h.version -eq $version.TrimStart('v')) { return $true }
		} catch {}
		Start-Sleep -Seconds 1
	}
	return $false
}

function Write-Updater($path, $repo, $target, $binDir, $stateDir, $port) {
	$q = { param($s) "'" + ($s -replace "'", "''") + "'" }
	$head = @"
# Arr updater, written by install.ps1. The daily "Arr update" task runs it; you can run it
# by hand too. It trusts only the release zips CI builds from version tags (checked by
# SHA-256), and goes back to the old version if the new one doesn't start.
`$Repo = $(& $q $repo)
`$Target = $(& $q $target)
`$BinDir = $(& $q $binDir)
`$StateDir = $(& $q $stateDir)
`$Port = $port
"@
	$body = @'
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$log = Join-Path $StateDir 'update.log'
function Log($m) { $l = '{0} arr-update: {1}' -f (Get-Date -Format s), $m; Add-Content $log $l; Write-Output $l }
$arr = Join-Path $BinDir 'arr.exe'
$api = "http://127.0.0.1:$Port"
function Restart-Arr {
	Stop-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
	Get-CimInstance Win32_Process -Filter "Name = 'arr-background.exe'" -ErrorAction SilentlyContinue |
		Where-Object { $_.CommandLine -like '*--config*' } |
		ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
	Start-ScheduledTask -TaskName 'Arr'
}
function Healthy($version) {
	$deadline = (Get-Date).AddSeconds(30)
	while ((Get-Date) -lt $deadline) {
		try { if ((Invoke-RestMethod "$api/api/v1/health" -TimeoutSec 3).version -eq $version.TrimStart('v')) { return $true } } catch {}
		Start-Sleep -Seconds 1
	}
	return $false
}
# One update at a time: Windows releases this lock when the process ends, so it can't go stale.
try { $lock = [IO.File]::Open((Join-Path $BinDir 'arr-update.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
catch { Log 'another update is running'; exit 0 }
$tmp = $null
try {
	$releases = if ($env:ARR_RELEASES_URL) { $env:ARR_RELEASES_URL } else { "https://github.com/$Repo/releases" }
	$req = [Net.HttpWebRequest]::Create("$releases/latest")
	$req.AllowAutoRedirect = $false
	$resp = $req.GetResponse()
	$latest = ([string]$resp.Headers['Location']) -replace '.*/tag/', ''
	$resp.Close()
	if ($latest -notmatch '^v\d+\.\d+\.\d+$') { Log "couldn't find the latest release"; exit 1 }
	$current = 'v' + ((& $arr --version) -split ' ')[1]
	if ([version]$latest.TrimStart('v') -le [version]$current.TrimStart('v')) { Log "up to date ($current)"; exit 0 }
	Log "updating $current to $latest"
	$tmp = Join-Path ([IO.Path]::GetTempPath()) ('arr-update-' + [guid]::NewGuid().ToString('N'))
	New-Item -ItemType Directory $tmp | Out-Null
	$asset = "arr-$Target.zip"
	Invoke-WebRequest -UseBasicParsing "$releases/download/$latest/$asset" -OutFile "$tmp\$asset"
	Invoke-WebRequest -UseBasicParsing "$releases/download/$latest/$asset.sha256" -OutFile "$tmp\$asset.sha256"
	$want = ((Get-Content "$tmp\$asset.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
	if ((Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower() -ne $want) { Log 'checksum mismatch; not updating'; exit 1 }
	Expand-Archive "$tmp\$asset" -DestinationPath $tmp -Force
	$new = Join-Path $tmp "arr-$Target"
	& (Join-Path $new 'arr.exe') --version | Out-Null
	if ($LASTEXITCODE -ne 0) { Log "$latest doesn't run on this computer; not updating"; exit 1 }
	# Running programs can be renamed, not overwritten: move each aside, keeping it for a rollback.
	$exes = 'arr.exe', 'arr-background.exe'
	foreach ($e in $exes) {
		$p = Join-Path $BinDir $e
		Remove-Item "$p.previous" -Force -ErrorAction SilentlyContinue
		if (Test-Path "$p.previous") { Move-Item "$p.previous" "$p.old-$([guid]::NewGuid().ToString('N').Substring(0, 8))" -Force }
		Move-Item $p "$p.previous" -Force
		Copy-Item (Join-Path $new $e) $p -Force
	}
	Restart-Arr
	if (Healthy $latest) { Log "now running $latest"; exit 0 }
	Log "$latest didn't start; going back to $current"
	Stop-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
	foreach ($e in $exes) {
		$p = Join-Path $BinDir $e
		Move-Item $p "$p.failed-$([guid]::NewGuid().ToString('N').Substring(0, 8))" -Force
		Move-Item "$p.previous" $p -Force
	}
	Restart-Arr
	if (-not (Healthy $current)) { Log "$current didn't start either; check $(Join-Path $StateDir 'arr.log')" }
	exit 1
} catch {
	Log "update failed: $($_.Exception.Message)"
	exit 1
} finally {
	if ($tmp) { Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue }
	$lock.Close()
}
'@
	Set-Content -Path $path -Value ($head + "`r`n" + $body) -Encoding UTF8
}

try {
	Install-Arr
} catch {
	Write-Host "error: $($_.Exception.Message)" -ForegroundColor Red
	if ($env:ARR_INSTALL_STRICT -eq '1') { throw } # tests want a failing exit code
}
