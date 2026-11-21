# Install or upgrade hars on Windows with one command, in PowerShell:
#
#   irm https://raw.githubusercontent.com/adamt94/hars/main/install.ps1 | iex
#
# It checks for Docker Desktop (and offers to install it), downloads hars,
# starts it in a container named `hars` that starts again after a reboot,
# waits until it answers and opens it in your browser. Run it again to
# upgrade: your data and settings stay.
#
# Settings, as environment variables set before running it:
#   $env:HARS_DIR    where hars keeps its data (default: $HOME\hars)
#   $env:HARS_PORT   the web port (default: 8123, or the next free one)
#   $env:HARS_IMPORT a copy of a Home Assistant 2026.6 to 2026.11 config
#                    folder to bring over before hars starts (see the README)
#   $env:HARS_YES=1  answer yes to every question
#   $env:HARS_IMAGE  the image to run (default: the published one)
#
# It never calls `exit`: under `| iex` that would close your window.

function Install-Hars {
    $ErrorActionPreference = 'Continue'
    $image = if ($env:HARS_IMAGE) { $env:HARS_IMAGE } else { 'ghcr.io/adamt94/hars:latest' }
    $dir = if ($env:HARS_DIR) { $env:HARS_DIR } else { Join-Path $HOME 'hars' }
    $import = $env:HARS_IMPORT
    if ($env:HARS_PORT -and $env:HARS_PORT -notmatch '^\d+$') { Fail "HARS_PORT must be a number, not '$($env:HARS_PORT)'"; return }

    function Say($msg) { Write-Host "==> $msg" }
    function Fail($msg) { Write-Host "Error: $msg" -ForegroundColor Red }
    function Ask($question) {
        if ($env:HARS_YES -eq '1') { return $true }
        $reply = Read-Host "$question [Y/n]"
        return ($reply -eq '' -or $reply -match '^(y|yes)$')
    }
    function DockerReady {
        if (-not (Get-Command docker -ErrorAction SilentlyContinue)) { return $false }
        & docker info *> $null
        return ($LASTEXITCODE -eq 0)
    }
    function Answers($port) {
        try {
            Invoke-WebRequest "http://127.0.0.1:$port/" -UseBasicParsing -TimeoutSec 2 -ErrorAction Stop | Out-Null
            return $true
        } catch {
            # An error status is still an answer.
            return ($null -ne $_.Exception.Response)
        }
    }
    # Docker Desktop reports a bind mount as C:\x, /run/desktop/mnt/host/c/x
    # or /host_mnt/c/x: compare them as c:/x.
    function NormPath($p) {
        if (-not $p) { return $null }
        $p = ($p -replace '\\', '/').TrimEnd('/').ToLowerInvariant()
        if ($p -match '^/(?:run/desktop/mnt/host|host_mnt|mnt)/([a-z])(/.*)?$') { $p = $Matches[1] + ':' + $Matches[2] }
        return $p
    }
    function PortFree($port) {
        $client = New-Object System.Net.Sockets.TcpClient
        try { $client.Connect('127.0.0.1', $port); return $false } catch { return $true } finally { $client.Close() }
    }

    if ($import) {
        if (-not (Test-Path (Join-Path $import '.storage'))) {
            Fail "HARS_IMPORT must be a copy of Home Assistant's config folder (the one with .storage in it): $import"
            return
        }
        $import = (Resolve-Path $import).Path
    }

    # Docker Desktop.
    $desktop = Join-Path $env:ProgramFiles 'Docker\Docker\Docker Desktop.exe'
    $env:Path += ";" + (Join-Path $env:ProgramFiles 'Docker\Docker\resources\bin')
    if (-not (DockerReady)) {
        if (-not (Test-Path $desktop)) {
            if ((Get-Command winget -ErrorAction SilentlyContinue) -and
                (Ask 'hars runs in Docker Desktop, which isn''t installed. Install it now?')) {
                Say 'Installing Docker Desktop'
                & winget install -e --id Docker.DockerDesktop --accept-source-agreements --accept-package-agreements
                if (-not (Test-Path $desktop)) { Fail 'Docker Desktop did not install; its output is above.'; return }
                Say 'Docker Desktop is installed. If Windows asks you to restart, do that, then run this command again.'
            } else {
                Fail 'hars runs in Docker. Install Docker Desktop from https://www.docker.com/products/docker-desktop/ then run this command again.'
                return
            }
        }
        Say 'Starting Docker Desktop (accept its terms if it asks; this can take a few minutes)'
        Start-Process $desktop
        $ready = $false
        for ($i = 0; $i -lt 90 -and -not $ready; $i++) { Start-Sleep 2; $ready = DockerReady }
        if (-not $ready) {
            Fail 'Docker Desktop did not start. Open it from the Start menu, wait until it says it''s running, then run this command again.'
            return
        }
    }

    Say 'Downloading hars'
    & docker pull $image
    if ($LASTEXITCODE -ne 0) {
        & docker image inspect $image *> $null
        if ($LASTEXITCODE -ne 0) { Fail "couldn't download $image. Check your internet connection and run this again."; return }
        Say 'Couldn''t download a new version; using the one already here'
    }

    New-Item -ItemType Directory -Force (Join-Path $dir 'config') | Out-Null
    $dir = (Resolve-Path $dir).Path
    $config = Join-Path $dir 'config'

    # An earlier install from this folder is replaced; its data stays. Never
    # take over a `hars` container that keeps its data somewhere else.
    # Plain JSON: Windows PowerShell mangles quotes inside a -f template.
    $replace = $false
    $runPort = $null
    $json = & docker inspect hars 2> $null
    if ($LASTEXITCODE -eq 0) {
        $info = @($json | Out-String | ConvertFrom-Json)[0]
        $old = ($info.Mounts | Where-Object { $_.Destination -eq '/config' } | Select-Object -First 1).Source
        $same = $old -and ((NormPath $old) -eq (NormPath $config))
        if (-not $same) {
            Fail "a container named hars already exists and keeps its data in $old. To upgrade it, set `$env:HARS_DIR to the folder that holds that config folder and run this again. To start fresh instead, remove it first: docker rm -f hars"
            return
        }
        if ($import) {
            Fail "hars is already installed in $dir, and HARS_IMPORT only goes into a new install. Run Remove-Item Env:HARS_IMPORT and the install command again, or import by hand: see docs/INSTALL.md, `"Importing by hand`"."
            return
        }
        $replace = $true
        # The port hars holds while it runs (hars.env may have been edited since).
        if ($info.State.Running) { foreach ($e in @($info.Config.Env)) { if ($e -match '^HARS_BIND=.*:(\d+)$') { $runPort = $Matches[1] } } }
    }

    # Check the port before stopping anything.
    $envFile = Join-Path $dir 'hars.env'
    $oldPort = $null
    if (Test-Path $envFile) {
        $m = Select-String -Path $envFile -Pattern '^HARS_BIND=.*:(\d+)\s*$' | Select-Object -First 1
        if ($m) { $oldPort = $m.Matches[0].Groups[1].Value }
    }
    $port = if ($env:HARS_PORT) { $env:HARS_PORT } else { $oldPort }
    if (-not $port) {
        $port = (8123..8133 | Where-Object { PortFree $_ } | Select-Object -First 1)
        if (-not $port) { Fail 'ports 8123 to 8133 are all in use; pick one with $env:HARS_PORT'; return }
        if ($port -ne 8123) { Say "Port 8123 is in use (Home Assistant?), so hars uses $port" }
    } elseif (-not $replace -or "$port" -ne "$runPort") {
        # The running hars holds its own port; any other must be free.
        if (-not (PortFree ([int]$port))) {
            Fail "port $port is in use by another program (Home Assistant?). Stop it, or pick another port with `$env:HARS_PORT."
            return
        }
    }

    # LF line endings: Docker reads a carriage return as part of the value.
    if (Test-Path $envFile) {
        $text = [IO.File]::ReadAllText($envFile) -replace "`r", ''
        if ($text -match '(?m)^HARS_BIND=') {
            $text = $text -replace '(?m)^HARS_BIND=.*$', "HARS_BIND=0.0.0.0:$port"
        } else {
            $text = $text.TrimEnd("`n") + "`nHARS_BIND=0.0.0.0:$port`n"
        }
    } else {
        $text = "# Settings for hars. Change them, then run the installer again.`n" +
            "# The port hars listens on.`nHARS_BIND=0.0.0.0:$port`n" +
            "# More detail in the logs: uncomment the next line.`n#RUST_LOG=debug`n"
    }
    [IO.File]::WriteAllText($envFile, $text)

    if ($replace) {
        # `docker stop` gives hars its minute to save; `rm -f` would kill it.
        Say 'Stopping the running hars (your data stays)'
        & docker stop hars | Out-Null
        if ($LASTEXITCODE -ne 0) { Fail "couldn't stop the running hars; it is left as it was."; return }
        & docker rm hars | Out-Null
    }

    # Before hars first starts, so the import doesn't meet its new files.
    if ($import) {
        Say "Bringing over Home Assistant's setup from $import"
        & docker run --rm -v "${config}:/config" -v "${import}:/ha:ro" $image import /ha
        if ($LASTEXITCODE -ne 0) {
            Fail 'the import stopped; its message is above. hars is not started. Your Home Assistant copy is unchanged.'
            return
        }
    }

    # Docker Desktop can't share the host's network, so publish the port.
    Say 'Starting hars'
    & docker run -d --name hars --restart unless-stopped --stop-timeout 60 `
        --env-file $envFile -v "${config}:/config" -p "${port}:${port}" $image | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail 'Docker could not start hars; its message is above.'; return }

    $up = $false
    for ($i = 0; $i -lt 60 -and -not $up; $i++) { $up = Answers $port; if (-not $up) { Start-Sleep 1 } }
    if (-not $up) {
        & docker logs --tail 30 hars
        Fail "hars didn't answer on port $port within a minute. Its log is above."
        return
    }

    $url = "http://localhost:$port"
    $ip = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { "$($_.PrefixOrigin)" -in 'Dhcp', 'Manual' -and $_.InterfaceAlias -notmatch 'vEthernet|WSL|Loopback' } |
        Select-Object -First 1 -ExpandProperty IPAddress
    Write-Host ''
    Write-Host 'hars is running.' -ForegroundColor Green
    Write-Host ''
    if ($ip) { Write-Host "  Open  http://${ip}:$port  (on this computer: $url)" } else { Write-Host "  Open  $url" }
    if ($import) {
        Write-Host '  and log in as you did on Home Assistant.'
        Write-Host "  What came across: $config\import-report.json"
    } else {
        Write-Host '  and create your account.'
    }
    Write-Host ''
    Write-Host "  Your data:  $config"
    Write-Host '  Logs:       docker logs -f hars'
    Write-Host '  Stop/start: docker stop hars  /  docker start hars'
    Write-Host '  Upgrade:    run this installer again'
    Write-Host ''
    Start-Process $url
}

Install-Hars
