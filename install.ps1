# Arr installer for Windows: downloads a prebuilt Arr, then sets it up next to your Plex
# server in your web browser. Open PowerShell (Start menu > type "PowerShell") and paste:
#
#   irm https://raw.githubusercontent.com/newtonharry/Arr/main/install.ps1 | iex
#
# Nothing here needs administrator rights. `irm | iex` can't pass options, so they come
# from environment variables, for example: $env:ARR_VERSION = "v0.4.0"; irm ... | iex
#
#   ARR_VERSION          a release tag such as v0.4.0 (default: the latest release)
#   ARR_AUTO_UPDATE      1 to install new versions automatically, 0 to turn that off
#   ARR_ERROR_REPORTS    1 to send error reports, 0 not to (docs/PRIVACY.md)
#   ARR_TMDB_KEY         an optional backup TMDB key (movies come from Plex)
#   ARR_UPLOAD_LIMIT_KIB upload limit for downloads in KiB/s (0 = no limit)
#   ARR_TORRENT_PORT     the port downloads use (default 50413)
#   ARR_PORT             Arr's port on this computer (default 8787; first setup only, and
#                        with ARR_YES a re-run with a different port is refused)
#   ARR_STATE_DIR        Arr's folder: config, secrets, database (default: where Arr is
#                        installed, else %LOCALAPPDATA%\Arr)
#   ARR_BIN_DIR          where arr.exe goes (default: where it is, else %LOCALAPPDATA%\Programs\Arr)
#   ARR_NO_SETUP         1 to install the program only
#   ARR_YES              1 to never ask (FFmpeg is installed without asking); first setup
#                        needs the setup page, so it needs someone at the browser
#
# Running this again keeps your settings and updates Arr; the options above that you give
# are applied, the rest are kept.
#
# Use Arr from ChatGPT on your phone or any device (docs/REMOTE.md): set ARR_REMOTE=1, or
# answer yes when asked. These skip the questions (an AI agent can pass them all); they are
# used only while adding remote access, and ignored once it's set up:
#
#   ARR_NGROK_TOKEN, ARR_NGROK_DOMAIN                     your ngrok authtoken and free domain
#   ARR_GOOGLE_CLIENT_ID, ARR_GOOGLE_CLIENT_SECRET        your Google OAuth client
#   ARR_ALLOW_EMAILS                                      Google accounts allowed in, comma-separated

# GitHub's downloads sometimes time out (504) for a while: retry what can clear up (no answer,
# 5xx, 408, 429), 6 tries 10 s apart; anything else (a 404) fails at once.
function Get-Download($url, $out) {
	for ($i = 1; ; $i++) {
		try { Invoke-WebRequest -UseBasicParsing $url -OutFile $out; return }
		catch {
			$code = 0; try { $code = [int]$_.Exception.Response.StatusCode } catch {}
			if ($i -ge 6 -or ($code -ne 0 -and $code -lt 500 -and $code -ne 408 -and $code -ne 429)) { throw }
			Start-Sleep -Seconds 10
		}
	}
}
# Arr's own release files: when GitHub's download page won't answer and the default GitHub
# address is in use ($fallback), the same file comes through GitHub's API, which stays up.
# Once that happens, the later files go straight there.
function Get-ArrAsset($url, $out, $name, $repo, $release, $fallback) {
	if (-not $script:ViaApi) {
		try { Get-Download $url $out; return }
		catch { if (-not $fallback) { throw } }
		Write-Output "GitHub's download page didn't answer; trying GitHub's API instead"
		$script:ViaApi = $true
	}
	$rel = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/$release" -Headers @{ Accept = 'application/vnd.github+json' }
	$asset = @($rel.assets | Where-Object { $_.name -eq $name })[0]
	if (-not $asset) { throw "GitHub's API doesn't list $name in release $release" }
	Invoke-WebRequest -UseBasicParsing $asset.url -Headers @{ Accept = 'application/octet-stream' } -OutFile $out
}

function Install-Arr {
	# irm | iex runs in the caller's scope: forget an earlier run's fallback in this window.
	$script:ViaApi = $false
	$ErrorActionPreference = 'Stop'
	$ProgressPreference = 'SilentlyContinue' # the progress bar makes downloads very slow
	# What this run changed, for the error handler at the bottom to undo (an earlier run in this
	# window may have left these set): Arr stopped, programs swapped, settings saved aside.
	$script:ArrStopped = $false # set by Stop-Arr: if this run fails, Arr is started again
	$script:Swapped = @()
	$script:Snapshot = $null
	$script:Healthy = $false # once Arr runs well again, nothing is undone
	[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

	$Repo = 'newtonharry/Arr'
	$Target = 'x86_64-pc-windows-msvc'
	$Version = if ($env:ARR_VERSION) { $env:ARR_VERSION } else { 'latest' }
	$DownloadBase = $env:ARR_DOWNLOAD_BASE
	$ApiFallback = -not $DownloadBase # only GitHub's own address falls back to its API
	$ApiRelease = if ($Version -eq 'latest') { 'latest' } else { "tags/$Version" }
	if (-not $DownloadBase) {
		$DownloadBase = if ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download" }
		else { "https://github.com/$Repo/releases/download/$Version" }
	}
	$BinDir, $StateDir = Get-ArrDirs
	# A given option is checked before anything changes, so it either applies or stops here.
	foreach ($o in @(@('ARR_PORT', 1, 65535), @('ARR_TORRENT_PORT', 1, 65535), @('ARR_UPLOAD_LIMIT_KIB', 0, 4194303),
			@('ARR_ERROR_REPORTS', 0, 1), @('ARR_AUTO_UPDATE', 0, 1))) {
		$v = [Environment]::GetEnvironmentVariable($o[0])
		if ($v -and -not ($v -match '^\d{1,7}$' -and [int]$v -ge $o[1] -and [int]$v -le $o[2])) {
			$what = if ($o[2] -eq 1) { '0 or 1' } else { "a number from $($o[1]) to $($o[2])" }
			throw "$($o[0]) must be $what, not '$v'; nothing was changed"
		}
	}
	$Port = if ($env:ARR_PORT) { [int]$env:ARR_PORT } else { 8787 }
	$TorrentPort = if ($env:ARR_TORRENT_PORT) { [int]$env:ARR_TORRENT_PORT } else { 50413 }
	$UploadLimit = if ($env:ARR_UPLOAD_LIMIT_KIB) { [int]$env:ARR_UPLOAD_LIMIT_KIB } else { 0 }
	$AutoUpdate = $env:ARR_AUTO_UPDATE
	$Yes = $env:ARR_YES -eq '1'
	$Config = Join-Path $StateDir 'config.toml'
	$Arr = Join-Path $BinDir 'arr.exe'
	$Background = Join-Path $BinDir 'arr-background.exe'
	$Updater = Join-Path $BinDir 'arr-update.ps1'
	$NgrokYml = Join-Path $StateDir 'ngrok.yml'
	$TunnelLog = Join-Path $StateDir 'tunnel.log'

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

	# Runs arr with any piped text on its stdin; returns its output, stderr too, as text, and
	# $LASTEXITCODE says how it went. (Windows PowerShell turns a native program's stderr
	# into errors, hence Continue.)
	function Run-Arr {
		$ErrorActionPreference = 'Continue'
		$input | & $Arr @args 2>&1 | ForEach-Object { "$_" }
	}
	# Changes one setting through `arr set`, which checks it and changes nothing if it's refused.
	function Set-Arr {
		$out = $input | Run-Arr set --config $Config @args
		if ($LASTEXITCODE -ne 0) { throw "couldn't set $($args[0]): $(($out -join ' ') -replace '^Error: ', '')" }
		Say $out
	}

	if (-not [Environment]::Is64BitOperatingSystem) { throw 'Arr needs 64-bit Windows.' }
	if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
		Say 'This PC has an ARM processor; Arr runs through Windows 11''s x64 emulation.'
	}
	if ($Yes -and $env:ARR_NO_SETUP -ne '1' -and -not (Test-Path $Config)) {
		throw 'Windows setup needs the setup page in a browser; run this without ARR_YES'
	}
	if ($StateDir -ne (Join-Path $env:LOCALAPPDATA 'Arr') -and -not $env:ARR_STATE_DIR) { Say "Found Arr installed in $StateDir" }
	Assert-OwnTask
	# A re-run keeps Arr's port from its config.
	if (Test-Path $Config) {
		$m = Select-String -Path $Config -Pattern '^listen = "[^"]*:(\d+)"$' | Select-Object -First 1
		if ($m) { $Port = [int]$m.Matches[0].Groups[1].Value }
		if ($env:ARR_PORT -and [int]$env:ARR_PORT -ne $Port) {
			if ($Yes) { throw "ARR_PORT is $env:ARR_PORT, but Arr uses port $Port from its config (change listen in $Config); nothing was changed" }
			# Options left set in this window from an earlier run shouldn't stop this one.
			Write-Warning "ARR_PORT is ignored: Arr keeps port $Port from its config"
		}
		# Torrent settings need Arr's own downloader; refuse before anything changes.
		if (($env:ARR_UPLOAD_LIMIT_KIB -or $env:ARR_TORRENT_PORT) -and
			-not (Select-String -Path $Config -Pattern '^\[embedded\]' -Quiet)) {
			throw "ARR_UPLOAD_LIMIT_KIB and ARR_TORRENT_PORT need Arr's own downloader ([embedded] in $Config); nothing was changed"
		}
	}

	# ------------------------------------------------------------ install the program

	Step "Downloading Arr ($Version)"
	$asset = "arr-$Target.zip"
	$tmp = Join-Path ([IO.Path]::GetTempPath()) ('arr-' + [guid]::NewGuid().ToString('N'))
	New-Item -ItemType Directory $tmp | Out-Null
	try {
		Get-ArrAsset "$DownloadBase/$asset" "$tmp\$asset" $asset $Repo $ApiRelease $ApiFallback
		Get-ArrAsset "$DownloadBase/$asset.sha256" "$tmp\$asset.sha256" "$asset.sha256" $Repo $ApiRelease $ApiFallback
		$want = ((Get-Content "$tmp\$asset.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
		$got = (Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower()
		if ($got -ne $want) { throw "checksum mismatch for $asset; nothing was installed" }
		Expand-Archive "$tmp\$asset" -DestinationPath $tmp -Force
		$new = "$tmp\arr-$Target"
		# Try it before touching the installed one.
		$version = $null
		try { $version = & "$new\arr.exe" --version } catch {}
		if ($LASTEXITCODE -ne 0 -or -not $version) { throw "the downloaded Arr doesn't run on this computer; nothing was changed" }
		New-Item -ItemType Directory -Force $BinDir | Out-Null
		# Arr keeps running meanwhile (Windows can rename a running program); a re-run keeps the
		# old programs as .previous, to go back to if the new ones don't start.
		# Until Arr runs well again, a failure puts them back (Undo-Run).
		$script:SwapDir = $BinDir
		try {
			foreach ($exe in 'arr.exe', 'arr-background.exe') {
				$dst = Join-Path $BinDir $exe
				$had = Test-Path $dst
				Replace-File "$new\$exe" $dst -Keep
				if ($had) { $script:Swapped += $exe }
			}
		} catch {
			Undo-Run
			throw "couldn't put the new version in place ($($_.Exception.Message)); the old one is still there"
		}
	} finally {
		Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
	}
	Say "Installed $version to $Arr"
	Add-ToPath $BinDir

	if ($env:ARR_NO_SETUP -eq '1') {
		if (Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue) {
			Say 'Restart Arr to use the new version: log out and back in, or run'
			Say '  Stop-ScheduledTask Arr; Start-ScheduledTask Arr'
		}
		return
	}

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
		# A config the new version can't use stops here, with Arr still running as it was (and
		# the previous programs put back).
		$check = Run-Arr check --config $Config
		if ($LASTEXITCODE -ne 0) {
			$check | ForEach-Object { Say $_ }
			throw "Arr can't use $Config (see above), so Arr keeps running as it was. Fix the file, or move it aside and run this again to set up again."
		}
		if (Select-String -Path $Config -Pattern '^\[plex\]' -Quiet) {
			$null = Run-Arr plex-signin --config $Config --check
			if ($LASTEXITCODE -eq 1 -and (Interactive)) {
				Step 'Signing Arr in to Plex again'
				Say 'Plex no longer accepts Arr''s sign-in, for example after "sign out of all devices".'
				# Its link is opened as soon as it's printed; it then waits for you to sign in.
				& { $ErrorActionPreference = 'Continue'; & $Arr plex-signin --config $Config 2>&1 } | ForEach-Object {
					Say "$_"
					if ($_ -match '(https?://\S+)') { Open-Page $Matches[1] }
				}
				if ($LASTEXITCODE -ne 0) { Write-Warning 'Arr isn''t signed in to Plex; run this again to retry.' }
			} elseif ($LASTEXITCODE -eq 1) {
				Write-Warning 'Plex no longer accepts Arr''s sign-in; run this again in PowerShell to sign in again.'
			} elseif ($LASTEXITCODE -ne 0) {
				Write-Warning 'couldn''t reach Plex to check Arr''s sign-in; carrying on.'
			}
		}
		# Movies come from the Plex sign-in. A TMDB key is an optional backup ($env:ARR_TMDB_KEY);
		# an existing one is kept.
		$tmdbKey = $env:ARR_TMDB_KEY
		$tmdbOn = (Get-Content $Config -Raw) -match '(?ms)^\[tmdb\][^\[]*?^enabled *= *true'
		if ($tmdbKey -or $tmdbOn) { Say 'Movies now come from your Plex sign-in; your TMDB key stays as a backup.' }
		else { Say 'Movies now come from your Plex sign-in — no TMDB key needed.' }
		# Given options change only those settings; each is checked, and a refused one stops
		# here, before Arr is restarted. The TMDB key goes first: it's the one likely refused.
		# If this run fails before Arr runs well again, the config and secrets go back as they are now.
		Save-Settings $StateDir
		if ($tmdbKey) { $tmdbKey.Trim() | Set-Arr tmdb-key }
		if ($env:ARR_UPLOAD_LIMIT_KIB) { Set-Arr upload-limit-kib $UploadLimit }
		if ($env:ARR_TORRENT_PORT) { Set-Arr torrent-port $TorrentPort }
		if ($env:ARR_ERROR_REPORTS) { Set-Arr error-reports $(if ($env:ARR_ERROR_REPORTS -eq '1') { 'on' } else { 'off' }) }
	} else {
		Step 'Setting up Arr in your web browser'
		if (Test-Path "$Config.failed") { Say "(config.toml.failed in $StateDir is the last setup attempt that failed; this starts over)" }
		$firstInstall = $true
		$reports = @(if ($env:ARR_ERROR_REPORTS -eq '1') { '--error-reports' })
		# Remote access given up front: the page can show its address (it's set up after this).
		$wizardDomain = ($env:ARR_NGROK_DOMAIN -replace '^https?://', '' -replace '/.*$', '').ToLowerInvariant()
		$publicArgs = @(if ($wizardDomain -match '^[a-z0-9-]+(\.[a-z0-9-]+)+$') { '--public-url'; "https://$wizardDomain" })
		& $Arr wizard --state-dir $StateDir --port $Port --torrent-port $TorrentPort `
			--upload-limit-kib $UploadLimit --ffprobe $ffprobe --open @reports @publicArgs
		if ($LASTEXITCODE -ne 0 -or -not (Test-Path $Config)) { throw "setup wasn't finished; run this again to start over" }
	}
	# The setup page asked about automatic updates (maybe on a run that stopped before here);
	# the answer is used once, below.
	$choices = Join-Path $StateDir 'setup-choices'
	if (-not $AutoUpdate -and (Test-Path $choices)) {
		$c = Select-String -Path $choices -Pattern '^auto_update=([01])$' | Select-Object -First 1
		if ($c) { $AutoUpdate = $c.Matches[0].Groups[1].Value }
	}
	Remove-Item $choices -Force -ErrorAction SilentlyContinue
	$downloading = (Get-Content $Config -Raw) -match '\[execution\]\r?\nenabled = true'

	# ------------------------------------------------------------ phone and ChatGPT access

	$hasRemote = Select-String -Path $Config -Pattern '^\[mcp\.oauth\]' -Quiet
	$remoteValues = "$env:ARR_NGROK_TOKEN$env:ARR_NGROK_DOMAIN$env:ARR_GOOGLE_CLIENT_ID$env:ARR_GOOGLE_CLIENT_SECRET$env:ARR_ALLOW_EMAILS"
	$wantRemote = $env:ARR_REMOTE -eq '1' -or $remoteValues
	if ($hasRemote -and $remoteValues) {
		Write-Warning 'remote access is already set up, so the ARR_NGROK_*, ARR_GOOGLE_* and ARR_ALLOW_EMAILS values are ignored (docs/REMOTE.md says how to change it)'
	}
	if (-not $wantRemote -and -not $hasRemote -and (Interactive)) {
		Say ''
		Say 'Arr can also work from ChatGPT on your phone and other devices. That needs a free ngrok'
		Say 'account and a Google sign-in app; this guide walks you through both (about 15 minutes).'
		$wantRemote = Confirm 'Set that up now?' 'n'
	}
	if ($wantRemote -and $hasRemote) {
		Step 'Setting up access from ChatGPT on your phone and other devices'
		Say 'Your config already has remote access; keeping it.'
	} elseif ($wantRemote) {
		Set-RemoteAccess
	}

	# ------------------------------------------------------------ run it

	Step 'Starting Arr'
	# A task that starts Arr when you log in (as Plex on Windows does), without a window.
	$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
	$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
		-ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
		-StartWhenAvailable -MultipleInstances IgnoreNew
	$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
	# Only now is Arr stopped: everything that could fail without it running is done.
	Stop-Arr $Background
	Register-ScheduledTask -TaskName 'Arr' -Description 'Arr media manager (installed by install.ps1)' -Force `
		-Action (New-ScheduledTaskAction -Execute $Background -Argument "--config `"$Config`"") `
		-Trigger (New-ScheduledTaskTrigger -AtLogOn -User $user) -Settings $settings -Principal $principal | Out-Null
	Start-ScheduledTask -TaskName 'Arr'
	$script:ArrStopped = $false
	$api = "http://127.0.0.1:$Port"
	$log = Join-Path $StateDir 'arr.log'
	if (-not (Wait-Healthy $api '' 30)) {
		if (-not $script:Swapped -and -not $script:Snapshot) { throw "Arr didn't start; see $log" }
		$what = if ($script:Swapped) { 'the new version of Arr didn''t start, so the previous version and settings are back' }
		else { 'Arr didn''t start with the new settings, so the previous ones are back' }
		Stop-Arr $Background
		Undo-Run
		Start-ScheduledTask -TaskName 'Arr'
		$script:ArrStopped = $false
		$back = if (Wait-Healthy $api '' 30) { 'and it is running' } else { 'but it didn''t start either' }
		throw "$what, $back; see $log"
	}
	$script:Healthy = $true
	if ($script:Snapshot) { Remove-Item -Recurse -Force $script:Snapshot -ErrorAction SilentlyContinue; $script:Snapshot = $null }

	# A configured tunnel runs as its own log-on task next to Arr, without a window.
	$PublicUrl = $null
	$m = Select-String -Path $Config -Pattern '^public_url = "(https://[^"]+)"$' | Select-Object -First 1
	if ($m -and (Test-Path $NgrokYml) -and (Select-String -Path $Config -Pattern '^\[mcp\.oauth\]' -Quiet)) {
		$PublicUrl = $m.Matches[0].Groups[1].Value
		# Also after an uninstall that kept Arr's data: the tunnel needs ngrok back.
		$ngrok = Find-Ngrok $BinDir
		if (-not $ngrok) { $ngrok = Install-Ngrok $BinDir }
		Step "Starting the tunnel for $PublicUrl"
		Stop-Tunnel $NgrokYml
		$tunnelArgs = "--hidden `"$ngrok`" http 127.0.0.1:$Port `"--url=$PublicUrl`" --config `"$NgrokYml`" `"--log=$TunnelLog`" --log-level=warn"
		Register-ScheduledTask -TaskName 'Arr tunnel' -Description 'Public HTTPS address for Arr, ngrok (installed by install.ps1)' -Force `
			-Action (New-ScheduledTaskAction -Execute $Background -Argument $tunnelArgs) `
			-Trigger (New-ScheduledTaskTrigger -AtLogOn -User $user) -Settings $settings -Principal $principal | Out-Null
		Start-ScheduledTask -TaskName 'Arr tunnel'
	} elseif (Get-ScheduledTask -TaskName 'Arr tunnel' -ErrorAction SilentlyContinue) {
		Step 'Removing the tunnel (remote access isn''t set up any more)'
		Stop-Tunnel $NgrokYml
		Unregister-ScheduledTask -TaskName 'Arr tunnel' -Confirm:$false
	}

	# ------------------------------------------------------------ automatic updates

	$updateTask = Get-ScheduledTask -TaskName 'Arr update' -ErrorAction SilentlyContinue
	if (-not $AutoUpdate) {
		# Already on (its script may be gone, for example removed by antivirus): repaired below.
		if ((Test-Path $Updater) -or $updateTask) { $AutoUpdate = '1' }
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
	} elseif ((Test-Path $Updater) -or $updateTask) {
		Step 'Turning off automatic updates'
		Unregister-ScheduledTask -TaskName 'Arr update' -Confirm:$false -ErrorAction SilentlyContinue
		Remove-Item $Updater -Force -ErrorAction SilentlyContinue
	}

	# ------------------------------------------------------------ what you already own

	if (Select-String -Path $Config -Pattern '^\[plex\]' -Quiet) {
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
	}

	$mcp = @{ mcpServers = @{ arr = @{ command = $Arr; args = @('mcp', '--api-url', "$api/", '--token-file', (Join-Path $StateDir 'secrets\api_token')) } } } |
		ConvertTo-Json -Depth 5
	Say ''
	Say 'Arr is running. Open it at:'
	Say "  this computer: http://localhost:$Port"
	if ($PublicUrl) { Say "  any device:    $PublicUrl   (sign in with Google)" }
	# A first install has nobody signed in yet: a link that works once, for 15 minutes. Anyone
	# who has it can sign in, so it is made only when a person is at the console (not ARR_YES,
	# not redirected) and goes straight to the console with [Console]::WriteLine: Write-Host output can still be
	# captured by stream redirection (*> file), the console can't. (With
	# remote access it would name the public address, which may not be reachable yet, so it
	# names this computer.)
	if ($firstInstall -and (Interactive) -and -not [Console]::IsOutputRedirected) {
		$link = & { $ErrorActionPreference = 'Continue'; & $Arr app-link --config $Config 2>$null | Select-Object -First 1 }
		if ($link) {
			[Console]::WriteLine('')
			[Console]::WriteLine('To sign in on this computer, open this link. It works once, within 15 minutes:')
			[Console]::WriteLine("  " + ($link -replace '^https://[^/]*', "http://localhost:$Port"))
		}
	}
	Say ''
	Say "Arr API: $api"
	Say "  API token:   $(Join-Path $StateDir 'secrets\api_token')"
	Say "  Config:      $Config"
	Say "  Logs:        $(Join-Path $StateDir 'arr.log')"
	$dl = if ($downloading) { 'on' } else { "off (turn it on: arr set --config `"$Config`" downloading on)" }
	Say "  Downloading: $dl"
	Say ''
	Say 'To use Arr from Claude Desktop or another MCP client on this computer, add:'
	Say ''
	Say $mcp
	if (-not $PublicUrl) {
		Say ''
		Say 'To use Arr from ChatGPT on your phone or any device, run the install line again after'
		Say "`$env:ARR_REMOTE = '1' (guide: https://github.com/$Repo/blob/main/docs/REMOTE.md)."
		return
	}

	Step "Checking $PublicUrl"
	$ok = $false
	$tries = if ($env:ARR_REMOTE_CHECK_SECONDS) { [int]([int]$env:ARR_REMOTE_CHECK_SECONDS / 2) } else { 30 }
	for ($i = 0; $i -lt $tries -and -not $ok; $i++) {
		try {
			# The header skips ngrok's "You are about to visit" page, shown to browser-like clients.
			$r = Invoke-WebRequest -UseBasicParsing "$PublicUrl/.well-known/oauth-protected-resource/mcp" `
				-Headers @{ 'ngrok-skip-browser-warning' = '1' } -TimeoutSec 10
			$ok = $r.Content -match [regex]::Escape("`"$PublicUrl/mcp`"")
		} catch {}
		if (-not $ok) { Start-Sleep -Seconds 2 }
	}
	if (-not $ok) {
		throw "$PublicUrl doesn't reach Arr yet. The tunnel's log says why (an ERR_NGROK_... code names the problem, often a wrong authtoken or a domain from another account): $TunnelLog"
	}
	Say "Arr is reachable at $PublicUrl"
	Say ''
	Say 'Add it to ChatGPT once, on chatgpt.com in a browser; it then works in the ChatGPT app on'
	Say 'your phone and every other device signed in to your account:'
	Say ''
	Say '  1. Settings > Apps & Connectors > Advanced: turn on Developer mode.'
	Say '  2. Back in Apps & Connectors, choose Create and fill in:'
	Say '       Name:            Arr'
	Say "       MCP server URL:  $PublicUrl/mcp"
	Say '       Authentication:  OAuth'
	Say '  3. Create, then sign in: choose Visit Site on the ngrok page, approve Arr, and pick your'
	Say '     Google account.'
	Say '  4. In a new chat, ask "what''s in my library?"'
	Say ''
	Say "ChatGPT's menu names change now and then; docs/REMOTE.md has the current steps."
}

# Sets up the public address (ngrok) and Google sign-in, so ChatGPT on any device can reach
# Arr (docs/REMOTE.md). Uses Install-Arr's variables. Account sign-ups and Google's pages
# are yours to click through; this asks for what they give you, opening each page for you.
function Set-RemoteAccess {
	Step 'Setting up access from ChatGPT on your phone and other devices'
	Say 'You need a free ngrok account and a Google OAuth client. The full guide:'
	Say "  https://github.com/$Repo/blob/main/docs/REMOTE.md"

	Say ''
	Say 'Step 1 of 2: ngrok gives Arr a public HTTPS address (free, no router changes).'
	Say '  Sign up or log in, then copy "Your Authtoken", and on the Domains page copy your free'
	Say '  domain (create one if the list is empty).'
	Open-Page 'https://dashboard.ngrok.com/get-started/your-authtoken'
	$token = Read-Value 'ngrok authtoken' $env:ARR_NGROK_TOKEN 'ARR_NGROK_TOKEN' -Secret
	if ($token -notmatch '^[A-Za-z0-9_-]+$') { throw "that doesn't look like an ngrok authtoken" }
	Open-Page 'https://dashboard.ngrok.com/domains'
	$domain = Read-Value 'ngrok domain, like quiet-river-1234.ngrok-free.app' $env:ARR_NGROK_DOMAIN 'ARR_NGROK_DOMAIN'
	$domain = ($domain -replace '^https?://', '' -replace '/.*$', '').ToLowerInvariant()
	if ($domain -notmatch '^[a-z0-9-]+(\.[a-z0-9-]+)+$') { throw "that doesn't look like a domain: $domain" }

	Say ''
	Say 'Step 2 of 2: Google sign-in decides who may use Arr. In Google Cloud:'
	Say '  - create a project (any name, for example arr);'
	Say '  - Google Auth Platform > Get started: app name Arr, audience External, your email;'
	Say '  - Audience: keep it in Testing and add each Google account that may use Arr as a test user;'
	Say '  - Clients > Create client: type "Web application", with this authorized redirect URI:'
	Say "      https://$domain/auth/callback"
	Say '  - copy the client ID and client secret.'
	Open-Page 'https://console.cloud.google.com/auth/clients'
	$clientId = Read-Value 'Google OAuth client ID' $env:ARR_GOOGLE_CLIENT_ID 'ARR_GOOGLE_CLIENT_ID'
	if ($clientId -notmatch '^[A-Za-z0-9._-]+$') { throw "that doesn't look like a Google client ID" }
	$secret = Read-Value 'Google OAuth client secret' $env:ARR_GOOGLE_CLIENT_SECRET 'ARR_GOOGLE_CLIENT_SECRET' -Secret
	if ($secret -notmatch '^[\x21-\x7e]+$') { throw "that doesn't look like a Google client secret" }
	$emailsRaw = Read-Value 'Google accounts allowed to use Arr (comma-separated)' $env:ARR_ALLOW_EMAILS 'ARR_ALLOW_EMAILS'
	$emails = @($emailsRaw -split '[,\s]+' | Where-Object { $_ } | ForEach-Object { $_.ToLowerInvariant() })
	foreach ($e in $emails) {
		if ($e -notmatch '^[a-z0-9._%+-]+@[a-z0-9-]+(\.[a-z0-9-]+)+$') { throw "not an email address: $e" }
	}
	if ($emails.Count -eq 0) { throw 'remote access needs at least one allowed Google account' }

	$ngrok = Find-Ngrok $BinDir
	if (-not $ngrok) { $ngrok = Install-Ngrok $BinDir }

	# The authtoken stays in a private file, never on a command line. web_addr: false turns
	# off ngrok's local inspection page, which would show the sign-in tokens passing through.
	Write-Private $NgrokYml "version: `"3`"`nagent:`n  authtoken: $token`n  web_addr: false`n"
	& $ngrok config check --config $NgrokYml | Out-Null
	if ($LASTEXITCODE -ne 0) { throw 'ngrok rejected its config' }
	$secrets = Join-Path $StateDir 'secrets'
	New-Item -ItemType Directory -Force $secrets | Out-Null
	Write-Private (Join-Path $secrets 'google_client_secret') $secret
	$mcpToken = Join-Path $secrets 'mcp_token'
	if (-not (Test-Path $mcpToken)) {
		$bytes = New-Object byte[] 32
		[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
		Write-Private $mcpToken (-join ($bytes | ForEach-Object { $_.ToString('x2') }))
	}

	# /mcp must accept the ngrok host; then add the sign-in section. Restored if Arr rejects it.
	$before = [IO.File]::ReadAllText($Config)
	$hosts = "allowed_hosts = [`"$domain`", `"localhost`", `"127.0.0.1`"]"
	$lines = $before -split "`r?`n"
	$header = $lines | Where-Object { $_ -match '^\s*\[mcp\]\s*(#.*)?$' } | Select-Object -First 1
	if ($header) {
		if ($lines | Where-Object { $_ -match '^\s*allowed_hosts\s*=' }) {
			throw 'your config already sets allowed_hosts; add [mcp.oauth] by hand (see docs/MCP.md)'
		}
		$out = foreach ($l in $lines) { $l; if ($l -eq $header) { $hosts } }
		$text = ($out -join "`n")
	} else {
		$text = $before.TrimEnd() + "`n`n[mcp]`nenabled = true`ntoken_file = `"secrets/mcp_token`"`n$hosts`n"
	}
	$list = ($emails | ForEach-Object { "`"$_`"" }) -join ', '
	$text = $text.TrimEnd() + "`n`n# Google sign-in for ChatGPT and other remote MCP clients (docs/REMOTE.md).`n[mcp.oauth]`npublic_url = `"https://$domain`"`ngoogle_client_id = `"$clientId`"`ngoogle_client_secret_file = `"secrets/google_client_secret`"`nallowed_emails = [$list]`n"
	Write-Private $Config $text
	# Whatever goes wrong from here, the config goes back as it was. (Windows PowerShell can
	# turn arr's error output into an exception before its exit code is even read.)
	try {
		$ErrorActionPreference = 'Continue'
		$check = & $Arr check --config $Config 2>&1 | Out-String
		$failed = $LASTEXITCODE -ne 0
		$ErrorActionPreference = 'Stop'
		if ($failed) { throw "arr check rejected the remote access settings:`n$check" }
		if ($check -notmatch '"mcp_enabled":\s*true') {
			throw 'your config turns MCP off ([mcp] enabled = false); set it to true and run this again'
		}
	} catch {
		$ErrorActionPreference = 'Stop'
		Write-Private $Config $before
		throw "$($_.Exception.Message) (your previous config is restored)"
	}
	Say "Remote access configured for https://$domain"
}

# A value from its environment variable, or asked for (typed hidden for secrets).
function Read-Value($label, $value, $envName, [switch]$Secret) {
	if ($value) { return $value.Trim() }
	if (-not (Interactive)) { throw "remote access needs `$env:$envName (see docs/REMOTE.md)" }
	if ($Secret) {
		$s = Read-Host "  $label" -AsSecureString
		$p = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
		try { $value = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($p) }
		finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($p) }
	} else {
		$value = Read-Host "  $label"
	}
	if (-not $value.Trim()) { throw "$label is needed for remote access" }
	return $value.Trim()
}

# Opens a page in the browser while someone is at the keyboard.
function Open-Page($url) {
	if (Interactive) {
		Say "  (opening $url)"
		try { Start-Process $url } catch { Say "  Open it yourself: $url" }
	}
}

# Writes text as UTF-8 without a byte-order mark (Arr and ngrok read the bytes as they are);
# Arr's folder is private, so the file is too.
function Write-Private($path, $text) {
	[IO.File]::WriteAllText($path, $text, (New-Object Text.UTF8Encoding $false))
}

# Installs the ngrok agent into Arr's program folder; returns its path.
function Install-Ngrok($binDir) {
	Say 'Installing the ngrok agent'
	$n = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'windows-arm64' } else { 'windows-amd64' }
	$tmp = Join-Path ([IO.Path]::GetTempPath()) ('arr-ngrok-' + [guid]::NewGuid().ToString('N'))
	New-Item -ItemType Directory $tmp | Out-Null
	try {
		Invoke-WebRequest -UseBasicParsing "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-$n.zip" -OutFile "$tmp\ngrok.zip"
		Expand-Archive "$tmp\ngrok.zip" -DestinationPath $tmp -Force
		New-Item -ItemType Directory -Force $binDir | Out-Null
		Replace-File "$tmp\ngrok.exe" (Join-Path $binDir 'ngrok.exe')
	} finally {
		Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
	}
	return (Join-Path $binDir 'ngrok.exe')
}

function Find-Ngrok($binDir) {
	$own = Join-Path $binDir 'ngrok.exe'
	if (Test-Path $own) { return $own }
	$c = Get-Command ngrok.exe -ErrorAction SilentlyContinue | Select-Object -First 1
	if ($c) { return $c.Source }
	return $null
}

# Stops the tunnel task and any ngrok it started (its launcher can go while ngrok stays).
function Stop-Tunnel($ngrokYml) {
	if (Get-ScheduledTask -TaskName 'Arr tunnel' -ErrorAction SilentlyContinue) {
		Stop-ScheduledTask -TaskName 'Arr tunnel' -ErrorAction SilentlyContinue
	}
	Get-CimInstance Win32_Process -Filter "Name = 'ngrok.exe'" -ErrorAction SilentlyContinue |
		Where-Object { $_.CommandLine -and $_.CommandLine.Contains($ngrokYml) } |
		ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

# Stops the running Arr (the log-on task and its program), to restart it on the new version.
function Stop-Arr($background) {
	$script:ArrStopped = $true
	if (Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue) {
		Stop-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
	}
	# It may run from a renamed file (.previous) by now; its command line still names $background.
	Get-CimInstance Win32_Process -Filter "Name LIKE 'arr-background%'" -ErrorAction SilentlyContinue |
		Where-Object { ("$($_.ExecutablePath)".StartsWith($background, 'OrdinalIgnoreCase') -or "$($_.CommandLine)".Contains($background)) -and
			$_.CommandLine -like '*--config*' -and $_.CommandLine -notlike '*--hidden*' } |
		ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

# Puts $src at $dst even while $dst runs (Claude Desktop may be running `arr mcp`):
# Windows can rename a running program, just not overwrite it. The copy goes next to $dst
# first, so a failed copy leaves $dst as it was. -Keep keeps the old file as $dst.previous.
function Replace-File($src, $dst, [switch]$Keep) {
	Get-ChildItem "$dst*.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
	Copy-Item $src "$dst.new" -Force
	if (Test-Path $dst) {
		$old = if ($Keep) { Remove-Aside "$dst.previous"; "$dst.previous" } else { Old-Name $dst }
		Move-Item $dst $old
		try { Move-Item "$dst.new" $dst } catch { Move-Item $old $dst; throw }
		if (-not $Keep) { Remove-Item $old -Force -ErrorAction SilentlyContinue } # still running: left for next time
	} else {
		Move-Item "$dst.new" $dst
	}
}

function Old-Name($path) { "$path.old-" + [guid]::NewGuid().ToString('N').Substring(0, 8) }

# Deletes $path, or, while it runs, renames it out of the way for a later run to delete.
function Remove-Aside($path) {
	if (-not (Test-Path $path)) { return }
	$old = Old-Name $path
	Move-Item $path $old
	Remove-Item $old -Force -ErrorAction SilentlyContinue
}

# Refuses to go on when the "Arr" task runs as someone else: this run would take it over.
function Assert-OwnTask {
	$who = (Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue).Principal.UserId
	if (-not $who) { return }
	$me = [Security.Principal.WindowsIdentity]::GetCurrent()
	$sid = if ($who -match '^S-1-') { $who } else {
		try { (New-Object Security.Principal.NTAccount $who).Translate([Security.Principal.SecurityIdentifier]).Value } catch { $null }
	}
	# Without a SID, compare whole names when the task has a domain, so OTHERPC\alex isn't alex.
	$mine = if ($sid) { $sid -eq $me.User.Value }
		elseif ($who -match '\\') { $who -eq $me.Name }
		else { $who -eq ($me.Name -split '\\')[-1] }
	if (-not $mine) { throw "Arr is installed for $who; run this as that user" }
}

# Copies config.toml and secrets\ into Arr's private folder, for Undo-Run to put back.
function Save-Settings($stateDir) {
	$snap = Join-Path $stateDir 'rerun-backup'
	if (Test-Path $snap) { Remove-Item -Recurse -Force $snap }
	New-Item -ItemType Directory $snap | Out-Null
	Copy-Item (Join-Path $stateDir 'config.toml') $snap
	$secrets = Join-Path $stateDir 'secrets'
	if (Test-Path $secrets) { Copy-Item $secrets $snap -Recurse }
	$script:Snapshot = $snap
}

# Puts back what this run changed before Arr ran well again: the programs it replaced and the
# settings Save-Settings saved, copied over the files in place so they keep their permissions.
# Doing it twice is harmless.
function Undo-Run {
	# Stop on the first failure, so a restore that didn't work keeps its backup and is reported.
	$ErrorActionPreference = 'Stop'
	if ($script:Swapped) { Restore-Previous $script:SwapDir $script:Swapped; $script:Swapped = @() }
	$snap = $script:Snapshot
	if (-not $snap) { return }
	$stateDir = Split-Path $snap
	Copy-Item (Join-Path $snap 'config.toml') (Join-Path $stateDir 'config.toml') -Force
	$secrets = Join-Path $stateDir 'secrets'
	$saved = Join-Path $snap 'secrets'
	if (Test-Path $saved) {
		Get-ChildItem $secrets -File | Where-Object { -not (Test-Path (Join-Path $saved $_.Name)) } | Remove-Item -Force
		Copy-Item (Join-Path $saved '*') $secrets -Recurse -Force
	} elseif (Test-Path $secrets) {
		Remove-Item -Recurse -Force $secrets
	}
	Remove-Item -Recurse -Force $snap
	$script:Snapshot = $null
}

# Puts back the programs Replace-File -Keep kept, for each name in $names.
function Restore-Previous($binDir, $names) {
	foreach ($e in $names) {
		$p = Join-Path $binDir $e
		if (-not (Test-Path "$p.previous")) { continue }
		Remove-Aside $p
		Move-Item "$p.previous" $p
	}
}

# Arr's folders: those of the install that the "Arr" log-on task runs (it may not be in the
# default folders), else the defaults; ARR_BIN_DIR and ARR_STATE_DIR each override only their own.
function Get-ArrDirs {
	$bin = Join-Path $env:LOCALAPPDATA 'Programs\Arr'
	$state = Join-Path $env:LOCALAPPDATA 'Arr'
	$a = (Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue).Actions | Select-Object -First 1
	if ($a -and $a.Arguments -match '--config "([^"]+)"') {
		$state = Split-Path $Matches[1]
		$bin = Split-Path $a.Execute
	}
	if ($env:ARR_BIN_DIR) { $bin = $env:ARR_BIN_DIR }
	if ($env:ARR_STATE_DIR) { $state = $env:ARR_STATE_DIR }
	return $bin, $state
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
# GitHub's downloads sometimes time out (504) for a while: retry what can clear up (no answer,
# 5xx, 408, 429), 6 tries 10 s apart; anything else (a 404) fails at once.
function Get-Download($url, $out) {
	for ($i = 1; ; $i++) {
		try { Invoke-WebRequest -UseBasicParsing $url -OutFile $out; return }
		catch {
			$code = 0; try { $code = [int]$_.Exception.Response.StatusCode } catch {}
			if ($i -ge 6 -or ($code -ne 0 -and $code -lt 500 -and $code -ne 408 -and $code -ne 429)) { throw }
			Start-Sleep -Seconds 10
		}
	}
}
# Arr's own release files: when GitHub's download page won't answer and the default GitHub
# address is in use ($fallback), the same file comes through GitHub's API, which stays up.
# Once that happens, the later files go straight there.
function Get-ArrAsset($url, $out, $name, $repo, $release, $fallback) {
	if (-not $script:ViaApi) {
		try { Get-Download $url $out; return }
		catch { if (-not $fallback) { throw } }
		Write-Output "GitHub's download page didn't answer; trying GitHub's API instead"
		$script:ViaApi = $true
	}
	$rel = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/$release" -Headers @{ Accept = 'application/vnd.github+json' }
	$asset = @($rel.assets | Where-Object { $_.name -eq $name })[0]
	if (-not $asset) { throw "GitHub's API doesn't list $name in release $release" }
	Invoke-WebRequest -UseBasicParsing $asset.url -Headers @{ Accept = 'application/octet-stream' } -OutFile $out
}
$arr = Join-Path $BinDir 'arr.exe'
$api = "http://127.0.0.1:$Port"
function Restart-Arr {
	Stop-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
	Get-CimInstance Win32_Process -Filter "Name = 'arr-background.exe'" -ErrorAction SilentlyContinue |
		Where-Object { $_.CommandLine -like '*--config*' -and $_.CommandLine -notlike '*--hidden*' } |
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
	$ApiFallback = -not $env:ARR_RELEASES_URL # only GitHub's own address falls back to its API
	$latest = ''
	try {
		$req = [Net.HttpWebRequest]::Create("$releases/latest")
		$req.AllowAutoRedirect = $false
		$resp = $req.GetResponse()
		$latest = ([string]$resp.Headers['Location']) -replace '.*/tag/', ''
		$resp.Close()
	} catch {}
	if ($latest -notmatch '^v\d+\.\d+\.\d+$' -and $ApiFallback) {
		Log "GitHub's download page didn't answer; trying GitHub's API instead"
		$script:ViaApi = $true
		try { $latest = [string](Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ Accept = 'application/vnd.github+json' }).tag_name } catch {}
	}
	if ($latest -notmatch '^v\d+\.\d+\.\d+$') { Log "couldn't find the latest release"; exit 1 }
	$current = 'v' + ((& $arr --version) -split ' ')[1]
	if ([version]$latest.TrimStart('v') -le [version]$current.TrimStart('v')) { Log "up to date ($current)"; exit 0 }
	Log "updating $current to $latest"
	$tmp = Join-Path ([IO.Path]::GetTempPath()) ('arr-update-' + [guid]::NewGuid().ToString('N'))
	New-Item -ItemType Directory $tmp | Out-Null
	$asset = "arr-$Target.zip"
	Get-ArrAsset "$releases/download/$latest/$asset" "$tmp\$asset" $asset $Repo "tags/$latest" $ApiFallback
	Get-ArrAsset "$releases/download/$latest/$asset.sha256" "$tmp\$asset.sha256" "$asset.sha256" $Repo "tags/$latest" $ApiFallback
	$want = ((Get-Content "$tmp\$asset.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
	if ((Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower() -ne $want) { Log 'checksum mismatch; not updating'; exit 1 }
	Expand-Archive "$tmp\$asset" -DestinationPath $tmp -Force
	$new = Join-Path $tmp "arr-$Target"
	& (Join-Path $new 'arr.exe') --version | Out-Null
	if ($LASTEXITCODE -ne 0) { Log "$latest doesn't run on this computer; not updating"; exit 1 }
	# Running programs can be renamed, not overwritten: move each aside, keeping it for a rollback.
	$exes = 'arr.exe', 'arr-background.exe'
	$swapped = @()
	function Roll-Back {
		Stop-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
		foreach ($e in $swapped) {
			$p = Join-Path $BinDir $e
			if (Test-Path $p) { Move-Item $p "$p.failed-$([guid]::NewGuid().ToString('N').Substring(0, 8))" -Force }
			Move-Item "$p.previous" $p -Force
		}
		Restart-Arr
	}
	try {
		foreach ($e in $exes) {
			$p = Join-Path $BinDir $e
			Remove-Item "$p.previous" -Force -ErrorAction SilentlyContinue
			if (Test-Path "$p.previous") { Move-Item "$p.previous" "$p.old-$([guid]::NewGuid().ToString('N').Substring(0, 8))" -Force }
			Move-Item $p "$p.previous" -Force
			$swapped += $e
			Copy-Item (Join-Path $new $e) $p -Force
		}
	} catch {
		Log "couldn't put $latest in place ($($_.Exception.Message)); going back to $current"
		Roll-Back
		exit 1
	}
	Restart-Arr
	if (Healthy $latest) { Log "now running $latest"; exit 0 }
	Log "$latest didn't start; going back to $current"
	Roll-Back
	if (-not (Healthy $current)) { Log "$current didn't start either; check $(Join-Path $StateDir 'arr.log')" }
	# Told to Arr's developer only if the config says so (docs/PRIVACY.md).
	try { & $arr report --config (Join-Path $StateDir 'config.toml') --component updater --message "$latest didn't start, so Arr went back to $current" 2>&1 | Out-Null } catch {}
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
	# Install-Arr returning at all (ARR_NO_SETUP included) means this run finished well.
	$script:Healthy = $true
} catch {
	$failure = $_
	Write-Host "error: $($failure.Exception.Message)" -ForegroundColor Red
	# Arr isn't running well on what this run changed: put the previous programs and settings
	# back first, so Arr starts on them again (or, still running, keeps them next time).
	if (-not $script:Healthy -and ($script:Swapped -or $script:Snapshot)) {
		Write-Host 'Putting back the previous version and settings.'
		try { Undo-Run } catch { Write-Host "couldn't put them all back: $($_.Exception.Message)" -ForegroundColor Red }
	}
	# Arr was stopped to restart it on the new version, and that didn't happen: start it again.
	if ($script:ArrStopped -and (Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue)) {
		Write-Host 'Starting Arr again.'
		try { Start-ScheduledTask -TaskName 'Arr' } catch {}
	}
	# Told to Arr's developer only if this install's config says so (docs/PRIVACY.md).
	try { $bin, $state = Get-ArrDirs } catch { $bin, $state = 'none', 'none' }
	if ((Test-Path (Join-Path $bin 'arr.exe')) -and (Test-Path (Join-Path $state 'config.toml'))) {
		try {
			& (Join-Path $bin 'arr.exe') report --config (Join-Path $state 'config.toml') --component installer `
				--message $failure.Exception.Message 2>&1 | Out-Null
		} catch {}
	}
	if ($env:ARR_INSTALL_STRICT -eq '1') { throw $failure } # tests want a failing exit code
} finally {
	# Ctrl-C at a prompt skips catch: put back what this run changed all the same.
	if (-not $script:Healthy -and ($script:Swapped -or $script:Snapshot)) { try { Undo-Run } catch {} }
}
