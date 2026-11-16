#!/bin/sh
# Installs vaultsync for this user (no root needed) and runs it as a user
# service: launchd on macOS, systemd --user on Linux.
#
#   install.sh <join code or link> [--folder PATH] [--machine NAME]
#       join a vault; the folder defaults to the inviting machine's
#   install.sh --init PATH [--machine NAME]
#       make PATH a vault (keeping any git history it has), as its first machine
#   install.sh
#       upgrade the binary and restart the service
#
# One service runs every vault this user has made or joined
# (`vaultsync run --all`, listed in ~/.config/vaultsync/vaults).
#   install.sh --uninstall
#       stop and remove the service; the binary and the vault stay
set -eu

BASE=${VAULTSYNC_DOWNLOAD:-https://github.com/plow-pbc/vaultsync/releases/latest/download}
BIN_DIR=${VAULTSYNC_BIN_DIR:-$HOME/.local/bin}
BIN=$BIN_DIR/vaultsync
NAME=vaultsync
PLIST=$HOME/Library/LaunchAgents/$NAME.plist
UNIT=$HOME/.config/systemd/user/$NAME.service

die() { echo "install.sh: $*" >&2; exit 1; }
say() { echo "==> $*"; }

code='' folder='' machine='' init='' uninstall=''
while [ $# -gt 0 ]; do
    case $1 in
        --folder) folder=${2:?--folder needs a path}; shift 2 ;;
        --machine) machine=${2:?--machine needs a name}; shift 2 ;;
        --init) init=${2:?--init needs a path}; shift 2 ;;
        --uninstall) uninstall=1; shift ;;
        -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) die "unknown option $1" ;;
        *) code=$1; shift ;;
    esac
done

case $(uname -s) in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) die "unsupported system $(uname -s)" ;;
esac

stop_service() {
    if [ $os = macos ]; then
        launchctl bootout "gui/$(id -u)/$NAME" 2>/dev/null || true
    else
        systemctl --user disable --now "$NAME" 2>/dev/null || true
    fi
}

if [ -n "$uninstall" ]; then
    stop_service
    rm -f "$PLIST" "$UNIT"
    if [ $os = linux ]; then systemctl --user daemon-reload 2>/dev/null || true; fi
    say "Service removed. The binary ($BIN) and your vault are untouched."
    exit 0
fi

case $(uname -s)-$(uname -m) in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    *) die "no build for $(uname -s) $(uname -m)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# Downloads a release asset and its checksum into $tmp, and verifies it.
fetch() {
    for f in "$1" "$1.sha256"; do
        curl -fsSL "$BASE/$f" -o "$tmp/$f" || return 1
    done
    if command -v sha256sum >/dev/null; then
        (cd "$tmp" && sha256sum -c "$1.sha256" >/dev/null)
    else
        (cd "$tmp" && shasum -a 256 -c "$1.sha256" >/dev/null)
    fi
}

say "Downloading vaultsync for $target"
asset=vaultsync-$target.tar.gz
fetch "$asset" || die "could not download and verify $BASE/$asset"
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 755 "$tmp/vaultsync" "$BIN"
say "Installed $("$BIN" --version) at $BIN"

# A service from before `run --all` names its one vault: list it first, so
# it stays synced whatever this run adds.
legacy=$(sed -n -e 's|.*<string>--vault</string><string>\(.*\)</string><string>run</string>.*|\1|p' \
    -e 's|^ExecStart=".*" --vault "\(.*\)" run$|\1|p' "$PLIST" "$UNIT" 2>/dev/null | head -1)
if [ -n "$legacy" ]; then "$BIN" --vault "$legacy" track; fi

# Joining or making a vault adds it to the service's list.
if [ -n "$code" ]; then
    # Drop join's "start syncing with vaultsync run" hint: the service does that.
    out=$("$BIN" join "$code" ${folder:+--folder "$folder"} ${machine:+--machine "$machine"})
    echo "$out" | sed 's/^Joined\. .*/Joined./'
elif [ -n "$init" ]; then
    "$BIN" init "$init" ${machine:+--machine "$machine"}
elif [ -f "$PLIST" ] || [ -f "$UNIT" ] \
    || [ -n "$(ls -A "$HOME/.config/vaultsync/vaults" 2>/dev/null)" ]; then
    : # an upgrade, or vaults joined without the service, which runs that list
else
    say "Binary installed. To sync a vault, run this again with a join code or --init PATH."
    exit 0
fi

# The binary's path goes verbatim into a plist and a unit file.
case $BIN in
    *[!A-Za-z0-9._/@+,:\ -]*) die "unsupported characters in '$BIN'; set VAULTSYNC_BIN_DIR to a plain path" ;;
esac

if [ $os = macos ]; then
    log=$HOME/Library/Logs/$NAME.log
    mkdir -p "$(dirname "$PLIST")" "$(dirname "$log")"
    cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$NAME</string>
    <key>ProgramArguments</key>
    <array><string>$BIN</string><string>run</string><string>--all</string></array>
    <key>EnvironmentVariables</key>
    <dict><key>PATH</key><string>/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin</string></dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$log</string>
    <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
EOF
    stop_service
    launchctl bootstrap "gui/$(id -u)" "$PLIST"
    say "Running as launchd agent '$NAME'; log: ~/Library/Logs/$NAME.log"
    # The menu-bar app shows what the service syncs. A download by curl
    # carries no quarantine flag, so macOS opens it however it is signed.
    # Without it a Mac has no sign of syncing, so failing to get it is an error.
    app="$HOME/Applications/BrainSync by Plow.app"
    if fetch BrainSync-by-Plow.dmg; then
        hdiutil attach -quiet -nobrowse -readonly -mountpoint "$tmp/dmg" "$tmp/BrainSync-by-Plow.dmg"
        osascript -e 'quit app id "co.plow.vaultsync"' 2>/dev/null || true
        mkdir -p "$HOME/Applications" && rm -rf "$app"
        ditto "$tmp/dmg/BrainSync by Plow.app" "$app"
        hdiutil detach -quiet "$tmp/dmg"
        open "$app"
        say "Installed the BrainSync menu-bar app in ~/Applications"
    else
        die "syncing is set up, but the BrainSync app couldn't be downloaded from $BASE; run this again"
    fi
else
    mkdir -p "$(dirname "$UNIT")"
    cat > "$UNIT" <<EOF
[Unit]
Description=vaultsync: keeps this user's vaults in sync
After=network-online.target

[Service]
ExecStart="$BIN" run --all
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable "$NAME" >/dev/null 2>&1
    systemctl --user restart "$NAME"
    # Lingering starts user services at boot, before anyone logs in.
    if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]; then
        loginctl enable-linger "$USER" 2>/dev/null \
            || echo "install.sh: couldn't enable lingering; vaultsync will run only while you're logged in" >&2
    fi
    say "Running as systemd user service '$NAME'; log: journalctl --user -u $NAME"
fi
