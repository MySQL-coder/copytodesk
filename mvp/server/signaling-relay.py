#!/usr/bin/env python3
"""
CopyToDesk Zero-Dependency Python 3.10+ Signaling + TCP Relay + Cross-Platform Web Viewer (v0.4.0 P1)
Protocol-identical to `signaling-relay.mjs`:
  - TCP Relay (--relay-port 19901):
      Host Control:  REGISTER <deviceId> <name> [LAN=...] [KEY=<proof>]\n
                       -> OK REGISTERED <deviceId>\n
                       -> ERR ALREADY_OWNED\n  (anti-hijack device ownership binding!)
                     <- INCOMING <sessionId>\n
                     PING\n -> PONG\n
      Host Data:     ACCEPT <sessionId>\n  ->  OK TUNNEL\n  (transparent E2EE CTD1 pipe)
      Client/Viewer: LOOKUP <deviceId>\n   ->  OK LAN <ip1:port,...>\n
                     CONNECT <deviceId>\n  ->  OK TUNNEL\n  (transparent E2EE CTD1 pipe)
"""
import argparse
import asyncio
import base64
import datetime
import hashlib
import json
import os
import secrets
import socket
import struct
import sys

MAGIC = 0x31445443  # "CTD1" LE

hosts = {}            # device_id -> dict(deviceId, name, writer, registeredAt, remoteAddr)
pending_sessions = {} # session_id -> dict(reader, writer, device_id, fut)
stats = {"totalTunnels": 0, "totalBytesRelayed": 0, "hijackBlocks": 0}
BASE_DIR = os.path.dirname(os.path.abspath(__file__))
UI_DIR = os.path.join(BASE_DIR, "ui")
OWNERS_FILE = os.path.join(BASE_DIR, "device_owners.json")
ADMIN_KEY = os.environ.get("CTD_ADMIN_KEY", "").strip()
ALLOW_PUBLIC_LIST = False


def load_device_owners() -> dict:
    if os.path.exists(OWNERS_FILE):
        try:
            with open(OWNERS_FILE, "r", encoding="utf-8") as f:
                data = json.load(f)
                if isinstance(data, dict):
                    return {str(k): str(v) for k, v in data.items()}
        except Exception as e:
            print(f"[Relay] Warn loading device_owners.json: {e}", flush=True)
    return {}


def save_device_owners(owners: dict):
    tmp = OWNERS_FILE + ".tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(owners, f, indent=2, ensure_ascii=False)
        os.replace(tmp, OWNERS_FILE)
    except Exception as e:
        print(f"[Relay] Warn saving device_owners.json: {e}", flush=True)


device_owners = load_device_owners()


def set_nodelay(writer):
    sock = writer.get_extra_info("socket")
    if sock is not None:
        try:
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError:
            pass


async def pipe_one_way(src_reader, dst_writer):
    try:
        while True:
            data = await src_reader.read(65536)
            if not data:
                break
            stats["totalBytesRelayed"] += len(data)
            dst_writer.write(data)
            await dst_writer.drain()
    except Exception:
        pass
    finally:
        try:
            dst_writer.close()
        except Exception:
            pass


async def bridge(r1, w1, r2, w2):
    stats["totalTunnels"] += 1
    set_nodelay(w1)
    set_nodelay(w2)
    t1 = asyncio.create_task(pipe_one_way(r1, w2))
    t2 = asyncio.create_task(pipe_one_way(r2, w1))
    await asyncio.wait([t1, t2], return_when=asyncio.FIRST_COMPLETED)
    for t in (t1, t2):
        t.cancel()
    for w in (w1, w2):
        try:
            w.close()
        except Exception:
            pass


async def handle_relay_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    set_nodelay(writer)
    try:
        raw_line = await asyncio.wait_for(reader.readline(), timeout=8.0)
        if not raw_line:
            writer.close()
            return
        line = raw_line.decode("utf-8", errors="replace").strip()
        parts = line.split()
        if not parts:
            writer.write(b"ERR EMPTY\n")
            await writer.drain()
            writer.close()
            return
        cmd = parts[0].upper()

        if cmd == "LOOKUP" and len(parts) >= 2:
            device_id = parts[1]
            host_info = hosts.get(device_id)
            if not host_info or host_info["writer"].is_closing():
                writer.write(b"ERR OFFLINE\n")
            else:
                lan_csv = ",".join(host_info.get("lanAddrs", []))
                writer.write(f"OK LAN {lan_csv}\n".encode("utf-8"))
            await writer.drain()
            writer.close()
            return

        if cmd == "REGISTER" and len(parts) >= 2:
            device_id = parts[1]
            lan_addrs = []
            key_proof = ""
            name_parts = []
            for tok in parts[2:]:
                up = tok.upper()
                if up.startswith("LAN="):
                    lan_addrs = [x.strip() for x in tok[4:].split(",") if x.strip()]
                elif up.startswith("KEY="):
                    key_proof = tok[4:].strip()
                else:
                    name_parts.append(tok)
            name = " ".join(name_parts) if name_parts else "Host"

            # Anti-Hijack Device Ownership Verification
            key_hash = hashlib.sha256(key_proof.encode("utf-8")).hexdigest() if key_proof else ""
            existing_owner = device_owners.get(device_id)
            if existing_owner:
                if not key_hash or not secrets.compare_digest(existing_owner, key_hash):
                    stats["hijackBlocks"] += 1
                    print(f"[Relay] BLOCKED hijack attempt on owned deviceId={device_id}", flush=True)
                    writer.write(b"ERR ALREADY_OWNED\n")
                    await writer.drain()
                    writer.close()
                    return
            elif key_hash:
                device_owners[device_id] = key_hash
                save_device_owners(device_owners)
                print(f"[Relay] Bound new device ownership: deviceId={device_id}", flush=True)

            old = hosts.get(device_id)
            if old:
                try:
                    old["writer"].close()
                except Exception:
                    pass
            peer = writer.get_extra_info("peername")
            now_iso = datetime.datetime.utcnow().isoformat() + "Z"
            info = {
                "deviceId": device_id,
                "name": name,
                "lanAddrs": lan_addrs,
                "owned": device_id in device_owners,
                "writer": writer,
                "registeredAt": now_iso,
                "lastSeenAt": now_iso,
                "remoteAddr": str(peer[0]) if peer else "",
            }
            hosts[device_id] = info
            writer.write(f"OK REGISTERED {device_id}\n".encode("utf-8"))
            await writer.drain()
            print(
                f"[Relay] Host registered: deviceId={device_id} name={name} lan={lan_addrs} owned={info['owned']}",
                flush=True,
            )
            try:
                while True:
                    hb_raw = await asyncio.wait_for(reader.readline(), timeout=70.0)
                    if not hb_raw:
                        break
                    hb_cmd = hb_raw.decode("utf-8", errors="replace").strip().upper()
                    info["lastSeenAt"] = datetime.datetime.utcnow().isoformat() + "Z"
                    if hb_cmd == "PING":
                        writer.write(b"PONG\n")
                        await writer.drain()
            except Exception:
                pass
            finally:
                if hosts.get(device_id) is info:
                    del hosts[device_id]
                    print(f"[Relay] Host offline: deviceId={device_id}", flush=True)
                try:
                    writer.close()
                except Exception:
                    pass
            return

        if cmd == "CONNECT" and len(parts) >= 2:
            device_id = parts[1]
            host_info = hosts.get(device_id)
            if not host_info or host_info["writer"].is_closing():
                writer.write(b"ERR OFFLINE\n")
                await writer.drain()
                writer.close()
                return
            session_id = secrets.token_hex(6)
            fut = asyncio.get_running_loop().create_future()
            pending_sessions[session_id] = {
                "reader": reader,
                "writer": writer,
                "deviceId": device_id,
                "fut": fut,
            }
            try:
                host_info["writer"].write(f"INCOMING {session_id}\n".encode("utf-8"))
                await host_info["writer"].drain()
            except Exception:
                pending_sessions.pop(session_id, None)
                writer.write(b"ERR HOST_UNREACHABLE\n")
                await writer.drain()
                writer.close()
                return
            try:
                host_r, host_w = await asyncio.wait_for(fut, timeout=8.0)
            except asyncio.TimeoutError:
                pending_sessions.pop(session_id, None)
                writer.write(b"ERR TIMEOUT\n")
                await writer.drain()
                writer.close()
                return
            writer.write(b"OK TUNNEL\n")
            host_w.write(b"OK TUNNEL\n")
            await writer.drain()
            await host_w.drain()
            print(f"[Relay] Tunnel established: session={session_id} device={device_id}", flush=True)
            await bridge(reader, writer, host_r, host_w)
            return

        if cmd == "ACCEPT" and len(parts) >= 2:
            session_id = parts[1]
            pending = pending_sessions.pop(session_id, None)
            if not pending or pending["fut"].done():
                writer.write(b"ERR NO_SESSION\n")
                await writer.drain()
                writer.close()
                return
            pending["fut"].set_result((reader, writer))
            while not writer.is_closing():
                await asyncio.sleep(1.0)
            return

        writer.write(b"ERR UNKNOWN_CMD\n")
        await writer.drain()
        writer.close()
    except Exception:
        try:
            writer.close()
        except Exception:
            pass


# ---- One-shot local CTD1 client for HTTP Web Viewer proxy ----
async def ctd_send(writer, msg_type, ch, payload=b"", seq=1):
    hdr = struct.pack("<IBBBBII", MAGIC, 1, msg_type, 0, ch, len(payload), seq)
    writer.write(hdr + payload)
    await writer.drain()


async def ctd_read(reader):
    hdr = await asyncio.wait_for(reader.readexactly(16), timeout=8.0)
    magic, ver, mtype, flags, ch, plen, seq = struct.unpack("<IBBBBII", hdr)
    if magic != MAGIC:
        raise ValueError("Invalid CTD1 magic")
    payload = await asyncio.wait_for(reader.readexactly(plen), timeout=8.0) if plen > 0 else b""
    return mtype, payload


async def open_local_tunnel(relay_port, device_id, token):
    reader, writer = await asyncio.open_connection("127.0.0.1", relay_port)
    set_nodelay(writer)
    writer.write(f"CONNECT {device_id}\n".encode("utf-8"))
    await writer.drain()
    line = (await asyncio.wait_for(reader.readline(), timeout=6.0)).decode("utf-8", errors="replace").strip()
    if not line.startswith("OK TUNNEL"):
        writer.close()
        raise RuntimeError(line)
    hello = json.dumps({"clientName": "WebViewer-PyRelay", "token": token}).encode("utf-8")
    await ctd_send(writer, 0x01, 0, hello, 1)
    mtype, payload = await ctd_read(reader)
    if mtype != 0x02:
        writer.close()
        raise RuntimeError("Handshake rejected")
    ack = json.loads(payload.decode("utf-8"))
    return reader, writer, ack


async def wait_ctd_msg(reader, target_type):
    for _ in range(64):
        mtype, payload = await ctd_read(reader)
        if mtype == target_type:
            return payload
    raise TimeoutError("Timed out waiting for CTD1 response")


def http_response(status_line, content_type, body: bytes, extra_headers=None):
    hdrs = [
        f"HTTP/1.1 {status_line}",
        f"Content-Type: {content_type}",
        f"Content-Length: {len(body)}",
        "Connection: close",
    ]
    if extra_headers:
        for k, v in extra_headers.items():
            hdrs.append(f"{k}: {v}")
    return ("\r\n".join(hdrs) + "\r\n\r\n").encode("utf-8") + body


async def handle_http(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, relay_port: int):
    try:
        req_line = await asyncio.wait_for(reader.readline(), timeout=5.0)
        if not req_line:
            writer.close()
            return
        parts = req_line.decode("utf-8", errors="replace").strip().split()
        if len(parts) < 2:
            writer.close()
            return
        method, path = parts[0], parts[1].split("?")[0]
        content_len = 0
        req_admin_key = ""
        while True:
            hline = await asyncio.wait_for(reader.readline(), timeout=5.0)
            if not hline or hline in (b"\r\n", b"\n"):
                break
            s = hline.decode("latin1")
            low = s.lower()
            if low.startswith("content-length:"):
                content_len = int(s.split(":", 1)[1].strip())
            elif low.startswith("x-admin-key:"):
                req_admin_key = s.split(":", 1)[1].strip()
        body_raw = await asyncio.wait_for(reader.readexactly(content_len), timeout=10.0) if content_len > 0 else b""

        if method == "GET" and path == "/":
            p = os.path.join(UI_DIR, "index.html")
            data = open(p, "rb").read() if os.path.exists(p) else b"<h1>CopyToDesk Relay Online</h1>"
            writer.write(http_response("200 OK", "text/html; charset=utf-8", data))
        elif method == "GET" and path == "/app.js":
            p = os.path.join(UI_DIR, "app.js")
            data = open(p, "rb").read() if os.path.exists(p) else b""
            writer.write(http_response("200 OK", "application/javascript; charset=utf-8", data))
        elif method == "GET" and path == "/api/devices":
            can_list = ALLOW_PUBLIC_LIST or (bool(ADMIN_KEY) and secrets.compare_digest(req_admin_key, ADMIN_KEY))
            if not can_list:
                payload = {"devices": [], "privacyProtected": True, "onlineCount": len(hosts)}
                writer.write(http_response("200 OK", "application/json", json.dumps(payload).encode("utf-8")))
            else:
                devs = [
                    {
                        "deviceId": h["deviceId"],
                        "name": h["name"],
                        "owned": h.get("owned", False),
                        "lanAddrs": h.get("lanAddrs", []),
                        "registeredAt": h["registeredAt"],
                        "lastSeenAt": h.get("lastSeenAt", h["registeredAt"]),
                    }
                    for h in hosts.values()
                ]
                writer.write(http_response("200 OK", "application/json", json.dumps({"devices": devs, "privacyProtected": False}).encode("utf-8")))
        elif method == "GET" and path == "/api/health":
            out = {"status": "ok", "version": "0.6.0", "privacyProtected": not ALLOW_PUBLIC_LIST, "onlineHosts": len(hosts), "ownedDevices": len(device_owners), **stats}
            writer.write(http_response("200 OK", "application/json", json.dumps(out).encode("utf-8")))
        elif method == "POST" and path == "/api/screenshot":
            b = json.loads(body_raw.decode("utf-8"))
            r, w, _ = await open_local_tunnel(relay_port, b["deviceId"], b.get("token", ""))
            try:
                req = struct.pack("<HBB", int(b.get("maxWidth", 1280)), int(b.get("quality", 72)), 0)
                await ctd_send(w, 0x40, 0, req, 2)
                resp = await wait_ctd_msg(r, 0x41)
                orig_w, orig_h, _, _, _, plen = struct.unpack_from("<HHHHBI", resp, 0)
                img = resp[13:13 + plen]
                writer.write(http_response("200 OK", "image/jpeg", img, {"X-Orig-W": str(orig_w), "X-Orig-H": str(orig_h)}))
            finally:
                w.close()
        elif method == "POST" and path == "/api/input":
            b = json.loads(body_raw.decode("utf-8"))
            r, w, ack = await open_local_tunnel(relay_port, b["deviceId"], b.get("token", ""))
            try:
                ow = max(1, int(b.get("origW") or ack.get("width", 1280)))
                oh = max(1, int(b.get("origH") or ack.get("height", 720)))
                nx = max(0, min(65535, round(int(b["x"]) * 65535 / ow)))
                ny = max(0, min(65535, round(int(b["y"]) * 65535 / oh)))
                for act, seq in ((1, 2), (2, 3)):
                    await ctd_send(w, 0x30, 3, struct.pack("<BBHHh", act, 1, nx, ny, 0), seq)
                writer.write(http_response("200 OK", "application/json", b'{"ok":true}'))
            finally:
                w.close()
        elif method == "POST" and path == "/api/clipboard":
            b = json.loads(body_raw.decode("utf-8"))
            r, w, _ = await open_local_tunnel(relay_port, b["deviceId"], b.get("token", ""))
            try:
                if b.get("action") == "set":
                    await ctd_send(w, 0x51, 0, str(b.get("text", "")).encode("utf-8"), 2)
                else:
                    await ctd_send(w, 0x50, 0, b"", 2)
                resp = await wait_ctd_msg(r, 0x51)
                writer.write(http_response("200 OK", "application/json", json.dumps({"ok": True, "text": resp.decode("utf-8", errors="replace")}).encode("utf-8")))
            finally:
                w.close()
        elif method == "POST" and path == "/api/files":
            b = json.loads(body_raw.decode("utf-8"))
            r, w, _ = await open_local_tunnel(relay_port, b["deviceId"], b.get("token", ""))
            try:
                if b.get("action") == "upload":
                    rel = str(b.get("remotePath", "")).encode("utf-8")
                    data = base64.b64decode(b.get("contentBase64", ""))
                    sha = hashlib.sha256(data).hexdigest().encode("ascii")
                    payload = struct.pack("<H", len(rel)) + rel + sha + data
                    await ctd_send(w, 0x62, 5, payload, 2)
                    resp = await wait_ctd_msg(r, 0x63)
                elif b.get("action") == "download":
                    rel = str(b.get("remotePath", "")).encode("utf-8")
                    await ctd_send(w, 0x64, 5, rel, 2)
                    raw = await wait_ctd_msg(r, 0x65)
                    if not raw or raw[0] == 0:
                        err_len = struct.unpack_from("<H", raw, 65)[0] if len(raw) >= 67 else 0
                        err_msg = raw[67:67 + err_len].decode("utf-8", errors="replace") if err_len else "Download failed"
                        resp = json.dumps({"ok": False, "error": err_msg}).encode("utf-8")
                    else:
                        sha_hex = raw[1:65].decode("ascii", errors="replace")
                        file_bytes = raw[67:]
                        resp = json.dumps({
                            "ok": True,
                            "sha256": sha_hex,
                            "bytes": len(file_bytes),
                            "contentBase64": base64.b64encode(file_bytes).decode("ascii"),
                        }).encode("utf-8")
                else:
                    await ctd_send(w, 0x60, 5, str(b.get("remotePath", "")).encode("utf-8"), 2)
                    resp = await wait_ctd_msg(r, 0x61)
                writer.write(http_response("200 OK", "application/json", resp))
            finally:
                w.close()
        elif method == "POST" and path == "/api/exec":
            b = json.loads(body_raw.decode("utf-8"))
            r, w, _ = await open_local_tunnel(relay_port, b["deviceId"], b.get("token", ""))
            try:
                req = json.dumps({"command": str(b.get("command", "")), "timeoutMs": 5000}).encode("utf-8")
                await ctd_send(w, 0x70, 0, req, 2)
                resp = await wait_ctd_msg(r, 0x71)
                writer.write(http_response("200 OK", "application/json", resp))
            finally:
                w.close()
        else:
            writer.write(http_response("404 Not Found", "text/plain", b"Not Found"))
        await writer.drain()
    except Exception as e:
        try:
            writer.write(http_response("500 Internal Server Error", "text/plain; charset=utf-8", str(e).encode("utf-8")))
            await writer.drain()
        except Exception:
            pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    global ALLOW_PUBLIC_LIST
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--http-port", type=int, default=19900)
    ap.add_argument("--relay-port", type=int, default=19901)
    ap.add_argument("--allow-public-list", action="store_true")
    args = ap.parse_args()
    ALLOW_PUBLIC_LIST = bool(args.allow_public_list)

    relay_srv = await asyncio.start_server(handle_relay_client, args.bind, args.relay_port)
    http_srv = await asyncio.start_server(lambda r, w: handle_http(r, w, args.relay_port), args.bind, args.http_port)
    print(f"[SignalingRelay-Py v0.6.0] TCP Relay listening on {args.bind}:{args.relay_port} (privacy={not ALLOW_PUBLIC_LIST})", flush=True)
    print(f"[SignalingRelay-Py v0.6.0] HTTP Web Viewer listening on http://{args.bind}:{args.http_port}", flush=True)
    async with relay_srv, http_srv:
        await asyncio.gather(relay_srv.serve_forever(), http_srv.serve_forever())


if __name__ == "__main__":
    asyncio.run(main())
