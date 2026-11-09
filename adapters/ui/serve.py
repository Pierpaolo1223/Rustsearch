"""Static server for the playground, with byte ranges.

`python -m http.server` ignores `Range` and answers `200` with the whole object, so a
sharded index (`?shards=demo/shards/manifest.txt`) would fetch `texts.bin` once per
hit instead of the few hundred bytes the hit needs. This handler serves a single
range as `206` with `Content-Range`, advertises `Accept-Ranges: bytes`, and otherwise
is `SimpleHTTPRequestHandler`.

    python3 serve.py [root] [port]

The root defaults to this file's directory, not to the current one: launched from the
repository root, the page has to be `adapters/ui/index.html`, not a listing of the
checkout.
"""

import os
import re
import sys
from functools import partial
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

RANGE = re.compile(r"^bytes=(\d*)-(\d*)$")


class RangeHandler(SimpleHTTPRequestHandler):
    range_length = None

    def parse_range(self, size):
        """`(start, end)`, `None` for no range, or `"invalid"` for a bad one."""
        header = self.headers.get("Range")
        if not header:
            return None
        match = RANGE.match(header.strip())
        if not match:
            return None
        first, last = match.groups()
        if first == "" and last == "":
            return None
        if first == "":
            length = int(last)
            if length == 0:
                return None
            start, end = max(0, size - length), size - 1
        else:
            start = int(first)
            end = int(last) if last else size - 1
        if start >= size or end < start:
            return "invalid"
        return start, min(end, size - 1)

    def send_head(self):
        self.range_length = None
        path = self.translate_path(self.path)
        if os.path.isdir(path):
            return super().send_head()
        if not os.path.exists(path):
            self.send_error(HTTPStatus.NOT_FOUND, "File not found")
            return None
        size = os.path.getsize(path)
        parsed = self.parse_range(size)
        if parsed == "invalid":
            self.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
            self.send_header("Content-Range", f"bytes */{size}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return None
        if parsed is None:
            return super().send_head()
        start, end = parsed
        try:
            handle = open(path, "rb")
        except OSError:
            self.send_error(HTTPStatus.NOT_FOUND, "File not found")
            return None
        handle.seek(start)
        self.range_length = end - start + 1
        self.send_response(HTTPStatus.PARTIAL_CONTENT)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(self.range_length))
        self.end_headers()
        return handle

    def copyfile(self, source, outputfile):
        if self.range_length is None:
            return super().copyfile(source, outputfile)
        remaining = self.range_length
        while remaining > 0:
            chunk = source.read(min(64 * 1024, remaining))
            if not chunk:
                break
            outputfile.write(chunk)
            remaining -= len(chunk)

    def end_headers(self):
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Cache-Control", "no-store, must-revalidate")
        super().end_headers()


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__))
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8765
    server = ThreadingHTTPServer(("127.0.0.1", port), partial(RangeHandler, directory=root))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
