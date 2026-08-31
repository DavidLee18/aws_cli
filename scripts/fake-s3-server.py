"""A minimal S3-compatible server, enough to exercise cp/mv/rm end to end.

Not a conformance oracle -- it exists so uploads, downloads, multipart and ranged reads
can be verified for real rather than only in dry-run.
"""
import re, sys, threading, hashlib, time, os, base64
from xml.sax.saxutils import unescape

# Optional artificial latency, so concurrency shows up as wall-clock time.
DELAY = float(os.environ.get('FAKE_S3_DELAY', '0'))
# Any key containing this string is refused by DeleteObjects, to exercise partial failure.
DENY_KEY = os.environ.get('FAKE_S3_DENY_KEY', '')
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs, unquote

OBJECTS = {}          # (bucket, key) -> bytes
MTIMES  = {}          # (bucket, key) -> epoch seconds, so sync can be exercised
UPLOADS = {}          # upload_id -> {(part): bytes}
LOCK = threading.Lock()
COUNTER = [0]
INFLIGHT = [0]
PEAK = [0]
PART_SIZES = []       # every UploadPart body length, in arrival order

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
            INFLIGHT[0] += 1
            PEAK[0] = max(PEAK[0], INFLIGHT[0])
        try:
            if DELAY:
                time.sleep(DELAY)
            BaseHTTPRequestHandler.handle_one_request(self)
            # The last call on a kept-alive connection reads EOF and returns without
            # parsing anything, leaving `self.command` set to the *previous* request's
            # method. Counting that returns one phantom request per connection,
            # attributed to whatever came last -- which read as the client sending every
            # request twice. An empty request line is the only reliable "nothing was
            # read" signal.
            if not self.raw_requestline:
                return
            # The instrumentation endpoints are not workload; counting them means every
            # measurement includes the act of measuring.
            if self.path.startswith('/__'):
                return
            with LOCK:
                REQUESTS[0] += 1
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
                PART_SIZES.append(len(data))
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
        payload = self.rfile.read(length) if length else b''
        if 'delete' in q:
            return self._delete_objects(bucket, payload)
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

    def _delete_objects(self, bucket, payload):
        """DeleteObjects. Content-MD5 is verified, because that is the part a client
        gets wrong silently -- S3 rejects a bad digest and we should too."""
        want = self.headers.get('Content-MD5')
        if not want:
            return self._error(400, 'MissingContentMD5', 'Missing required header Content-MD5.')
        got = base64.b64encode(hashlib.md5(payload).digest()).decode()
        if want != got:
            return self._error(400, 'BadDigest', 'The Content-MD5 you specified did not match.')
        keys = re.findall(r'<Key>(.*?)</Key>', payload.decode('utf-8', 'replace'), re.S)
        keys = [unescape(k) for k in keys]
        if os.environ.get('FAKE_S3_LOG_DELETE'):
            print('DeleteObjects keys=%d' % len(keys), file=sys.stderr, flush=True)
        parts = ['<?xml version="1.0"?><DeleteResult>']
        with LOCK:
            for k in keys:
                # FAKE_S3_DENY_KEY exercises the partial-failure path: a 200 whose body
                # reports some keys as errors.
                if DENY_KEY and DENY_KEY in k:
                    parts.append('<Error><Key>%s</Key><Code>AccessDenied</Code>'
                                 '<Message>Access Denied</Message></Error>' % xml_escape(k))
                    continue
                OBJECTS.pop((bucket, k), None)
                MTIMES.pop((bucket, k), None)
                parts.append('<Deleted><Key>%s</Key></Deleted>' % xml_escape(k))
        parts.append('</DeleteResult>')
        self._send(200, ''.join(parts).encode(), {'Content-Type': 'application/xml'})

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
        if self.path.startswith('/__seed'):
            # Populate N synthetic objects so listing can be exercised without uploading
            # anything: /__seed?n=100000&bucket=b
            #
            # Keys are laid out as p/000000/, p/000001/, ... -- 1000 per sub-prefix --
            # so a seeded bucket has the sub-prefix structure that parallel listing
            # shards on. A flat keyspace would exercise only the sequential fallback.
            q = parse_qs(urlparse(self.path).query)
            n = int(q.get('n', ['1000'])[0])
            bucket = q.get('bucket', ['b'])[0]
            with LOCK:
                for i in range(n):
                    key = 'p/%06d/object-%06d.bin' % (i // 1000, i)
                    OBJECTS[(bucket, key)] = b'x' * 10
                    MTIMES[(bucket, key)] = 1700000000
            return self._send(200, b'seeded %d' % n)
        if self.path.startswith('/__stats'):
            # `/__` requests are excluded from the request and method counters, but the
            # connection this one arrived on was counted in setup(), so discount it.
            body = ('connections=%d requests=%d server_closed=%d methods=%s'
                    % (CONNECTIONS[0] - 1, REQUESTS[0], CLOSED_BY_SERVER[0],
                       sorted((str(k), v) for k, v in METHODS.items()))).encode()
            return self._send(200, body)
        if self.path.startswith('/__parts'):
            # Part sizes seen since the last read, which is how a `--multipart-chunksize`
            # is confirmed to have reached the wire rather than merely been parsed.
            body = ('count=%d sizes=%s' % (len(PART_SIZES), PART_SIZES)).encode()
            del PART_SIZES[:]
            return self._send(200, body)
        if self.path.startswith('/__peak'):
            body = str(PEAK[0]).encode()
            PEAK[0] = 0
            return self._send(200, body)
        with LOCK:
            if 'list-type' in q:
                prefix = q.get('prefix', [''])[0]
                delim = q.get('delimiter', [None])[0]
                # S3 caps a page at 1000 keys and truncates with a continuation token.
                # Modelling that matters: without it a large listing arrives in one
                # response and hides the sequential round trips that dominate real
                # listing cost.
                max_keys = min(int(q.get('max-keys', ['1000'])[0]), 1000)
                token = q.get('continuation-token', [None])[0]
                start_after = q.get('start-after', [None])[0]
                after = token or start_after

                keys = sorted(k for (b, k) in OBJECTS if b == bucket and k.startswith(prefix))
                if after:
                    keys = [k for k in keys if k > after]

                out = []
                prefixes = []
                seen_prefixes = set()
                emitted = 0
                last = None
                truncated = False
                for k in keys:
                    if emitted >= max_keys:
                        truncated = True
                        break
                    rest = k[len(prefix):]
                    if delim and delim in rest:
                        cp = prefix + rest.split(delim)[0] + delim
                        if cp not in seen_prefixes:
                            seen_prefixes.add(cp)
                            prefixes.append(cp)
                            emitted += 1
                            last = k
                        continue
                    out.append('<Contents><Key>%s</Key><Size>%d</Size>'
                               '<LastModified>%s</LastModified></Contents>'
                               % (xml_escape(k), len(OBJECTS[(bucket, k)]), iso(bucket, k)))
                    emitted += 1
                    last = k

                body = ['<?xml version="1.0"?><ListBucketResult><Name>%s</Name>' % bucket,
                        '<IsTruncated>%s</IsTruncated>' % ('true' if truncated else 'false')]
                body.extend(out)
                for cp in prefixes:
                    body.append('<CommonPrefixes><Prefix>%s</Prefix></CommonPrefixes>' % xml_escape(cp))
                if truncated and last is not None:
                    body.append('<NextContinuationToken>%s</NextContinuationToken>' % xml_escape(last))
                body.append('</ListBucketResult>')
                return self._send(200, ''.join(body).encode(), {'Content-Type': 'application/xml'})
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
