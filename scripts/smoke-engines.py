"""Windows named-pipe smoke test against installed engine packages (no third-party Python packages).
Usage: python scripts/smoke-engines.py --models C:/path/to/models [--engine cuda]
"""
import argparse, os, struct, subprocess, time, wave
from pathlib import Path


def varint(n):
    data = bytearray()
    while n > 127:
        data.append((n & 127) | 128); n >>= 7
    data.append(n)
    return bytes(data)


def field(n, value):
    if isinstance(value, str): value = value.encode('utf-8')
    if isinstance(value, bytes): return varint(n * 8 + 2) + varint(len(value)) + value
    return varint(n * 8) + varint(value)


def fields(data):
    pos = 0
    def readint():
        nonlocal pos
        n = shift = 0
        while True:
            b = data[pos]; pos += 1; n |= (b & 127) << shift
            if b < 128: return n
            shift += 7
    result = {}
    while pos < len(data):
        tag = readint(); key, wire = tag >> 3, tag & 7
        if wire == 0: value = readint()
        elif wire == 2:
            size = readint(); value = data[pos:pos+size]; pos += size
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4; value = data[pos:pos+size]; pos += size
        else: raise ValueError(f'Bad protobuf wire type {wire}')
        result.setdefault(key, []).append(value)
    return result


def verify(engine, models, root):
    binary = root / 'engines' / engine / 'translator-core.exe'
    pipe = f'translator-smoke-{engine}-{os.getpid()}'
    logs = root / 'artifacts' / 'engine-tests'; logs.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env['PATH'] = str(Path(env.get('CUDA_PATH', '')) / 'bin') + os.pathsep + env['PATH']
    with (logs / f'{engine}.log').open('w', encoding='utf-8') as log:
        proc = subprocess.Popen([str(binary), '--pipe', '\\\\.\\pipe\\' + pipe], env=env,
                                stdout=log, stderr=log, creationflags=subprocess.CREATE_NO_WINDOW)
        fd = None
        try:
            deadline = time.monotonic() + 15
            while fd is None:
                try: fd = os.open('\\\\.\\pipe\\' + pipe, os.O_RDWR | os.O_BINARY)
                except OSError:
                    if proc.poll() is not None or time.monotonic() > deadline: raise RuntimeError(f'{engine} did not start; see {log.name}')
                    time.sleep(.1)
            seq = 0
            def send(tag, data):
                nonlocal seq
                seq += 1
                msg = field(1, seq) + field(tag, data)
                os.write(fd, struct.pack('>I', len(msg)) + msg)
            def readexact(size):
                data = b''
                while len(data) < size:
                    part = os.read(fd, size-len(data))
                    if not part: raise EOFError()
                    data += part
                return data
            def receive():
                return fields(readexact(struct.unpack('>I', readexact(4))[0]))
            def until(tag):
                while True:
                    msg = receive()
                    if tag in msg: return fields(msg[tag][0])
            send(2, field(1, 1)); hello = until(3)
            assert f'asr.whisper.engine.{engine}'.encode() in hello.get(4, []), hello
            fmt = field(1, 16000) + field(2, 1) + field(3, 2)
            def start(backend):
                asr = field(1, 'whisper') + field(2, 'ggml-tiny.bin') + field(3, 'en') + field(4, backend) + field(5, str(models))
                send(4, field(1, 'test') + field(2, fmt) + field(4, asr))
                return until(5)
            rejected = start('not-installed')
            assert rejected.get(1, [0])[0] == 0, rejected
            accepted = start(engine)
            assert accepted.get(1, [0])[0] == 1, accepted
            with wave.open(str(root / 'crates/translator-core/tests/speech_test.wav')) as wav:
                assert wav.getnchannels() == 1 and wav.getsampwidth() == 2 and wav.getframerate() == 16000
                pcm = wav.readframes(wav.getnframes())
            # The short fixture fits the pipe buffer. Stop flushes final recognition.
            for offset in range(0, len(pcm), 3200):
                chunk = pcm[offset:offset+3200]
                send(10, field(1, 'test') + field(2, offset // 2 * 1000000 // 16000) + field(3, fmt) + field(4, len(chunk)//2) + field(5, chunk))
            send(6, field(1, 'test'))
            captions = []
            while True:
                event = receive()
                if 11 in event:
                    caption = fields(event[11][0]).get(4, [b''])[0].decode('utf-8')
                    if caption: captions.append(caption)
                if 7 in event: break
            assert captions, f'{engine} produced no captions'
            print(f'{engine}: capability + mismatch rejection + model load + transcription PASS: {captions[-1]}', flush=True)
        finally:
            if fd is not None: os.close(fd)
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait()


if __name__ == '__main__':
    args = argparse.ArgumentParser()
    args.add_argument('--models', required=True, type=Path)
    args.add_argument('--engine', choices=['cpu', 'blas', 'vulkan', 'cuda'])
    opts = args.parse_args()
    root = Path(__file__).resolve().parent.parent
    for engine in [opts.engine] if opts.engine else ['cpu', 'blas', 'vulkan', 'cuda']:
        verify(engine, opts.models.resolve(), root)
