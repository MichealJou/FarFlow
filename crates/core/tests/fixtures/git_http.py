"""仅供集成测试：标准库 HTTP 入口委托系统 git-http-backend。"""
import base64
import email.parser
import http.server
import os
import subprocess
import sys
import urllib.parse

project_root, backend, port = sys.argv[1:]
expected = "Basic " + base64.b64encode(b"ff-test:only-test-password").decode()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.serve_git()

    def do_POST(self):
        self.serve_git()

    def serve_git(self):
        if self.headers.get("Authorization") != expected:
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="FarFlow integration test"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        route = urllib.parse.urlsplit(self.path)
        environment = os.environ.copy()
        environment.update(
            GIT_PROJECT_ROOT=project_root,
            GIT_HTTP_EXPORT_ALL="1",
            REMOTE_USER="ff-test",
            REQUEST_METHOD=self.command,
            PATH_INFO=route.path,
            QUERY_STRING=route.query,
            CONTENT_TYPE=self.headers.get("Content-Type", ""),
            CONTENT_LENGTH=self.headers.get("Content-Length", "0"),
        )
        body = self.rfile.read(int(environment["CONTENT_LENGTH"]))
        completed = subprocess.run(
            [backend], input=body, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        headers, separator, body = completed.stdout.partition(b"\r\n\r\n")
        if not separator:
            headers, separator, body = completed.stdout.partition(b"\n\n")
        if not separator:
            self.send_error(500)
            return
        parsed = email.parser.BytesHeaderParser().parsebytes(headers + b"\r\n\r\n")
        self.send_response(int(parsed.get("Status", "200").split()[0]))
        for key, content in parsed.items():
            if key.lower() not in ("status", "content-length", "connection"):
                self.send_header(key, content)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


http.server.HTTPServer(("127.0.0.1", int(port)), Handler).serve_forever()
