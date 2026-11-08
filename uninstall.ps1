# Removes Arr, Plex Media Server, or both, from this PC. In PowerShell:
#
#   irm https://raw.githubusercontent.com/newtonharry/Arr/main/uninstall.ps1 | iex
#
# It asks what to remove, and separately whether to delete each one's data (settings,
# database). Your movie and TV files, and the download folder, are never deleted.
# Without questions, from environment variables:
#   ARR_REMOVE        arr, plex or all
#   ARR_DELETE_DATA   1 to delete their data too, 0 to keep it
#   ARR_YES           1 to never ask (removes Arr only, keeping data, unless the two above say otherwise)
#   ARR_DRY_RUN       1 to only show what would be done
#   ARR_BIN_DIR, ARR_STATE_DIR   if Arr isn't in the default folders

function Uninstall-Arr {
	$ErrorActionPreference = 'Stop'
	$BinDir = if ($env:ARR_BIN_DIR) { $env:ARR_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\Arr' }
	$StateDir = if ($env:ARR_STATE_DIR) { $env:ARR_STATE_DIR } else { Join-Path $env:LOCALAPPDATA 'Arr' }
	$Target = $env:ARR_REMOVE
	$DeleteData = $env:ARR_DELETE_DATA
	$Yes = $env:ARR_YES -eq '1'
	$DryRun = $env:ARR_DRY_RUN -eq '1'

	function Say($m) { Write-Host $m }
	function Step($m) { Write-Host ''; Write-Host "==> $m" -ForegroundColor Cyan }
	function Interactive { -not $Yes -and [Environment]::UserInteractive -and -not [Console]::IsInputRedirected }
	function Confirm($question) {
		if (-not (Interactive)) { return $false }
		return (Read-Host "$question [y/N]") -match '^[Yy]'
	}
	# Runs $action, or only describes it with ARR_DRY_RUN=1.
	function Do-It($what, [scriptblock]$action) {
		if ($DryRun) { Say "  would $what" } else { & $action; Say "  $what" }
	}

	# ------------------------------------------------------------ what's here

	$arrTask = Get-ScheduledTask -TaskName 'Arr' -ErrorAction SilentlyContinue
	$updateTask = Get-ScheduledTask -TaskName 'Arr update' -ErrorAction SilentlyContinue
	$arrHere = $arrTask -or (Test-Path (Join-Path $BinDir 'arr.exe')) -or (Test-Path (Join-Path $StateDir 'config.toml'))
	$downloads = $null
	$config = Join-Path $StateDir 'config.toml'
	if (Test-Path $config) {
		$m = Select-String -Path $config -Pattern '^download_dir = "(.*)"$' | Select-Object -First 1
		if ($m) { $downloads = $m.Matches[0].Groups[1].Value -replace '\\\\', '\' }
	}
	# Plex's installer registers itself for "Apps & features"; its uninstaller asks for admin itself.
	$plex = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
		'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
		'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
		Where-Object { $_.DisplayName -like 'Plex Media Server*' } | Select-Object -First 1
	$plexData = Join-Path $env:LOCALAPPDATA 'Plex Media Server'
	$plexKey = 'HKCU:\Software\Plex, Inc.\Plex Media Server'

	Say 'Found on this PC:'
	if ($arrHere) { Say "  Arr: $BinDir, data in $StateDir" } else { Say '  Arr: not installed' }
	if ($plex) { Say "  Plex Media Server: $($plex.DisplayName), data in $plexData" } else { Say '  Plex Media Server: not installed' }

	# ------------------------------------------------------------ what to remove

	if (-not $Target) {
		if (-not (Interactive)) { $Target = 'arr' }
		else {
			Say ''
			Say 'What should be removed?'
			Say '  1) Arr'
			Say '  2) Plex Media Server'
			Say '  3) Both'
			$a = Read-Host 'Choose 1, 2 or 3 [1]'
			$Target = switch ($a) { '' { 'arr' } '1' { 'arr' } '2' { 'plex' } '3' { 'all' } default { throw 'nothing chosen; nothing was changed' } }
		}
	}
	$doArr = ($Target -eq 'arr' -or $Target -eq 'all') -and $arrHere
	$doPlex = ($Target -eq 'plex' -or $Target -eq 'all') -and $plex
	if (($Target -eq 'arr' -or $Target -eq 'all') -and -not $arrHere) { Say "Arr isn't installed here." }
	if (($Target -eq 'plex' -or $Target -eq 'all') -and -not $plex) { Say "Plex isn't installed here." }
	if (-not $doArr -and -not $doPlex) { Say 'Nothing to remove.'; return }

	$arrData = $false; $plexWipe = $false
	if ($doArr -and (Test-Path $StateDir)) {
		$arrData = if ($DeleteData) { $DeleteData -eq '1' } else { Confirm "Also delete Arr's settings, secrets and database in ${StateDir}?" }
	}
	if ($doPlex) {
		$plexWipe = if ($DeleteData) { $DeleteData -eq '1' } else { Confirm "Also delete Plex's own data (its library database, watch history, posters and settings)?" }
	}

	Say ''
	Say 'This will:'
	if ($doArr) { Say "  - stop and remove Arr$(if ($arrData) { ', and delete its data' } else { ', keeping its data' })" }
	if ($doPlex) { Say "  - remove Plex Media Server$(if ($plexWipe) { ', and delete its data' } else { ', keeping its data' })" }
	Say "  - keep your movie and TV files$(if ($downloads) { ", and the download folder $downloads" })"
	if ((Interactive) -and -not (Confirm 'Go ahead?')) { throw 'nothing was changed' }

	# ------------------------------------------------------------ Arr

	if ($doArr) {
		Step 'Removing Arr'
		foreach ($t in @($arrTask, $updateTask) | Where-Object { $_ }) {
			Do-It "remove the task `"$($t.TaskName)`"" {
				Stop-ScheduledTask -TaskName $t.TaskName -ErrorAction SilentlyContinue
				Unregister-ScheduledTask -TaskName $t.TaskName -Confirm:$false
			}
		}
		$running = Get-CimInstance Win32_Process -Filter "Name = 'arr-background.exe' OR Name = 'arr.exe'" -ErrorAction SilentlyContinue |
			Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($BinDir, [StringComparison]::OrdinalIgnoreCase) }
		foreach ($p in $running) {
			Do-It "stop $($p.Name) (process $($p.ProcessId))" { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
		}
		if (Test-Path $BinDir) {
			Do-It "delete $BinDir" { Start-Sleep -Milliseconds 500; Remove-Item -Recurse -Force $BinDir }
		}
		$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
		$path = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
		$kept = ($path -split ';') | Where-Object { $_ -and $_ -ne $BinDir }
		if (($path -split ';') -contains $BinDir) {
			Do-It "take $BinDir off your PATH" {
				$key.SetValue('Path', ($kept -join ';'), 'ExpandString')
				[Environment]::SetEnvironmentVariable('ARR_INSTALLER_REFRESH', $null, 'User')
			}
		}
		$key.Close()
		if ($arrData) {
			Do-It "delete $StateDir (Arr's settings and database)" { Remove-Item -Recurse -Force $StateDir }
		} elseif (Test-Path $StateDir) {
			Say "  kept $StateDir (delete it yourself, or run this again with `$env:ARR_DELETE_DATA = '1')"
		}
	}

	# ------------------------------------------------------------ Plex

	if ($doPlex) {
		Step 'Removing Plex Media Server'
		$plexProcs = Get-Process 'Plex Media Server', 'PlexScriptHost', 'Plex Tuner Service', 'Plex Update Service' -ErrorAction SilentlyContinue
		foreach ($pp in $plexProcs) {
			Do-It "stop $($pp.ProcessName)" { Stop-Process -Id $pp.Id -Force -ErrorAction SilentlyContinue }
		}
		$cmd = [string]$plex.UninstallString
		$exe = if ($cmd -match '^"([^"]+)"') { $Matches[1] } else { ($cmd -split ' ')[0] }
		Do-It "run Plex's uninstaller ($exe; Windows asks for permission)" {
			$p = Start-Process $exe -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait -PassThru
			if ($p.ExitCode -ne 0) { throw "Plex's uninstaller stopped with code $($p.ExitCode)" }
		}
		if ($plexWipe) {
			if (Test-Path $plexData) { Do-It "delete $plexData (Plex's library database and settings)" { Remove-Item -Recurse -Force $plexData } }
			if (Test-Path $plexKey) { Do-It "delete Plex's settings in the registry" { Remove-Item -Recurse -Force $plexKey } }
		} elseif (Test-Path $plexData) {
			Say "  kept Plex's data in $plexData (it's used again if you reinstall Plex)"
		}
	}

	Say ''
	Say 'Done. Your movie and TV files are untouched.'
	if ($DryRun) { Say '(This was a dry run: nothing was changed.)' }
}

try {
	Uninstall-Arr
} catch {
	Write-Host "error: $($_.Exception.Message)" -ForegroundColor Red
	if ($env:ARR_INSTALL_STRICT -eq '1') { throw }
}
