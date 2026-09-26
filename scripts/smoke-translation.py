"""Smoke test for the Apple Translation pipeline (no third-party packages).

Modes:
  1. Bridge only (default): probe translator-bridge capabilities and translate
     a fixed sentence.
     python3 scripts/smoke-translation.py [--socket PATH] [--source en] [--target zh-Hans]

  2. Full chain (--core PATH): also spawn translator-core on a temp socket with
     English mock sentences, run a session with translation enabled and assert
     committed subtitles carry a non-empty translated_text.
     python3 scripts/smoke-translation.py --core target/release/translator-core \
         [--bridge target/translator-bridge]
"""
import argparse, json, os, socket, struct, subprocess, sys, tempfile, time, math

def recv_frame(sock):
    header = b""
    while len(header) < 4:
        chunk = sock.recv(4 - len(header))
        if not chunk:
            return None
        header += chunk
    (length,) = struct.unpack(">I", header)
    payload = b""
    while len(payload) < length:
        chunk = sock.recv(length - len(payload))
        if not chunk:
            return None
        payload += chunk
    return payload

def send_frame(sock, obj):
    payload = json.dumps(obj).encode()
    sock.sendall(struct.pack(">I", len(payload)) + payload)

# --- protobuf wire helpers (translator.v1 schema) ---------------------------

def varint(n):
    data = bytearray()
    while n > 127:
        data.append((n & 127) | 128); n >>= 7
    data.append(n)
    return bytes(data)

def field(n, value):
    if isinstance(value, str): value = value.encode()
    if isinstance(value, bytes):
        return varint(n * 8 + 2) + varint(len(value)) + value
    return varint(n * 8) + varint(value)

def parse(data):
    pos, out = 0, {}
    while pos < len(data):
        tag = data[pos]; pos += 1
        key, wire = tag >> 3, tag & 7
        if wire == 0:
            shift = value = 0
            while True:
                b = data[pos]; pos += 1
                value |= (b & 127) << shift
                if b < 128: break
                shift += 7
            out.setdefault(key, []).append(value)
        elif wire == 2:
            shift = size = 0
            while True:
                b = data[pos]; pos += 1
                size |= (b & 127) << shift
                if b < 128: break
                shift += 7
            out.setdefault(key, []).append(data[pos:pos+size]); pos += size
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4
            out.setdefault(key, []).append(data[pos:pos+size]); pos += size
        else:
            raise ValueError(f"wire {wire}")
    return out

def envelope(seq, payload):
    return field(1, seq) + field(2, payload)

def audio_frame(seq, session, rate, channels, frames, samples):
    fmt = field(1, rate) + field(2, channels) + field(3, 1)  # SAMPLE_FORMAT_F32
    inner = field(1, session) + fmt + field(4, frames) + field(5, samples)
    return envelope(seq, varint(10 * 8 + 2) + varint(len(inner)) + inner)

def sine_chunk(seconds, rate, channels, loud=True):
    n = int(rate * seconds)
    amp = 0.4 if loud else 1e-5
    data = bytearray()
    for i in range(n):
        v = amp * math.sin(2 * math.pi * 440 * i / rate)
        for _ in range(channels):
            data += struct.pack("<f", v)
    return bytes(data), n

# --- bridge test -------------------------------------------------------------

def test_bridge(sock_path, source, target):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    # First translate may wait on the language asset download confirmation.
    sock.settimeout(150)
    sock.connect(sock_path)

    def rpc(op, **kw):
        req = {"id": rpc.n, "op": op, **kw}
        rpc.n += 1
        sock.sendall((json.dumps(req) + "\n").encode())
        line = b""
        while not line.endswith(b"\n"):
            chunk = sock.recv(4096)
            if not chunk:
                raise ConnectionError("bridge closed")
            line += chunk
        return json.loads(line.splitlines()[0])
    rpc.n = 1

    caps = rpc("capabilities", source=source, target=target)
    print(f"[bridge] capabilities: {json.dumps(caps, ensure_ascii=False)}")
    if not caps.get("ok"):
        sys.exit("bridge capabilities probe failed")

    sample = "Hello, this is a test sentence for the Apple translation pipeline."
    t0 = time.time()
    result = rpc("translate", text=sample, source=source, target=target)
    dt = time.time() - t0
    print(f"[bridge] translate ({dt:.2f}s): {json.dumps(result, ensure_ascii=False)}")

    if caps.get("status") != "installed":
        # Contract: uninstalled pairs fail fast with actionable guidance
        # instead of hanging on a download prompt a helper cannot present.
        assert not result.get("ok"), "uninstalled pairs must fail fast"
        assert "not installed" in result.get("error", ""), result
        print("[bridge] fail-fast verified (assets not installed)")
        sock.close()
        return

    if not result.get("ok") or not result.get("text"):
        sys.exit("bridge translation failed")
    assert result["text"] != sample, "translation must differ from the source text"
    sock.close()
    print("[bridge] OK")
    return result["text"]

# --- full chain --------------------------------------------------------------

def test_full_chain(core, source, target):
    workdir = tempfile.mkdtemp(prefix="translator-smoke-")
    core_sock = os.path.join(workdir, "core.sock")
    proc = subprocess.Popen(
        [core, "--socket", core_sock, "--log-level", "info",
         "--mock-sentence", "Hello, this is the first English sentence.",
         "--mock-sentence", "The quick brown fox jumps over the lazy dog."],
        stderr=subprocess.PIPE)
    try:
        # Wait for the socket.
        deadline = time.time() + 15
        while time.time() < deadline:
            if os.path.exists(core_sock):
                try:
                    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    probe.connect(core_sock); probe.close()
                    break
                except OSError:
                    pass
            time.sleep(0.05)
        else:
            sys.exit("core never started listening")

        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(60)
        sock.connect(core_sock)

        seq = 0
        def rpc(payload):
            nonlocal seq
            seq += 1
            send_frame(sock, {"seq": seq, "payload": payload})
            return parse(recv_frame(sock))

        rate, channels = 16000, 1
        inp = field(1, rate) + field(2, channels) + field(3, 1)
        asr = field(1, "mock") + field(3, source)          # provider + language
        translation = field(1, "apple-translate") + field(2, target)
        subtitle = field(1, 2) + field(2, 42) + field(3, 100) + field(4, 300)
        start = field(1, "smoke") + field(2, inp) + field(3, 0) + \
            field(4, varint(4 * 8 + 2) + varint(len(asr)) + asr) + \
            field(5, varint(5 * 8 + 2) + varint(len(translation)) + translation) + \
            field(6, varint(6 * 8 + 2) + varint(len(subtitle)) + subtitle)

        reply = parse(rpc(varint(4 * 8 + 2) + varint(len(start)) + start))
        assert 5 in reply, f"no StartSessionResponse: {reply.keys()}"
        start_resp = parse(reply[5][0])
        assert start_resp.get(2) == [1], f"session rejected: {start_resp}"

        # Two utterances separated by silence, ~1.2s each.
        audio = []
        for loud in (True, False, True, False):
            chunk, frames = sine_chunk(1.2, rate, channels, loud=loud)
            audio.append((chunk, frames))
        for chunk, frames in audio:
            rpc(audio_frame(0, "smoke", rate, channels, frames, chunk))

        # Drain events until the stop response.
        committed = []
        seq += 1
        send_frame(sock, {"seq": seq, "payload": varint(6 * 8) })
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                reply = parse(recv_frame(sock))
            except socket.timeout:
                break
            if not reply:
                break
            if 11 in reply:  # SubtitleEvent
                sub = parse(reply[11][0])
                kind = sub.get(3, [0])[0]
                text = sub.get(4, [b""])[0].decode()
                translated = sub.get(5, [b""])[0].decode()
                if kind == 2 and translated:
                    committed.append((text, translated))
            if 7 in reply:  # StopSessionResponse
                break
        sock.close()

        print(f"[chain] committed translated subtitles: {len(committed)}")
        for text, translated in committed:
            print(f"[chain]   {text!r} -> {translated!r}")
        assert committed, "no committed subtitle carried a translation"
        print("[chain] OK")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default="/tmp/translator-bridge-v1.sock")
    ap.add_argument("--source", default="en")
    ap.add_argument("--target", default="zh-Hans")
    ap.add_argument("--core", help="path to translator-core binary for the full chain test")
    args = ap.parse_args()

    test_bridge(args.socket, args.source, args.target)
    if args.core:
        test_full_chain(args.core, args.source, args.target)

if __name__ == "__main__":
    main()
