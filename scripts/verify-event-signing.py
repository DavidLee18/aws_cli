"""A server that independently verifies AWS event-stream request signing.

Implemented from the SigV4 spec rather than from the Rust code, so agreement between the
two is evidence and not a tautology. Every frame's :chunk-signature is recomputed here; a
wrong string-to-sign on either side shows up as a mismatch.

It also answers before the request body ends, and sends one response event per verified
request frame, which is what makes the client's read-while-still-writing observable in
the timestamps rather than merely assumed.

    python3 scripts/verify-event-signing.py 9961 &
    export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
    export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
    printf '%s\n' '{"TextEvent": {"Text": "hello", "TextType": "text"}}' |
      awsc polly start-speech-synthesis-stream --engine standard --output-format pcm \
        --voice-id Joanna --region us-east-1 --endpoint-url http://127.0.0.1:9961

The credentials above are AWS's published example pair, used by every SigV4 test vector;
they authenticate nothing. Running the same command with a different secret must be
rejected — that negative control is what shows the verification is load-bearing.
"""
import sys, hmac, hashlib, zlib, json, struct, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

AK = 'AKIAIOSFODNN7EXAMPLE'
SK = 'wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY'

def hmac_sha(key, msg):
    return hmac.new(key, msg.encode() if isinstance(msg, str) else msg, hashlib.sha256).digest()

def signing_key(date, region, service):
    k = hmac_sha(('AWS4' + SK).encode(), date)
    k = hmac_sha(k, region)
    k = hmac_sha(k, service)
    return hmac_sha(k, 'aws4_request')

# ---- event-stream framing ----
def parse_headers(buf):
    out, i = [], 0
    while i < len(buf):
        n = buf[i]; i += 1
        name = buf[i:i+n].decode(); i += n
        t = buf[i]; i += 1
        if t in (0, 1): val = (t == 0)
        elif t == 2: val = struct.unpack('>b', buf[i:i+1])[0]; i += 1
        elif t == 3: val = struct.unpack('>h', buf[i:i+2])[0]; i += 2
        elif t == 4: val = struct.unpack('>i', buf[i:i+4])[0]; i += 4
        elif t == 5: val = struct.unpack('>q', buf[i:i+8])[0]; i += 8
        elif t in (6, 7):
            ln = struct.unpack('>H', buf[i:i+2])[0]; i += 2
            val = buf[i:i+ln]; i += ln
            if t == 7: val = val.decode()
        elif t == 8: val = struct.unpack('>q', buf[i:i+8])[0]; i += 8
        elif t == 9: val = buf[i:i+16]; i += 16
        else: raise ValueError('bad header type %d' % t)
        out.append((name, val, t))
    return out

def encode_header(name, t, val):
    b = bytes([len(name)]) + name.encode() + bytes([t])
    if t == 8: return b + struct.pack('>q', val)
    if t == 7: return b + struct.pack('>H', len(val)) + val.encode()
    if t == 6: return b + struct.pack('>H', len(val)) + val
    raise ValueError(t)

def frame(headers, payload):
    hb = b''.join(encode_header(n, t, v) for n, t, v in headers)
    total = 12 + len(hb) + len(payload) + 4
    pre = struct.pack('>II', total, len(hb))
    pre += struct.pack('>I', zlib.crc32(pre) & 0xffffffff)
    body = pre + hb + payload
    return body + struct.pack('>I', zlib.crc32(body) & 0xffffffff)

def split_frames(buf):
    out, i = [], 0
    while i + 12 <= len(buf):
        total, hlen = struct.unpack('>II', buf[i:i+8])
        if i + total > len(buf): break
        f = buf[i:i+total]
        assert zlib.crc32(f[:8]) & 0xffffffff == struct.unpack('>I', f[8:12])[0], 'prelude crc'
        assert zlib.crc32(f[:-4]) & 0xffffffff == struct.unpack('>I', f[-4:])[0], 'message crc'
        out.append((f[12:12+hlen], f[12+hlen:total-4]))
        i += total
    return out, buf[i:]

def iso_from_ms(ms):
    return time.strftime('%Y%m%dT%H%M%SZ', time.gmtime(ms // 1000))

PROBLEMS = []
VERIFIED = [0]

class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass

    def do_POST(self):
        auth = self.headers.get('Authorization', '')
        try:
            scope, prior = self.verify_request(auth)
        except AssertionError as e:
            PROBLEMS.append('initial request: %s' % e)
            return self.fail(str(e))

        region, service = scope.split('/')[1], scope.split('/')[2]
        key = signing_key(scope.split('/')[0], region, service)

        # Respond as soon as the headers are in, before the request body is finished --
        # if the client waited for its own body to end, this would deadlock.
        out = frame([(':message-type', 7, 'event'), (':event-type', 7, 'AudioEvent'),
                     (':content-type', 7, 'application/octet-stream')], b'\x01\x02\x03')
        self.send_response(200)
        self.send_header('Content-Type', 'application/vnd.amazon.eventstream')
        self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()
        self.write_chunk(out)

        buf, seen = b'', 0
        while True:
            chunk = self.read_chunk()
            if chunk is None:
                break
            buf += chunk
            frames, buf = split_frames(buf)
            for hb, payload in frames:
                seen += 1
                try:
                    prior = self.verify_frame(hb, payload, key, scope, prior)
                    VERIFIED[0] += 1
                except AssertionError as e:
                    PROBLEMS.append('frame %d: %s' % (seen, e))
                if payload:
                    inner = split_frames(payload)[0]
                    ih = dict((n, v) for n, v, _ in parse_headers(inner[0][0]))
                    print('%.3f frame %d verified: event-type=%s payload=%r'
                          % (time.time(), seen, ih.get(':event-type'), inner[0][1][:60]),
                          file=sys.stderr, flush=True)
                    # One response per request frame, written the moment it is verified:
                    # this is what makes the client's read-while-writing observable.
                    self.write_chunk(frame(
                        [(':message-type', 7, 'event'), (':event-type', 7, 'AudioEvent'),
                         (':content-type', 7, 'application/octet-stream')],
                        b'reply-%d' % seen))
                else:
                    print('frame %d verified: end-of-stream (empty payload)' % seen,
                          file=sys.stderr, flush=True)

        closed = frame([(':message-type', 7, 'event'), (':event-type', 7, 'StreamClosedEvent'),
                        (':content-type', 7, 'application/json')],
                       json.dumps({'RequestCharacters': seen}).encode())
        self.write_chunk(closed)
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()
        print('TOTAL frames=%d verified=%d problems=%r' % (seen, VERIFIED[0], PROBLEMS),
              file=sys.stderr, flush=True)

    def write_chunk(self, data):
        self.wfile.write(('%x\r\n' % len(data)).encode() + data + b'\r\n')
        self.wfile.flush()

    def read_chunk(self):
        line = self.rfile.readline()
        if not line:
            return None
        n = int(line.strip().split(b';')[0], 16)
        if n == 0:
            self.rfile.readline()
            return None
        data = self.rfile.read(n)
        self.rfile.readline()
        return data

    def verify_request(self, auth):
        assert auth.startswith('AWS4-HMAC-SHA256 '), 'not sigv4'
        parts = dict(p.strip().split('=', 1) for p in auth[len('AWS4-HMAC-SHA256 '):].split(','))
        cred = parts['Credential']
        scope = cred.split('/', 1)[1]
        signed = parts['SignedHeaders'].split(';')
        given = parts['Signature']

        canonical_headers = ''.join(
            '%s:%s\n' % (h, ' '.join(self.headers.get(h, '').split()))
            for h in signed)
        path = self.path.split('?')[0]
        query = self.path.split('?')[1] if '?' in self.path else ''
        payload_hash = self.headers.get('x-amz-content-sha256')
        assert payload_hash == 'STREAMING-AWS4-HMAC-SHA256-EVENTS', \
            'payload hash is %r, expected the event-stream sentinel' % payload_hash
        creq = '\n'.join(['POST', path, query, canonical_headers,
                          ';'.join(signed), payload_hash])
        amzdate = self.headers.get('x-amz-date')
        sts = '\n'.join(['AWS4-HMAC-SHA256', amzdate, scope,
                         hashlib.sha256(creq.encode()).hexdigest()])
        key = signing_key(scope.split('/')[0], scope.split('/')[1], scope.split('/')[2])
        expect = hmac.new(key, sts.encode(), hashlib.sha256).hexdigest()
        assert expect == given, 'initial signature mismatch\n  canonical=%r' % creq
        print('initial request signature OK', file=sys.stderr, flush=True)
        return scope, given

    def verify_frame(self, header_block, payload, key, scope, prior):
        headers = parse_headers(header_block)
        by_name = {n: (v, t) for n, v, t in headers}
        assert ':date' in by_name, 'frame has no :date header'
        assert ':chunk-signature' in by_name, 'frame has no :chunk-signature header'
        ms = by_name[':date'][0]
        given = by_name[':chunk-signature'][0].hex()

        # The signed header block is :date alone -- the signature cannot sign itself.
        date_block = encode_header(':date', 8, ms)
        sts = '\n'.join([
            'AWS4-HMAC-SHA256-PAYLOAD',
            iso_from_ms(ms),
            scope,
            prior,
            hashlib.sha256(date_block).hexdigest(),
            hashlib.sha256(payload).hexdigest(),
        ])
        expect = hmac.new(key, sts.encode(), hashlib.sha256).hexdigest()
        assert expect == given, 'chunk signature mismatch\n  sts=%r' % sts
        return given

    def fail(self, message):
        body = json.dumps({'__type': 'SignatureDoesNotMatch', 'message': message}).encode()
        self.send_response(403)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), H).serve_forever()
