#!/usr/bin/env bash
# Boots image.iso + disk.img headless in QEMU and takes screenshots through
# the QEMU monitor, for checking the desktop without a visible window.
#
#   tools/qemu_shot.sh OUT_PREFIX "CMD;CMD;..."
#
# Each CMD is one of:
#   sleep N         wait N seconds
#   shot NAME       screenshot to OUT_PREFIX-NAME.ppm
#   regs NAME       CPU registers to OUT_PREFIX-NAME.txt (for hangs)
#   moveto X Y      put the mouse at screen pixel (X, Y)
#   click X Y       move there and left-click (dclick: double-click)
#   type TEXT       type lowercase TEXT (spaces, / . - _ allowed)
#   anything else   a raw QEMU monitor command (`sendkey ret`, ...)
# Serial output goes to OUT_PREFIX-serial.log. Needs image.iso + disk.img.
set -euo pipefail
cd "$(dirname "$0")/.."
out="$1"
script="$2"
sock="$(mktemp -u /tmp/konjac-mon.XXXXXX)"

qemu-system-x86_64 -m 256M -no-reboot -boot order=d -rtc base=localtime \
    -cdrom image.iso \
    -drive file=disk.img,format=raw,if=ide,index=0,media=disk \
    -display none -serial "file:$out-serial.log" \
    -monitor "unix:$sock,server,nowait" &
qpid=$!
trap 'kill $qpid 2>/dev/null || true; rm -f "$sock"' EXIT

for _ in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done

mon() {
    python3 - "$sock" "$1" "${2:-}" <<'EOF'
import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall(sys.argv[2].encode() + b"\n")
time.sleep(0.5 if sys.argv[3] else 0.15)
if sys.argv[3]:
    s.setblocking(False)
    out = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            out += chunk
    except BlockingIOError:
        pass
    open(sys.argv[3], "ab").write(out)
s.close()
EOF
}

# PS/2 packets carry 9-bit deltas, so move in small steps.
step_move() {
    local dx=$1 dy=$2
    while [ "$dx" -ne 0 ] || [ "$dy" -ne 0 ]; do
        local sx=$(( dx > 40 ? 40 : (dx < -40 ? -40 : dx) ))
        local sy=$(( dy > 40 ? 40 : (dy < -40 ? -40 : dy) ))
        mon "mouse_move $sx $sy"
        dx=$((dx - sx)); dy=$((dy - sy))
    done
}
moveto() {
    step_move -1000 -1000
    step_move "$1" "$2"
}

IFS=';' read -ra cmds <<< "$script"
for c in "${cmds[@]}"; do
    c="$(echo "$c" | sed 's/^ *//;s/ *$//')"
    case "$c" in
        "") ;;
        sleep\ *) sleep "${c#sleep }" ;;
        shot\ *) mon "screendump $out-${c#shot }.ppm"; sleep 0.3 ;;
        regs\ *) mon "info registers" "$out-${c#regs }.txt" ;;
        moveto\ *) read -r _ x y <<< "$c"; moveto "$x" "$y" ;;
        click\ *) read -r _ x y <<< "$c"; moveto "$x" "$y"; sleep 0.2; mon "mouse_button 1"; sleep 0.15; mon "mouse_button 0" ;;
        dclick\ *) read -r _ x y <<< "$c"; moveto "$x" "$y"; sleep 0.2; mon "mouse_button 1"; mon "mouse_button 0"; sleep 0.05; mon "mouse_button 1"; mon "mouse_button 0" ;;
        type\ *) t="${c#type }"; for ((k=0; k<${#t}; k++)); do ch="${t:k:1}"; case "$ch" in " ") ch=spc;; "/") ch=slash;; ".") ch=dot;; "-") ch=minus;; "_") ch=shift-minus;; esac; mon "sendkey $ch"; done ;;
        *) mon "$c" ;;
    esac
done
