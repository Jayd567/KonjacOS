import socket, json, sys

def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/shot.ppm"
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect("/tmp/qmp.sock")
    f = s.makefile("rwb")
    f.readline()
    s.sendall(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
    f.readline()
    s.sendall(json.dumps({"execute": "screendump", "arguments": {"filename": path}}).encode() + b"\n")
    print(f.readline())
    s.close()

if __name__ == "__main__":
    main()
