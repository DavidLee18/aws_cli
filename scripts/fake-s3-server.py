"""A minimal S3-compatible server, enough to exercise cp/mv/rm end to end.

Not a conformance oracle -- it exists so uploads, downloads, multipart and ranged reads
can be verified for real rather than only in dry-run.
"""
import re, sys, threading, hashlib, time, os

# Optional artificial latency, so concurrency shows up as wall-clock time.
DELAY = float(os.environ.get('FAKE_S3_DELAY', '0'))
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs, unquote

OBJECTS = {}          # (bucket, key) -> bytes
MTIMES  = {}          # (bucket, key) -> epoch seconds, so sync can be exercised
UPLOADS = {}          # upload_id -> {(part): bytes}
LOCK = threading.Lock()
COUNTER = [0]
INFLIGHT = [0]
PEAK = [0]

def iso(bucket, key):
    t = MTIMES.get((bucket, key), 0)
    return time.strftime('%Y-%m-%dT%H:%M:%S.000Z', time.gmtime(t))


def xml_escape(s):
    return s.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')

# Connections accepted vs requests served. The ratio is the whole point of connection
# pooling: without it they are equal, with it requests far exceed connections.
CONNECTIONS = [0]
REQUESTS = [0]
METHODS = {}
CLOSED_BY_SERVER = [0]


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *a): pass

    def setup(self):
        with LOCK:
            CONNECTIONS[0] += 1
        BaseHTTPRequestHandler.setup(self)

    def handle_one_request(self):
        with LOCK:
            REQUESTS[0] += 1
            INFLIGHT[0] += 1
            PEAK[0] = max(PEAK[0], INFLIGHT[0])
        try:
            if DELAY:
                time.sleep(DELAY)
            BaseHTTPRequestHandler.handle_one_request(self)
            with LOCK:
                METHODS[self.command] = METHODS.get(self.command, 0) + 1
                if self.close_connection:
                    CLOSED_BY_SERVER[0] += 1
        finally:
            with LOCK:
                INFLIGHT[0] -= 1

    def _split(self):
        u = urlparse(self.path)
        parts = unquote(u.path).lstrip('/').split('/', 1)
        bucket = parts[0]
        key = parts[1] if len(parts) > 1 else ''
        return bucket, key, parse_qs(u.query, keep_blank_values=True)

    def _send(self, code, body=b'', headers=None):
        self.send_response(code)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _error(self, code, s3code, msg):
        body = ('<?xml version="1.0"?><Error><Code>%s</Code><Message>%s</Message></Error>'
                % (s3code, xml_escape(msg))).encode()
        self._send(code, body, {'Content-Type': 'application/xml'})

    def do_PUT(self):
        bucket, key, q = self._split()
        length = int(self.headers.get('Content-Length', 0))
        data = self.rfile.read(length) if length else b''
        with LOCK:
            if 'partNumber' in q:
                uid = q['uploadId'][0]
                UPLOADS.setdefault(uid, {})[int(q['partNumber'][0])] = data
                etag = '"%s"' % hashlib.md5(data).hexdigest()
                return self._send(200, b'', {'ETag': etag})
            if not key:
                return self._send(200)   # CreateBucket
            src = self.headers.get('x-amz-copy-source')
            if src:
                sb, sk = unquote(src).lstrip('/').split('/', 1)
                if (sb, sk) not in OBJECTS:
                    return self._error(404, 'NoSuchKey', 'The specified key does not exist.')
                data = OBJECTS[(sb, sk)]
                OBJECTS[(bucket, key)] = data
                MTIMES[(bucket, key)] = time.time()
                body = ('<?xml version="1.0"?><CopyObjectResult><ETag>"%s"</ETag>'
                        '</CopyObjectResult>' % hashlib.md5(data).hexdigest()).encode()
                return self._send(200, body, {'Content-Type': 'application/xml'})
            OBJECTS[(bucket, key)] = data
            MTIMES[(bucket, key)] = time.time()
        self._send(200, b'', {'ETag': '"%s"' % hashlib.md5(data).hexdigest()})

    def do_POST(self):
        bucket, key, q = self._split()
        length = int(self.headers.get('Content-Length', 0))
        if length: self.rfile.read(length)
        with LOCK:
            if 'uploads' in q:
                COUNTER[0] += 1
                uid = 'upload-%d' % COUNTER[0]
                UPLOADS[uid] = {}
                body = ('<?xml version="1.0"?><InitiateMultipartUploadResult>'
                        '<Bucket>%s</Bucket><Key>%s</Key><UploadId>%s</UploadId>'
                        '</InitiateMultipartUploadResult>' % (bucket, xml_escape(key), uid)).encode()
                return self._send(200, body, {'Content-Type': 'application/xml'})
            if 'uploadId' in q:
                uid = q['uploadId'][0]
                parts = UPLOADS.pop(uid, {})
                OBJECTS[(bucket, key)] = b''.join(parts[n] for n in sorted(parts))
                MTIMES[(bucket, key)] = time.time()
                body = ('<?xml version="1.0"?><CompleteMultipartUploadResult>'
                        '<Bucket>%s</Bucket><Key>%s</Key></CompleteMultipartUploadResult>'
                        % (bucket, xml_escape(key))).encode()
                return self._send(200, body, {'Content-Type': 'application/xml'})
        self._error(400, 'BadRequest', 'unsupported POST')

    def do_HEAD(self):
        bucket, key, q = self._split()
        with LOCK:
            data = OBJECTS.get((bucket, key))
        if data is None:
            return self._send(404)
        self._send(200, b'', {'Content-Length': str(len(data)),
                              'Last-Modified': time.strftime(
                                  '%a, %d %b %Y %H:%M:%S GMT',
                                  time.gmtime(MTIMES.get((bucket, key), 0)))})

    def do_GET(self):
        bucket, key, q = self._split()
        if self.path.startswith('/__stats'):
            # Reported before this request is counted out, so subtract the /__stats call
            # itself and the connection it arrived on.
            body = ('connections=%d requests=%d server_closed=%d methods=%s'
                    % (CONNECTIONS[0] - 1, REQUESTS[0] - 1, CLOSED_BY_SERVER[0],
                       sorted((str(k), v) for k, v in METHODS.items()))).encode()
            return self._send(200, body)
        if self.path.startswith('/__peak'):
            body = str(PEAK[0]).encode()
            PEAK[0] = 0
            return self._send(200, body)
        with LOCK:
            if 'list-type' in q:
                prefix = q.get('prefix', [''])[0]
                delim = q.get('delimiter', [None])[0]
                keys = sorted(k for (b, k) in OBJECTS if b == bucket and k.startswith(prefix))
                out = ['<?xml version="1.0"?><ListBucketResult><Name>%s</Name>'
                       '<IsTruncated>false</IsTruncated>' % bucket]
                prefixes = set()
                for k in keys:
                    rest = k[len(prefix):]
                    if delim and delim in rest:
                        prefixes.add(prefix + rest.split(delim)[0] + delim)
                        continue
                    out.append('<Contents><Key>%s</Key><Size>%d</Size>'
                               '<LastModified>%s</LastModified></Contents>'
                               % (xml_escape(k), len(OBJECTS[(bucket, k)]), iso(bucket, k)))
                for p in sorted(prefixes):
                    out.append('<CommonPrefixes><Prefix>%s</Prefix></CommonPrefixes>' % xml_escape(p))
                out.append('</ListBucketResult>')
                return self._send(200, ''.join(out).encode(), {'Content-Type': 'application/xml'})
            data = OBJECTS.get((bucket, key))
        if data is None:
            return self._error(404, 'NoSuchKey', 'The specified key does not exist.')
        rng = self.headers.get('Range')
        if rng:
            m = re.match(r'bytes=(\d+)-(\d+)', rng)
            start, end = int(m.group(1)), int(m.group(2))
            chunk = data[start:end + 1]
            return self._send(206, chunk, {'Content-Range': 'bytes %d-%d/%d' % (start, end, len(data))})
        self._send(200, data, {'Last-Modified': 'Thu, 13 Aug 2026 21:48:16 GMT'})

    def do_DELETE(self):
        bucket, key, q = self._split()
        with LOCK:
            if 'uploadId' in q:
                UPLOADS.pop(q['uploadId'][0], None)
                return self._send(204)
            OBJECTS.pop((bucket, key), None)
            MTIMES.pop((bucket, key), None)
        self._send(204)

if __name__ == '__main__':
    ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()
