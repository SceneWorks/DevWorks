"""Loopback TLS/basic-auth Git smart HTTP fixture; never contacts external services."""
import base64, http.server, os, ssl, subprocess, sys, time
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'
    def do_GET(self): self.handle_git()
    def do_POST(self): self.handle_git()
    def log_message(self, *_): pass
    def handle_git(self):
        started=time.monotonic()
        def trace(message): print(message, file=sys.stderr, flush=True)
        # Bound each response explicitly: GnuTLS rejects an EOF-delimited TLS body
        # when this minimal fixture closes without a close_notify exchange.
        if self.headers.get('Authorization') != 'Basic '+base64.b64encode(b'fixture:fixture-auth-secret').decode():
            trace(f"{self.command} challenge 401"); self.send_response(401); self.send_header('WWW-Authenticate','Basic realm="fixture"'); self.send_header('Content-Length','0'); self.end_headers(); return
        path, _, query=self.path.partition('?')
        length=int(self.headers.get('Content-Length','0'))
        body=self.rfile.read(length)
        # CGI gets only runtime necessities and this fixture's explicit server context.
        # Never inherit the developer's Git, SSH, signing or credential environment.
        env={key:os.environ[key] for key in ('PATH','SYSTEMROOT','WINDIR','TMPDIR','TEMP','TMP','LANG','LC_ALL') if key in os.environ}
        env.update(GIT_CONFIG_NOSYSTEM='1',GIT_CONFIG_SYSTEM=os.devnull,GIT_CONFIG_GLOBAL=os.devnull,
                   GIT_TERMINAL_PROMPT='0',HOME=os.path.dirname(sys.argv[2]),
                   USERPROFILE=os.path.dirname(sys.argv[2]),XDG_CONFIG_HOME=os.path.dirname(sys.argv[2]),OPENSSL_CONF=os.devnull,GIT_PROJECT_ROOT=sys.argv[1],GIT_HTTP_EXPORT_ALL='1',PATH_INFO=path,
                   QUERY_STRING=query,GIT_PROTOCOL=self.headers.get('Git-Protocol',''),REQUEST_METHOD=self.command,CONTENT_TYPE=self.headers.get('Content-Type',''),CONTENT_LENGTH=str(length),REMOTE_USER='fixture')
        result=subprocess.run(['git','http-backend'],input=body,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
        trace(f"{self.command} {path} backendExit={result.returncode} elapsedMs={int((time.monotonic()-started)*1000)}")
        if result.returncode:
            trace(result.stderr[:4096].decode(errors='replace').replace('fixture-auth-secret','[fixture secret]'))
        headers, _, payload=result.stdout.partition(b'\r\n\r\n')
        values=[];status=200
        for line in headers.decode().splitlines():
            key,value=line.split(':',1)
            if key.lower()=='status': status=int(value.strip().split()[0])
            else: values.append((key,value.strip()))
        self.send_response(status)
        for key,value in values: self.send_header(key,value)
        if not any(key.lower() == 'content-length' for key,_ in values):
            self.send_header('Content-Length',str(len(payload)))
        self.end_headers();self.wfile.write(payload)
server=http.server.HTTPServer(('127.0.0.1',0),Handler)
context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(sys.argv[2],sys.argv[3])
server.socket=context.wrap_socket(server.socket,server_side=True)
print(server.server_port,flush=True)
server.serve_forever()
