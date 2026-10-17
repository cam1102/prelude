param(
    [int]$Boot = -1
)

$ErrorActionPreference = 'Stop'

$adb = (Get-Command adb -ErrorAction SilentlyContinue).Source
if (-not $adb) {
    $bundled = Join-Path $PSScriptRoot 'adb.exe'
    if (Test-Path $bundled) {
        $adb = $bundled
    } else {
        Write-Error 'adb was not found. Install Android platform-tools and add it to your PATH, or place this script next to adb.exe.'
    }
}

function Invoke-Adb {
    & $adb @args
    if ($LASTEXITCODE -ne 0) {
        throw "adb $($args -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Get-AdbState {
    $state = & $adb get-state 2>$null
    if ($LASTEXITCODE -eq 0) { "$state".Trim() } else { '' }
}

Write-Host 'Waiting for the Car Thing (30 s)...'
for ($i = 0; $i -lt 30 -and (Get-AdbState) -ne 'device'; $i++) {
    Start-Sleep -Seconds 1
}
if ((Get-AdbState) -ne 'device') {
    & $adb devices -l
    Write-Error 'No Car Thing found over adb. Make sure it booted into recovery mode and is the only adb device connected.'
}

$timestamp = Get-Date -Format 'yyyy-MM-dd_HH-mm-ss'
$collectedAt = Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz'
$output = Join-Path (Get-Location) "prelude-logs-$timestamp.txt"
$remoteScript = '/tmp/prelude-collect-logs.sh'
$remoteOutput = '/tmp/prelude-logs.txt'
$localScript = [IO.Path]::GetTempFileName()

$collector = @'
boot="$1"
collected_at="$2"

section() {
    printf '\n===== %s =====\n' "$1"
}

run() {
    section "$*"
    "$@" 2>&1
}

{
    printf 'Prelude logs\ncollected at (host time): %s\nboot offset: %s\n' "$collected_at" "$boot"
    run date -u
    run cat /etc/os-release
    run uname -a
    run cat /proc/cmdline
    run uptime
    run journalctl --list-boots --no-pager
    run preludectl doctor --json

    section "settings"
    for file in /var/lib/prelude/settings.toml /var/lib/prelude/audio-route; do
        printf -- '--- %s ---\n' "$file"
        cat "$file" 2>&1
    done

    section "/etc/prelude/config-dev.toml (passphrase redacted)"
    sed 's/^\(passphrase[[:space:]]*=\).*/\1 "<redacted>"/' /etc/prelude/config-dev.toml 2>&1

    section "crash dumps (pstore)"
    for dir in /var/lib/systemd/pstore /sys/fs/pstore; do
        [ -d "$dir" ] || continue
        find "$dir" -type f | sort | while read -r file; do
            printf -- '--- %s ---\n' "$file"
            cat "$file" 2>&1
        done
    done

    run journalctl -b "$boot" -o short-precise --no-pager
    run systemctl --failed --no-pager
    run df -h
    run free
    run ls -laR /var/lib/prelude
    run ls -laR /var/lib/bluetooth
} > /tmp/prelude-logs.txt 2>&1
exit 0
'@

try {
    [IO.File]::WriteAllText($localScript, ($collector -replace "`r`n", "`n"), (New-Object Text.UTF8Encoding $false))

    Write-Host 'Collecting logs...'
    Invoke-Adb push $localScript $remoteScript | Out-Null
    Invoke-Adb shell sh $remoteScript $Boot "'$collectedAt'"
    Invoke-Adb pull $remoteOutput $output | Out-Null
    Invoke-Adb shell rm -f $remoteScript $remoteOutput
} finally {
    Remove-Item -Force $localScript -ErrorAction SilentlyContinue
}

Write-Host "Saved $output"
