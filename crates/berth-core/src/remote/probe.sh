# Berth's look at a host before its server is offered (D55). POSIX sh. It
# reads and prints; it creates, changes and starts nothing.
os=$(uname -s 2>/dev/null)
arch=$(uname -m 2>/dev/null)
data=${1:-}
if [ -z "$data" ]; then
  # A Mac's own Berth app keeps its local server in Application
  # Support/berth; a host install never shares that directory.
  if [ "$os" = Darwin ]; then
    data="$HOME/Library/Application Support/berth-host"
  elif [ -n "${XDG_DATA_HOME:-}" ]; then
    data="$XDG_DATA_HOME/berth"
  else
    data="$HOME/.local/share/berth"
  fi
fi
echo "berth-probe=1"
echo "os=$os"
echo "arch=$arch"
echo "home=$HOME"
echo "data=$data"
for dir in "$data"/bin/*/; do
  if [ -x "${dir}berth-server" ]; then
    echo "build=$(basename "$dir")"
  fi
done
if command -v sha256sum >/dev/null 2>&1; then
  echo "sha=sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  echo "sha=shasum"
else
  echo "sha=none"
fi
if command -v gzip >/dev/null 2>&1; then echo "gzip=yes"; else echo "gzip=no"; fi
# Whether logind ends this user's processes when their last login goes
# (KillUserProcesses), and whether lingering is on, which exempts them.
# Asked of logind itself, and of its configuration where it cannot be asked.
kills=unknown
linger=unknown
if command -v busctl >/dev/null 2>&1; then
  case "$(busctl get-property org.freedesktop.login1 /org/freedesktop/login1 \
    org.freedesktop.login1.Manager KillUserProcesses 2>/dev/null)" in
    "b true") kills=yes ;;
    "b false") kills=no ;;
  esac
fi
if [ "$kills" = unknown ]; then
  conf=$(cat /etc/systemd/logind.conf /etc/systemd/logind.conf.d/*.conf 2>/dev/null |
    grep -i "^[[:space:]]*KillUserProcesses[[:space:]]*=" | tail -n 1 | tr -d "[:space:]")
  case "$conf" in
    *=yes | *=true | *=1 | *=on) kills=yes ;;
    *=no | *=false | *=0 | *=off) kills=no ;;
  esac
fi
if command -v loginctl >/dev/null 2>&1; then
  case "$(loginctl show-user "$(id -u)" -p Linger 2>/dev/null)" in
    Linger=yes) linger=yes ;;
    Linger=no) linger=no ;;
  esac
fi
echo "kills=$kills"
echo "linger=$linger"
sessions=0
for pid in "$data"/run/w*.pid; do
  if [ -r "$pid" ] && kill -0 "$(cat "$pid")" 2>/dev/null; then
    sessions=$((sessions + 1))
  fi
done
echo "sessions=$sessions"
echo "berth-probe=end"
