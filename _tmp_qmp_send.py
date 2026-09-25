import socket, json, sys, time

def qmp_connect(path="/tmp/qmp.sock"):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    f = s.makefile("rwb")
    f.readline()
    s.sendall(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
    f.readline()
    return s, f

def send_keys(s, f, qcodes):
    cmd = {"execute": "send-key", "arguments": {"keys": [{"type": "qcode", "data": q} for q in qcodes]}}
    s.sendall(json.dumps(cmd).encode() + b"\n")
    f.readline()
    time.sleep(0.03)

def send_text(s, f, text):
    for ch in text:
        shift = False
        if ch == " ":
            key = "spc"
        elif ch == "\n":
            key = "ret"
        elif ch == "=":
            key = "equal"
        elif ch == ".":
            key = "dot"
        elif ch == "/":
            key = "slash"
        elif ch == "-":
            key = "minus"
        elif ch == ",":
            key = "comma"
        elif ch == "_":
            key = "minus"; shift = True
        elif ch.isupper():
            key = ch.lower(); shift = True
        elif ch.isdigit():
            key = ch
        else:
            key = ch
        if shift:
            send_keys(s, f, ["shift", key])
        else:
            send_keys(s, f, [key])

def main():
    text = sys.argv[1] if len(sys.argv) > 1 else ""
    s, f = qmp_connect()
    send_text(s, f, text)
    s.close()

if __name__ == "__main__":
    main()
