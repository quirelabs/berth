#!/bin/sh
# Berth probe. POSIX sh, no dependencies beyond coreutils, awk and optionally
# tmux. Emits one line of JSON per mode. Runs on Debian, Ubuntu, Alpine,
# Fedora and macOS. Delete it with: sh probe.sh remove

set -u
BERTH_DIR="${HOME}/.berth"
UNAME="$(uname -s 2>/dev/null || echo unknown)"

# Escape a string for embedding in a JSON double-quoted value.
esc() {
  printf '%s' "$1" | awk 'BEGIN { RS = "\0" }
    { gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); gsub(/\t/, "\\t")
      gsub(/\r/, ""); gsub(/\n/, "\\n"); printf "%s", $0 }'
}

have() { command -v "$1" >/dev/null 2>&1; }

os_name() {
  if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release 2>/dev/null
    printf '%s' "${PRETTY_NAME:-${NAME:-$UNAME}}"
  elif [ "$UNAME" = "Darwin" ]; then
    printf 'macOS %s' "$(sw_vers -productVersion 2>/dev/null || echo '')"
  else
    printf '%s' "$UNAME"
  fi
}

uptime_seconds() {
  if [ -r /proc/uptime ]; then
    awk '{ printf "%d", $1 }' /proc/uptime
  elif [ "$UNAME" = "Darwin" ]; then
    boot=$(sysctl -n kern.boottime 2>/dev/null | sed -n 's/.*sec = \([0-9]*\).*/\1/p')
    now=$(date +%s)
    [ -n "$boot" ] && echo $((now - boot)) || echo 0
  else
    echo 0
  fi
}

load_avg() {
  if [ -r /proc/loadavg ]; then
    awk '{ printf "[%s,%s,%s]", $1, $2, $3 }' /proc/loadavg
  elif have sysctl; then
    sysctl -n vm.loadavg 2>/dev/null |
      awk '{ printf "[%s,%s,%s]", $2, $3, $4 }'
  else
    printf '[0,0,0]'
  fi
}

# Prints "total used" in bytes. MemAvailable is absent on older kernels and on
# BusyBox, so fall back to free + buffers + cached.
memory() {
  if [ -r /proc/meminfo ]; then
    awk '/^MemTotal:/ { total = $2 } /^MemAvailable:/ { avail = $2; seen = 1 }
      /^MemFree:/ { free = $2 } /^Buffers:/ { buffers = $2 } /^Cached:/ { cached = $2 }
      END { if (!seen) avail = free + buffers + cached
            printf "%d %d", total * 1024, (total - avail) * 1024 }' /proc/meminfo
  elif [ "$UNAME" = "Darwin" ]; then
    total=$(sysctl -n hw.memsize 2>/dev/null || echo 0)
    used=$(vm_stat 2>/dev/null | awk '/page size of/ { ps = $8 }
      /Pages active/ { active = $3 } /Pages wired down/ { wired = $4 }
      END { if (ps == "") ps = 4096; gsub(/\./, "", active); gsub(/\./, "", wired)
            printf "%d", (active + wired) * ps }')
    printf '%d %d' "$total" "${used:-0}"
  else
    printf '0 0'
  fi
}

disks() {
  # -P forces POSIX single-line output; -k normalises to 1K blocks. Fields are
  # read from the right because device names may contain spaces.
  df -Pk 2>/dev/null | awk '
    NR == 1 || NF < 6 { next }
    $1 ~ /^(tmpfs|devtmpfs|overlay|shm|none|udev|map)$/ { next }
    {
      mount = $NF; total = $(NF - 4); used = $(NF - 3)
      if (total !~ /^[0-9]+$/ || used !~ /^[0-9]+$/) next
      if (n++) printf ","
      gsub(/\\/, "\\\\", mount); gsub(/"/, "\\\"", mount)
      printf "{\"mount\":\"%s\",\"total\":%d,\"used\":%d}", mount, total * 1024, used * 1024
    }'
}

mode_sys() {
  set -- $(memory)
  printf '{"host":"%s","uptime_s":%s,"load":%s,"mem_total":%s,"mem_used":%s,"disks":[%s],"os":"%s"}\n' \
    "$(esc "$(hostname 2>/dev/null || echo unknown)")" \
    "$(uptime_seconds)" "$(load_avg)" "${1:-0}" "${2:-0}" \
    "$(disks)" "$(esc "$(os_name)")"
}

mode_ports() {
  out=""
  if have ss; then
    out=$(ss -ltnp 2>/dev/null | awk '
      NR == 1 { next }
      {
        split($4, a, ":"); port = a[length(a)]
        pid = 0; cmd = ""
        if (match($0, /pid=[0-9]+/)) pid = substr($0, RSTART + 4, RLENGTH - 4)
        if (match($0, /users:\(\("[^"]+/)) cmd = substr($0, RSTART + 9, RLENGTH - 9)
        gsub(/"/, "", cmd)
        if (port ~ /^[0-9]+$/) printf "tcp\t%s\t%s\t%s\n", port, pid, cmd
      }')
  elif have netstat; then
    out=$(netstat -ltnp 2>/dev/null | awk '/LISTEN/ {
        split($4, a, ":"); port = a[length(a)]
        split($NF, b, "/"); pid = b[1] + 0; cmd = b[2]
        if (port ~ /^[0-9]+$/) printf "tcp\t%s\t%s\t%s\n", port, pid, cmd }')
  elif have lsof; then
    out=$(lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR == 1 { next }
      { split($9, a, ":"); port = a[length(a)]
        if (port ~ /^[0-9]+$/) printf "tcp\t%s\t%s\t%s\n", port, $2, $1 }')
  fi
  printf '%s' "$out" | awk -F'\t' '
    BEGIN { printf "{\"ports\":[" }
    NF >= 2 {
      if (seen[$2]++) next
      if (n++) printf ","
      gsub(/\\/, "\\\\", $4); gsub(/"/, "\\\"", $4)
      printf "{\"proto\":\"%s\",\"port\":%d,\"pid\":%d,\"cmd\":\"%s\"}", $1, $2, $3, $4
    }
    END { printf "]}\n" }'
}

mode_sessions() {
  if ! have tmux; then
    printf '{"sessions":[]}\n'
    return
  fi
  tmux ls -F '#{session_name}	#{session_created}	#{session_attached}' 2>/dev/null |
    while IFS='	' read -r name created attached; do
      case "$name" in
        berth/*) ;;
        *) continue ;;
      esac
      windows=$(tmux list-windows -t "$name" -F '#{window_index}	#{pane_current_command}	#{pane_pid}' 2>/dev/null |
        awk -F'\t' '{ if (n++) printf ","
          gsub(/\\/, "\\\\", $2); gsub(/"/, "\\\"", $2)
          printf "{\"index\":%d,\"cmd\":\"%s\",\"pid\":%d}", $1, $2, $3 }')
      printf '%s\t%s\t%s\t%s\n' "$name" "$created" "$attached" "$windows"
    done | awk -F'\t' 'BEGIN { printf "{\"sessions\":[" }
      { if (n++) printf ","
        gsub(/\\/, "\\\\", $1); gsub(/"/, "\\\"", $1)
        printf "{\"name\":\"%s\",\"created\":%d,\"attached\":%s,\"windows\":[%s]}", \
          $1, $2, ($3 == "1" ? "true" : "false"), $4 }
      END { printf "]}\n" }'
}

# Detection is by process name only. No pane contents are read.
mode_agents() {
  if ! have tmux; then
    printf '{"agents":[]}\n'
    return
  fi
  tmux list-panes -a -F '#{session_name}	#{window_index}	#{pane_current_command}	#{pane_pid}' 2>/dev/null |
    awk -F'\t' '
      BEGIN { printf "{\"agents\":[" }
      $1 ~ /^berth\// {
        cmd = $3
        kind = "other"
        if (cmd ~ /claude/) kind = "claude-code"
        else if (cmd ~ /codex/) kind = "codex"
        else if (cmd ~ /opencode/) kind = "opencode"
        else next
        if (n++) printf ","
        gsub(/\\/, "\\\\", $1); gsub(/"/, "\\\"", $1)
        printf "{\"session\":\"%s\",\"window\":%d,\"kind\":\"%s\",\"pid\":%d,\"alive\":true}", \
          $1, $2, kind, $4
      }
      END { printf "]}\n" }'
}

mode_tail() {
  path="${1:-}"
  lines="${2:-200}"
  [ -z "$path" ] && { echo "usage: probe.sh tail <path> <n>" >&2; exit 2; }
  [ -r "$path" ] || { echo "cannot read $path" >&2; exit 1; }
  tail -n "$lines" -- "$path"
}

mode_remove() { rm -rf "$BERTH_DIR"; printf '{"removed":true}\n'; }

case "${1:-}" in
  sys)      mode_sys ;;
  ports)    mode_ports ;;
  sessions) mode_sessions ;;
  agents)   mode_agents ;;
  tail)     shift; mode_tail "$@" ;;
  remove)   mode_remove ;;
  *)        echo "usage: probe.sh {sys|ports|sessions|agents|tail <path> <n>|remove}" >&2; exit 2 ;;
esac
