#!/usr/bin/env python3
"""End-to-end exercise of every bugsee-cli upload flow against a local mock that
speaks the symbols presigned protocol AND the builds (single / chunked /
build-info) protocols, capturing every uploaded body. Cross-platform, stdlib
only — drives the REAL binary through real HTTP requests (not dry-run), so it
proves the wire path on whatever OS/arch it runs on.

Usage:
    python scripts/e2e_flows.py --bin /path/to/bugsee-cli[.exe] [--keep]

Exit code 0 = all flows passed; non-zero = at least one failed.
"""
import argparse
import hashlib
import http.server
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading

TOKEN = "TKN"
STATE = {"flow": "none", "port": 0, "cap": ""}


def cappath(name):
    return os.path.join(STATE["cap"], name)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        n = int(self.headers.get("Content-Length", 0))
        return self.rfile.read(n) if n else b""

    def _base(self):
        return f"http://127.0.0.1:{STATE['port']}"

    def do_GET(self):
        if self.path.endswith("/builds/chunk-options"):
            self._json({"ok": True, "result": {"chunk_size": 65536, "max_chunks": 100000}})
            return
        self._json({"ok": False, "error": "unexpected GET " + self.path}, 404)

    def do_POST(self):
        flow = STATE["flow"]
        body = self._read_body()
        p = self.path
        if p.endswith("/symbols"):
            with open(cappath(f"{flow}__symbols_post.json"), "wb") as f:
                f.write(body)
            self._json({"code": 0, "endpoint": f"{self._base()}/put/{flow}__upload"})
        elif p.endswith("/builds/chunks/check"):
            req = json.loads(body)
            hashes = req["sha1_list"]
            with open(cappath(f"{flow}__chunk_order.json"), "w") as f:
                json.dump(hashes, f)
            uniq = list(dict.fromkeys(hashes))
            urls = {h: f"{self._base()}/put/{flow}__chunk_{h}" for h in uniq}
            self._json({"ok": True, "result": {"missing": uniq, "upload_urls": urls}})
        elif p.endswith("/builds/chunked"):
            with open(cappath(f"{flow}__chunked_post.json"), "wb") as f:
                f.write(body)
            self._json({"ok": True, "result": {"build_id": "b-chunked-e2e", "build_info_upload_endpoint": ""}})
        elif p.endswith("/builds"):
            with open(cappath(f"{flow}__builds_post.json"), "wb") as f:
                f.write(body)
            self._json({"ok": True, "result": {
                "build_id": "b-e2e",
                "endpoint": f"{self._base()}/put/{flow}__artifact",
                "build_info_upload_endpoint": f"{self._base()}/put/{flow}__buildinfo",
            }})
        else:
            self._json({"ok": False, "error": "unexpected POST " + p}, 404)

    def do_PUT(self):
        body = self._read_body()
        name = self.path.rsplit("/put/", 1)[-1]
        with open(cappath(name + ".bin"), "wb") as f:
            f.write(body)
        self.send_response(200)
        self.end_headers()


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def make_fixtures(fix):
    os.makedirs(fix, exist_ok=True)
    with open(os.path.join(fix, "mapping.txt"), "w") as f:
        f.write("com.example.Foo -> a.a.a:\n    int field -> a\n    void method() -> a\n"
                "com.example.Bar -> a.a.b:\n    void run() -> b\n")
    with open(os.path.join(fix, "app.aab"), "wb") as f:
        f.write(b"PK\x03\x04" + bytes((i * 37) % 256 for i in range(200_000)))
    dist = os.path.join(fix, "dist")
    os.makedirs(dist, exist_ok=True)
    with open(os.path.join(dist, "app.js"), "w") as f:
        f.write("console.log('hello');\n//# sourceMappingURL=app.js.map\n")
    with open(os.path.join(dist, "app.js.map"), "w") as f:
        json.dump({"version": 3, "sources": ["app.js"], "names": [], "mappings": "AAAA"}, f)
    import zipfile as zf
    with zf.ZipFile(os.path.join(fix, "native-debug-symbols.zip"), "w") as z:
        z.writestr("arm64-v8a/libfoo.so", b"\x7fELF" + b"\x02\x01\x01\x00" + b"\x00" * 256)
    with open(os.path.join(fix, "payload.json"), "w") as f:
        json.dump({"version": "1.2.3", "build": "42", "platform": "android"}, f)
    with open(os.path.join(fix, "deps.json"), "w") as f:
        json.dump({"dependencies": [{"name": "okhttp", "version": "4.12.0"}]}, f)
    with open(os.path.join(fix, "timings.json"), "w") as f:
        json.dump({"phases": [{"name": "compile", "ms": 1234}]}, f)
    # dSYM fixture only when the Apple toolchain is present (macOS); skipped elsewhere.
    dsym = os.path.join(fix, "App.dSYM")
    if shutil.which("clang") and shutil.which("dsymutil"):
        c = os.path.join(fix, "t.c")
        with open(c, "w") as f:
            f.write("int main(){return 0;}\n")
        exe = os.path.join(fix, "t")
        try:
            subprocess.run(["clang", "-g", c, "-o", exe], check=True, capture_output=True)
            subprocess.run(["dsymutil", exe, "-o", dsym], check=True, capture_output=True)
        except Exception as e:
            print(f"  [warn] dSYM fixture build failed, skipping dsym flow: {e}")


def run(binpath, flow, args, expect_code=0):
    STATE["flow"] = flow
    env = dict(os.environ, BUGSEE_APP_TOKEN=TOKEN)
    endpoint = f"http://127.0.0.1:{STATE['port']}"
    cmd = [binpath, "--endpoint", endpoint, "--app-token", TOKEN] + args
    r = subprocess.run(cmd, capture_output=True, text=True, env=env)
    ok = (r.returncode == expect_code)
    print(f"  [{'PASS' if ok else f'FAIL(rc={r.returncode})'}] {flow}: {' '.join(args[:4])} ...")
    if not ok:
        print("    stderr:", (r.stderr or "").strip()[-800:])
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True, help="path to bugsee-cli[.exe]")
    ap.add_argument("--keep", action="store_true", help="keep the work directory")
    a = ap.parse_args()
    binpath = a.bin

    work = tempfile.mkdtemp(prefix="bugsee_e2e_")
    fix = os.path.join(work, "fixtures")
    STATE["cap"] = os.path.join(work, "captured")
    os.makedirs(STATE["cap"], exist_ok=True)
    make_fixtures(fix)

    port = free_port()
    STATE["port"] = port
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    print(f"bugsee-cli e2e — bin={binpath}")
    print(f"mock on 127.0.0.1:{port}, work={work}\n")

    results = {}
    v = ["--version", "1.2.3", "--build", "42"]

    STATE["flow"] = "inject"
    r = subprocess.run([binpath, "sourcemaps", "inject", os.path.join(fix, "dist")],
                       capture_output=True, text=True)
    results["sourcemaps_inject"] = (r.returncode == 0)
    print(f"  [{'PASS' if r.returncode == 0 else 'FAIL'}] sourcemaps_inject (local)")

    results["proguard"] = run(binpath, "proguard", ["debug-files", "upload", "--type", "proguard",
                                                    os.path.join(fix, "mapping.txt")] + v)
    results["sourcemaps_upload"] = run(binpath, "sourcemaps", ["debug-files", "upload", "--type", "sourcemaps",
                                       os.path.join(fix, "dist", "app.js.map")] + v)
    results["elf"] = run(binpath, "elf", ["debug-files", "upload", "--type", "elf",
                                          os.path.join(fix, "native-debug-symbols.zip"),
                                          "--uuid", "11111111-2222-3333-4444-555555555555"] + v)
    if os.path.isdir(os.path.join(fix, "App.dSYM")):
        results["dsym"] = run(binpath, "dsym", ["debug-files", "upload", "--type", "dsym",
                                                os.path.join(fix, "App.dSYM")] + v)
    else:
        print("  [SKIP] dsym (no Apple toolchain on this host)")

    pj = os.path.join(fix, "payload.json")
    results["build_single"] = run(binpath, "build_single", ["upload", "build", "--payload-json", pj,
                                  "--artifact", os.path.join(fix, "app.aab"),
                                  "--mapping", os.path.join(fix, "mapping.txt")])
    results["build_chunked"] = run(binpath, "build_chunked", ["upload", "build", "--payload-json", pj,
                                   "--artifact", os.path.join(fix, "app.aab"), "--chunked"])
    results["build_info"] = run(binpath, "build_info", ["upload", "build-info", "--payload-json", pj,
                                "--deps", os.path.join(fix, "deps.json"),
                                "--timings", os.path.join(fix, "timings.json")])

    # reassemble chunked artefact from captured chunks → sanity it is a zip
    co = cappath("build_chunked__chunk_order.json")
    if os.path.exists(co):
        order = json.load(open(co))
        blob = b"".join(open(cappath(f"build_chunked__chunk_{h}.bin"), "rb").read() for h in order)
        results["chunked_reassembly_is_zip"] = blob[:2] == b"PK"

    # spot-check a couple of captured wire bodies
    try:
        post = json.load(open(cappath("sourcemaps__symbols_post.json")))
        embedded = json.load(open(os.path.join(fix, "dist", "app.js.map"))).get("debug_id")
        results["sourcemap_keyed_by_debug_id"] = (post.get("uuid") == embedded and embedded is not None)
    except Exception as e:
        print("  [warn] could not verify sourcemap key:", e)
        results["sourcemap_keyed_by_debug_id"] = False
    try:
        bp = json.load(open(cappath("build_single__builds_post.json")))
        results["build_requests_artifact_upload"] = (bp.get("request_artifact_upload") is True)
    except Exception:
        results["build_requests_artifact_upload"] = False

    srv.shutdown()
    if not a.keep:
        shutil.rmtree(work, ignore_errors=True)

    print("\n=== RESULTS ===")
    allok = True
    for k, ok in results.items():
        print(f"  {'PASS' if ok else 'FAIL'}  {k}")
        allok = allok and ok
    print("\nOVERALL:", "ALL PASS" if allok else "FAILURES PRESENT")
    return 0 if allok else 1


if __name__ == "__main__":
    sys.exit(main())
