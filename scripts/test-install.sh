#!/bin/sh
# Runs install.sh's macOS upgrade against stand-ins for launchctl, hdiutil and
# the release downloads, on any system.
#
# The stand-in launchctl behaves as launchd does when the old service takes a
# moment to exit: bootout returns at once, the service stays listed until it
# has exited, and bootstrap fails with error 5 until then.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
mkdir -p "$t/bin" "$t/dl" "$t/pkg"

# stub <command>: makes the script on stdin stand in for <command>.
stub() { { echo '#!/bin/sh'; cat; } > "$t/bin/$1"; chmod +x "$t/bin/$1"; }
stub uname <<'EOF'
case $1 in -s) echo Darwin ;; -m) echo arm64 ;; esac
EOF
stub open < /dev/null
stub osascript < /dev/null
stub ditto <<'EOF'
cp -R "$1" "$2"
EOF
stub hdiutil <<'EOF'
case $1 in attach)
    while [ "$1" != -mountpoint ]; do shift; done
    mkdir -p "$2/WikiSync by Plow.app" ;;
esac
EOF
# $STATE holds "loaded", or "exiting N": still listed for N more prints.
# $BOOTSTRAP_BROKEN makes every bootstrap fail.
stub launchctl <<'EOF'
case $1 in
bootout)
    [ -f "$STATE" ] || exit 3
    echo "exiting 3" > "$STATE" ;;
print)
    [ -f "$STATE" ] || exit 113
    read -r what left < "$STATE"
    if [ "$what" = exiting ]; then
        if [ "$left" -le 1 ]; then rm "$STATE"; else echo "exiting $((left - 1))" > "$STATE"; fi
    fi ;;
bootstrap)
    if [ -f "$STATE" ] || [ -n "${BOOTSTRAP_BROKEN:-}" ]; then
        echo "Bootstrap failed: 5: Input/output error" >&2; exit 5
    fi
    echo loaded > "$STATE" ;;
esac
EOF

printf '#!/bin/sh\necho vaultsync 0.0.0-test\n' > "$t/pkg/vaultsync"
chmod +x "$t/pkg/vaultsync"
tar -czf "$t/dl/vaultsync-aarch64-apple-darwin.tar.gz" -C "$t/pkg" vaultsync
echo dmg > "$t/dl/WikiSync-by-Plow.dmg"
for f in vaultsync-aarch64-apple-darwin.tar.gz WikiSync-by-Plow.dmg; do
    if command -v sha256sum >/dev/null; then
        (cd "$t/dl" && sha256sum "$f" > "$f.sha256")
    else
        (cd "$t/dl" && shasum -a 256 "$f" > "$f.sha256")
    fi
done

failures=0
# upgrade <name> <expected exit: 0 or 1> [VAR=value...]
# Upgrades over a running service and checks it ends up running with the app
# installed, or, when it can't restart, that install.sh says so and fails.
upgrade() {
    name=$1 want=$2; shift 2
    home="$t/home-$name"
    mkdir -p "$home/Library/LaunchAgents"
    echo old > "$home/Library/LaunchAgents/vaultsync.plist"
    echo loaded > "$t/state-$name"
    got=0
    env HOME="$home" PATH="$t/bin:$PATH" STATE="$t/state-$name" \
        VAULTSYNC_DOWNLOAD="file://$t/dl" "$@" \
        sh "$repo/install.sh" > "$t/out-$name" 2>&1 || got=$?
    problems=''
    [ "$got" -eq "$want" ] || problems="exit $got, wanted $want;"
    [ -d "$home/Applications/WikiSync by Plow.app" ] || problems="$problems app not installed;"
    if [ "$want" -eq 0 ]; then
        [ "$(cat "$t/state-$name" 2>/dev/null)" = loaded ] || problems="$problems service not running;"
    else
        grep -q "sync is not running" "$t/out-$name" || problems="$problems no clear message;"
    fi
    if [ -n "$problems" ]; then
        echo "FAIL $name: $problems"; sed 's/^/    /' "$t/out-$name"
        failures=$((failures + 1))
    else
        echo "ok   $name"
    fi
}

upgrade restarts-a-slow-exiting-service 0
upgrade reports-a-service-that-wont-start 1 BOOTSTRAP_BROKEN=1

[ "$failures" -eq 0 ] || { echo "$failures failed"; exit 1; }
echo "all passed"
