"""Decodes an rpc-v2-cbor request independently and answers with a CBOR document.

The decoder is written from RFC 8949 rather than from the Rust, so a request it can read
is evidence the encoder is right rather than evidence the two share a bug. It checks the
protocol headers and the /service/{Name}/operation/{Op} path too, and reports trailing
bytes -- a body that decodes but does not consume itself means a length went wrong.

    python3 scripts/verify-cbor-requests.py 9971 &
    awsc cloudwatch put-metric-data --namespace demo \
      --metric-data '[{"MetricName":"m","Value":1.5,"Timestamp":"2026-08-24T00:00:00Z"}]' \
      --region us-east-1 --endpoint-url http://127.0.0.1:9971 --no-paginate

Each request prints one JSON line: the path, the decoded body, and any problems. Pass
--no-paginate: the canned response always carries a nextToken, so a paginating operation
would otherwise loop forever.
"""
import sys, json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def dec(b, i=0):
    ib = b[i]; i += 1
    major, info = ib >> 5, ib & 0x1f
    def arg(i, info):
        if info < 24: return info, i
        if info == 24: return b[i], i + 1
        if info == 25: return int.from_bytes(b[i:i+2], 'big'), i + 2
        if info == 26: return int.from_bytes(b[i:i+4], 'big'), i + 4
        if info == 27: return int.from_bytes(b[i:i+8], 'big'), i + 8
        if info == 31: return None, i
        raise ValueError('reserved info %d' % info)
    if major == 0:
        n, i = arg(i, info); return n, i
    if major == 1:
        n, i = arg(i, info); return -1 - n, i
    if major in (2, 3):
        n, i = arg(i, info)
        v = b[i:i+n]; i += n
        return (v.decode('utf-8', 'replace') if major == 3 else '<%d bytes>' % n), i
    if major == 4:
        n, i = arg(i, info); out = []
        for _ in range(n):
            v, i = dec(b, i); out.append(v)
        return out, i
    if major == 5:
        n, i = arg(i, info); out = {}
        for _ in range(n):
            k, i = dec(b, i); v, i = dec(b, i); out[k] = v
        return out, i
    if major == 6:
        t, i = arg(i, info); v, i = dec(b, i); return {'__tag%d' % t: v}, i
    if major == 7:
        if info == 20: return False, i
        if info == 21: return True, i
        if info == 22: return None, i
        if info == 27: 
            import struct; return struct.unpack('>d', b[i:i+8])[0], i + 8
    raise ValueError('major %d info %d' % (major, info))

def head(major, n):
    b = major << 5
    if n < 24: return bytes([b | n])
    if n < 256: return bytes([b | 24, n])
    return bytes([b | 25]) + n.to_bytes(2, 'big')

def text(s):
    e = s.encode(); return head(3, len(e)) + e

class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get('Content-Length', 0))
        body = self.rfile.read(n) if n else b''
        ok = True
        problems = []
        if self.headers.get('smithy-protocol') != 'rpc-v2-cbor':
            problems.append('smithy-protocol=%r' % self.headers.get('smithy-protocol'))
        if self.headers.get('Content-Type') != 'application/cbor':
            problems.append('content-type=%r' % self.headers.get('Content-Type'))
        if self.headers.get('Accept') != 'application/cbor':
            problems.append('accept=%r' % self.headers.get('Accept'))
        parts = self.path.split('/')
        if len(parts) < 5 or parts[1] != 'service' or parts[3] != 'operation':
            problems.append('path=%r' % self.path)
        try:
            decoded, used = dec(body) if body else ({}, 0)
            if used != len(body):
                problems.append('trailing bytes: used %d of %d' % (used, len(body)))
        except Exception as e:
            decoded = None
            problems.append('undecodable body: %s' % e)
        print(json.dumps({'path': self.path, 'body': decoded, 'problems': problems}),
              flush=True)
        # A minimal but non-empty response, so the parse path is exercised too.
        out = head(5, 1) + text('nextToken') + text('page-2')
        self.send_response(200)
        self.send_header('Content-Type', 'application/cbor')
        self.send_header('smithy-protocol', 'rpc-v2-cbor')
        self.send_header('Content-Length', str(len(out)))
        self.end_headers()
        self.wfile.write(out)

ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), H).serve_forever()
