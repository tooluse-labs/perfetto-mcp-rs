#!/usr/bin/env sh
# Install perfetto-mcp-rs and register it with Claude Code / Codex.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh
#
# Pin a specific version (recommended over the env-var form — it survives
# the `VAR=value curl … | sh` shell-pipe pitfall, which only sets VAR for
# `curl`, not the piped `sh`):
#
#   curl -fsSL …install.sh | sh -s -- --version v0.10.0
#
# Environment overrides (read if no equivalent flag is passed; flag wins):
#   INSTALL_DIR   Where to place the binary (default: $HOME/.local/bin)
#   REPO          GitHub slug to download from (default: tooluse-labs/perfetto-mcp-rs)
#   VERSION       Release tag to install (default: latest)
#   SCOPE         Claude scope: user|local|project (default: user). For
#                 local/project, run this script from the target project dir.

set -eu

# Argv parsing FIRST so flags can override env vars. Pattern follows
# rustup / pnpm — `sh -s -- --flag value` is the canonical "shell
# installer" form that sidesteps the shell-pipe env-var pitfall.
while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || { printf 'error: --version requires a tag argument\n' >&2; exit 1; }
      VERSION="$2"; shift 2 ;;
    --version=*)
      VERSION="${1#--version=}"; shift ;;
    -V)
      [ $# -ge 2 ] || { printf 'error: -V requires a tag argument\n' >&2; exit 1; }
      VERSION="$2"; shift 2 ;;
    -h|--help)
      cat <<'USAGE'
Usage: install.sh [--version <tag>] [-V <tag>] [-h|--help]

Pin a specific release with --version or -V. Without a flag, the script
honors the VERSION env var (if set), else installs the latest release.

Other env overrides: INSTALL_DIR, REPO, SCOPE. See script header.
USAGE
      exit 0 ;;
    *)
      printf 'warning: ignoring unknown arg %s\n' "$1" >&2; shift ;;
  esac
done

: "${REPO:=tooluse-labs/perfetto-mcp-rs}"
: "${INSTALL_DIR:=${HOME}/.local/bin}"
: "${VERSION:=latest}"
# Claude scope to register under: user | local | project. Codex ignores scope
# (it has no scope concept). For local/project, run install.sh from the
# target project directory.
: "${SCOPE:=user}"

BIN_NAME="perfetto-mcp-rs"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
info() { printf '==> %s\n' "$1"; }
warn() { printf 'warning: %s\n' "$1" >&2; }

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "$1 is required but not installed"
}

detect_platform() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "$os" in
    linux)                 os_tag="linux" ;;
    darwin)                os_tag="darwin" ;;
    msys*|mingw*|cygwin*)  os_tag="windows" ;;
    *) err "unsupported OS: $os" ;;
  esac
  case "$arch" in
    x86_64|amd64)  arch_tag="amd64" ;;
    aarch64|arm64) arch_tag="arm64" ;;
    *) err "unsupported architecture: $arch" ;;
  esac
  if [ "$os_tag" = "windows" ] && [ "$arch_tag" != "amd64" ]; then
    err "only windows-amd64 is released; got $arch_tag"
  fi
  printf '%s-%s' "$os_tag" "$arch_tag"
}

resolve_version() {
  if [ "$VERSION" != "latest" ]; then
    printf '%s' "$VERSION"
    return
  fi
  # Best-effort tag lookup for the display message only — the download
  # itself uses `releases/latest/download/<asset>` (no API call), so we
  # tolerate failure here. The GitHub anonymous API is rate-limited to
  # 60 req/hour per IP; corporate NAT / public Wi-Fi can hit the cap and
  # the old code aborted with "could not resolve release tag" even
  # though no API call was strictly necessary.
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
    | grep -o '"tag_name":[[:space:]]*"[^"]*"' \
    | head -1 \
    | sed 's/.*"\([^"]*\)"$/\1/' \
    || true
}

add_to_user_path_windows() {
  # Append INSTALL_DIR to the user-level Windows PATH via PowerShell. The
  # .NET SetEnvironmentVariable call writes to HKCU\Environment and
  # broadcasts WM_SETTINGCHANGE, so new processes see the update without a
  # reboot.
  dir_msys="$1"
  dir_win="$(cygpath -m "$dir_msys")"
  if ! command -v powershell.exe >/dev/null 2>&1; then
    warn "powershell.exe not found; add ${dir_win} to PATH manually"
    return
  fi
  result="$(PERFETTO_TARGET_DIR="$dir_win" powershell.exe -NoProfile -NonInteractive -Command '
    $target = $env:PERFETTO_TARGET_DIR
    $current = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($null -eq $current) { $current = "" }
    $parts = $current -split ";" | Where-Object { $_ -ne "" }
    if ($parts -contains $target) { Write-Output "already"; exit 0 }
    $new = if ($current) { "$current;$target" } else { $target }
    [Environment]::SetEnvironmentVariable("PATH", $new, "User")
    Write-Output "added"
  ' 2>/dev/null | tr -d '\r')"
  case "$result" in
    already) info "${dir_win} is already on your Windows user PATH" ;;
    added)   info "Added ${dir_win} to your Windows user PATH (new terminals and apps will see it)" ;;
    *)       warn "Failed to update Windows PATH; add ${dir_win} manually via System Properties → Environment Variables" ;;
  esac
}

download_file() {
  url="$1"
  dest="$2"
  platform="$3"
  curl_retry_args="-fsSL --retry 5 --retry-delay 2 --connect-timeout 20 --max-time 300"
  if curl --help all 2>/dev/null | grep -q -- '--retry-all-errors'; then
    curl_retry_args="${curl_retry_args} --retry-all-errors"
  fi

  # shellcheck disable=SC2086
  if curl $curl_retry_args -o "$dest" "$url"; then
    return 0
  fi

  case "$platform" in
    windows-*)
      warn "curl download failed; retrying with Windows SSL revocation checks disabled"
      # shellcheck disable=SC2086
      if curl $curl_retry_args --ssl-no-revoke -o "$dest" "$url"; then
        return 0
      fi
      if command -v powershell.exe >/dev/null 2>&1; then
        warn "curl download still failed; retrying with PowerShell"
        dest_win="$dest"
        if command -v cygpath >/dev/null 2>&1; then
          dest_win="$(cygpath -m "$dest")"
        fi
        if PERFETTO_DOWNLOAD_URL="$url" PERFETTO_DOWNLOAD_DEST="$dest_win" \
          powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command '
            $ErrorActionPreference = "Stop"
            try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}
            $url = $env:PERFETTO_DOWNLOAD_URL
            $dest = $env:PERFETTO_DOWNLOAD_DEST
            for ($attempt = 1; $attempt -le 4; $attempt++) {
              try {
                Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
                exit 0
              } catch {
                if ($attempt -eq 4) {
                  Write-Error $_
                  exit 1
                }
                Start-Sleep -Seconds (2 * $attempt)
              }
            }
          '; then
          return 0
        fi
      fi
      ;;
  esac

  return 1
}

main() {
  need_cmd curl
  need_cmd uname
  need_cmd install

  platform="$(detect_platform)"
  asset="${BIN_NAME}-${platform}"
  bin_file="${BIN_NAME}"
  case "$platform" in
    windows-*)
      asset="${asset}.exe"
      bin_file="${BIN_NAME}.exe"
      ;;
  esac

  if [ "$VERSION" = "latest" ]; then
    info "Detecting latest release from github.com/${REPO}"
  else
    info "Installing pinned release ${VERSION} from github.com/${REPO}"
  fi
  tag="$(resolve_version)"
  display_tag="${tag:-latest}"
  info "Installing ${BIN_NAME} ${display_tag} (${platform})"

  # Latest path uses the GitHub redirect (no API call → no rate limit).
  # Pinned path uses the direct download URL with the user-supplied tag.
  if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${asset}"
  else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
  fi
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT INT TERM
  download_file "$url" "$tmp" "$platform" \
    || err "download failed: ${url}"

  mkdir -p "$INSTALL_DIR"

  # On Windows a running .exe is locked against unlink+create, which is what
  # `install` does. MoveFile (mv) is allowed on locked files, so rename the
  # existing binary aside first and let the new one land at the canonical
  # path. The aside is cleaned up on the next install, once the old MCP
  # client subprocess has exited.
  case "$platform" in
    windows-*)
      rm -f "${INSTALL_DIR}/${bin_file}".old-* 2>/dev/null || true
      if [ -e "${INSTALL_DIR}/${bin_file}" ]; then
        ts="$(date +%s)"
        mv "${INSTALL_DIR}/${bin_file}" "${INSTALL_DIR}/${bin_file}.old-${ts}" 2>/dev/null \
          || err "cannot replace ${INSTALL_DIR}/${bin_file} (is an MCP client still running it? close it and retry)"
      fi
      ;;
  esac

  install -m 0755 "$tmp" "${INSTALL_DIR}/${bin_file}"
  info "Installed to ${INSTALL_DIR}/${bin_file}"

  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      case "$platform" in
        windows-*)
          add_to_user_path_windows "$INSTALL_DIR"
          ;;
        *)
          cat <<EOF

NOTE: ${INSTALL_DIR} is not on your PATH. Add this to your shell rc:

    export PATH="${INSTALL_DIR}:\$PATH"

EOF
          ;;
      esac
      ;;
  esac

  # Delegate Claude/Codex registration + cache accounting to the freshly-
  # installed binary's `install` subcommand (v0.8+). Binary owns the CLI
  # schemas + dirs::data_local_dir() knowledge so the shell doesn't have
  # to mirror it.
  #
  # Windows POSIX shells expose INSTALL_DIR as `/c/Users/...`; claude/codex
  # need Windows-form paths, and the binary doesn't have cygpath — so the
  # wrapper converts here, on this side of the --binary-path handoff.
  register_path_native="${INSTALL_DIR}/${bin_file}"
  case "$platform" in
    windows-*)
      if command -v cygpath >/dev/null 2>&1; then
        register_path_native="$(cygpath -m "$register_path_native")"
      fi
      ;;
  esac

  # SCOPE env var (default `user`) plumbs through as --scope. For
  # --scope local|project the caller must invoke install.sh from the
  # target project directory.
  if "${INSTALL_DIR}/${bin_file}" install \
       --scope "$SCOPE" \
       --binary-path "$register_path_native"; then
    info "Self-registered with available CLIs."
  else
    warn "binary self-install reported issues; check output above."
  fi
}

main "$@"
