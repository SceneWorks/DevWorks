"""Loopback TLS/basic-auth Git smart HTTP fixture; never contacts external services."""
import base64, http.server, os, ssl, subprocess, sys
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'
    def do_GET(self): self.handle_git()
    def do_POST(self): self.handle_git()
    def log_message(self, *_): pass
    def handle_git(self):
        # Bound each response explicitly: GnuTLS rejects an EOF-delimited TLS body
        # when this minimal fixture closes without a close_notify exchange.
        if self.headers.get('Authorization') != 'Basic '+base64.b64encode(b'fixture:fixture-auth-secret').decode():
            self.send_response(401); self.send_header('WWW-Authenticate','Basic realm="fixture"'); self.send_header('Content-Length','0'); self.end_headers(); return
        path, _, query=self.path.partition('?')
        length=int(self.headers.get('Content-Length','0'))
        body=self.rfile.read(length)
        env=os.environ.copy()
        for key in list(env):
            if key.startswith('GIT_'): del env[key]
        env.update(GIT_PROJECT_ROOT=sys.argv[1],GIT_HTTP_EXPORT_ALL='1',PATH_INFO=path,
                   QUERY_STRING=query,GIT_PROTOCOL=self.headers.get('Git-Protocol',''),REQUEST_METHOD=self.command,CONTENT_TYPE=self.headers.get('Content-Type',''),CONTENT_LENGTH=str(length),REMOTE_USER='fixture')
        result=subprocess.run(['git','http-backend'],input=body,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
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
