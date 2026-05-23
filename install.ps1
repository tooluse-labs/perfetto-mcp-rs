# Install perfetto-mcp-rs and register it with Claude Code / Codex.
#
# Usage:
#   irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
#
# Environment overrides:
#   $env:INSTALL_DIR   Where to place the binary (default: $HOME\.local\bin)
#   $env:REPO          GitHub slug to download from (default: tooluse-labs/perfetto-mcp-rs)
#   $env:VERSION       Release tag to install (default: latest)
#   $env:SCOPE         Claude scope: user|local|project (default: user). For
#                      local/project, run this script from the target project dir.
#
# Written to run under both FullLanguage and ConstrainedLanguage PowerShell
# modes — uses cmdlets instead of .NET static methods wherever possible so
# enterprise AppLocker / WDAC lockdowns don't break the installer.

function Install-PerfettoMcp {
    $ErrorActionPreference = 'Stop'
    # Old Windows defaulted to TLS 1.0 which github.com rejects. Wrapped in
    # try/catch because this is a non-core static property set — CLM blocks
    # it, and Windows 10/11's TLS 1.2+ default is fine without it.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

    $repo       = if ($env:REPO)        { $env:REPO }        else { 'tooluse-labs/perfetto-mcp-rs' }
    $installDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
    $version    = if ($env:VERSION)     { $env:VERSION }     else { 'latest' }
    $asset      = 'perfetto-mcp-rs-windows-amd64.exe'
    $binName    = 'perfetto-mcp-rs.exe'

    function _info($m) { Write-Host "==> $m" }
    function _warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
    function _fail($m) { Write-Host "error: $m" -ForegroundColor Red; throw $m }
    function _download($url, $dest) {
        for ($attempt = 1; $attempt -le 4; $attempt++) {
            try {
                Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
                return
            } catch {
                if ($attempt -eq 4) {
                    throw
                }
                _warn "download attempt $attempt failed; retrying"
                Start-Sleep -Seconds (2 * $attempt)
            }
        }
    }

    # Detect real OS architecture even when running under WOW64 (32-bit PS on
    # 64-bit Windows reports 'x86' in PROCESSOR_ARCHITECTURE).
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') {
        _fail "only windows-amd64 is released; detected '$arch'"
    }

    # `releases/latest/download/<asset>` redirects to the latest release
    # without an API call, avoiding the GitHub anonymous rate limit.
    $url = if ($version -eq 'latest') {
        "https://github.com/$repo/releases/latest/download/$asset"
    } else {
        "https://github.com/$repo/releases/download/$version/$asset"
    }

    _info "installing perfetto-mcp-rs ($version) from $repo"

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $dest = Join-Path $installDir $binName

    # Sweep aside files left by prior installs. -Name makes Get-ChildItem
    # return plain filename strings instead of FileInfo objects, which
    # avoids any property access on a non-core type.
    $asides = @(Get-ChildItem -LiteralPath $installDir -Filter "$binName.old-*" -Name -ErrorAction SilentlyContinue)
    foreach ($name in $asides) {
        Remove-Item -LiteralPath (Join-Path $installDir $name) -Force -ErrorAction SilentlyContinue
    }

    # Windows holds a DELETE lock on a running .exe, so we can't overwrite
    # or unlink it — but MoveFile is allowed on locked files. Rename aside
    # so the download target is free; the aside is cleaned up next install.
    if (Test-Path -LiteralPath $dest) {
        $aside = "$dest.old-$(Get-Random)"
        try {
            Move-Item -LiteralPath $dest -Destination $aside -Force -ErrorAction Stop
        } catch {
            _fail "cannot replace $dest - is an MCP client still running it? Close Claude Code, Codex, or any other client using it and retry."
        }
    }

    try {
        _download $url $dest
    } catch {
        _fail "download failed: $url"
    }
    _info "installed to $dest"

    # Idempotent user-PATH update. CLM forbids
    # [Environment]::SetEnvironmentVariable, but the registry provider
    # cmdlets work. Side effect: we don't broadcast WM_SETTINGCHANGE, so
    # only newly launched terminals see the update — acceptable because
    # MCP registrations store the absolute path anyway.
    $current = (Get-ItemProperty -Path 'HKCU:\Environment' -Name PATH -ErrorAction SilentlyContinue).PATH
    if ($null -eq $current) { $current = '' }
    $parts = $current -split ';' | Where-Object { $_ -ne '' }
    if ($parts -contains $installDir) {
        _info "$installDir is already on your user PATH"
    } else {
        $new = if ($current) { "$current;$installDir" } else { $installDir }
        try {
            New-ItemProperty -Path 'HKCU:\Environment' -Name PATH -Value $new -PropertyType ExpandString -Force -ErrorAction Stop | Out-Null
            _info "added $installDir to your user PATH (new terminals will see it)"
        } catch {
            _warn "couldn't update your user PATH automatically; add $installDir manually"
        }
    }

    # Forward-slash form avoids backslash-escaping hazards in JSON configs
    # that Claude/Codex eventually write. The binary consumes this verbatim
    # via `--binary-path`.
    $clientPath = ($dest -replace '\\', '/')

    # Delegate Claude + Codex registration to the binary's `install`
    # subcommand (v0.8+). The binary owns CLI schemas and cache-dir
    # knowledge — keeping it out of PowerShell avoids the four-places drift
    # problem that motivated this refactor. Scope is `user` unless
    # $env:SCOPE overrides.
    $scope = if ($env:SCOPE) { $env:SCOPE } else { 'user' }
    $savedEAP = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    $registerExit = 1
    try {
        & $dest install --scope $scope --binary-path $clientPath
        $registerExit = $LASTEXITCODE
    } catch {
        $registerExit = 1
    }
    $ErrorActionPreference = $savedEAP
    if ($registerExit -eq 0) {
        _info "self-registered with available CLIs (scope=$scope)."
    } else {
        _warn "binary self-install reported issues (exit $registerExit); check output above."
    }
}

Install-PerfettoMcp
