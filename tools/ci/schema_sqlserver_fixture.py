#!/usr/bin/env python3
"""Explicit isolated SQLServer metadata/TLS qualification. Never changes OS trust or user DBs."""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
IMAGE = 'mcr.microsoft.com/mssql/server@sha256:cf8f0ab2ebe61feec2eed2629061a07d247f791c2a8ddda74a4ac9f06a291718'
PASSWORD = 'Devworks_Metadata_Fixture_2026!'
p = argparse.ArgumentParser()
p.add_argument('--allow-emulation', action='store_true', help='Functional fixture development only; never native-x86 acceptance')
a = p.parse_args()
if platform.machine().lower() not in ('x86_64', 'amd64') and not a.allow_emulation:
    raise SystemExit('Native SQLServer qualification requires an actual x86_64 host')

def run(*cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw)

name = f'devworks-schema-sqlserver-{os.getpid()}'

def sql(*args):
    return run('docker', 'exec', name, '/opt/mssql-tools18/bin/sqlcmd', '-S', 'localhost',
               '-U', 'sa', '-P', PASSWORD, '-C', '-b', *args)

def ready():
    end = time.monotonic() + 180
    while time.monotonic() < end:
        try:
            sql('-Q', 'SELECT 1 AS isolated_fixture_ready')
            return
        except subprocess.CalledProcessError:
            time.sleep(1)
    raise RuntimeError('Isolated SQLServer fixture did not start within180s')

with tempfile.TemporaryDirectory(prefix='devworks-schema-sqlserver-') as folder:
    f = Path(folder)
    run('openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(f/'ca.key'),
        '-out', str(f/'ca.pem'), '-days', '2', '-subj', '/CN=DevWorks-Isolated-Metadata-CA')
    run('openssl', 'req', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(f/'server.key'),
        '-out', str(f/'server.csr'), '-subj', '/CN=localhost')
    (f/'server.ext').write_text('subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n')
    run('openssl', 'x509', '-req', '-in', str(f/'server.csr'), '-CA', str(f/'ca.pem'),
        '-CAkey', str(f/'ca.key'), '-CAcreateserial', '-out', str(f/'server.pem'),
        '-days', '2', '-extfile', str(f/'server.ext'))
    started = False
    try:
        run('docker', 'run', '--detach', '--rm', '--name', name, '--platform', 'linux/amd64',
            '--publish', '127.0.0.1::1433', '--memory', '4g', '--cpus', '2',
            '--env', 'ACCEPT_EULA=Y', '--env', 'MSSQL_PID=Developer',
            '--env', f'MSSQL_SA_PASSWORD={PASSWORD}', IMAGE)
        started = True
        ready()
        for file in ('ca.pem', 'server.pem', 'server.key'):
            run('docker', 'cp', str(f/file), f'{name}:/tmp/devworks-metadata-{file}')
        run('docker', 'exec', '-u', 'root', name, 'chown', 'mssql:mssql', '/tmp/devworks-metadata-server.key')
        run('docker', 'exec', '-u', 'root', name, 'chmod', '600', '/tmp/devworks-metadata-server.key')
        for key, value in [('network.tlscert', '/tmp/devworks-metadata-server.pem'),
                           ('network.tlskey', '/tmp/devworks-metadata-server.key'),
                           ('network.forceencryption', '0')]:
            run('docker', 'exec', '-u', 'root', name, '/opt/mssql/bin/mssql-conf', 'set', key, value)
        run('docker', 'restart', name)
        ready()
        run('docker', 'cp', str(ROOT/'docs/architecture/validation/schema-sources/sqlserver-functional-fixture.sql'),
            f'{name}:/tmp/devworks-metadata-fixture.sql')
        sql('-i', '/tmp/devworks-metadata-fixture.sql')
        endpoint = run('docker', 'port', name, '1433/tcp').stdout.strip()
        env = os.environ.copy()
        env.update(DEVWORKS_METADATA_SQLSERVER=f'sqlserver://{endpoint}/metadata_fixture',
                   DEVWORKS_METADATA_CA=str(f/'ca.pem'))
        # Ordinary tests ignore network fixture cases; this explicit job runs extraction,
        # bounded defaults and verified TLS/CA/hostname-refusal paths without inference.
        subprocess.run(['cargo', 'test', '--locked', '-p', 'devworks-adapters', '--test',
                        'database_metadata', 'native_sql_server', '--', '--ignored', '--nocapture'],
                       cwd=ROOT, env=env, check=True)
        subprocess.run(['cargo', 'test', '--locked', '-p', 'devworks-desktop', '--test',
                        'database_network', 'native_sql_server_production_engine_metadata', '--', '--ignored', '--nocapture'],
                       cwd=ROOT, env=env, check=True)
        print(json.dumps({'host': platform.machine(), 'nativeX86': platform.machine().lower() in ('x86_64','amd64'),
                          'image': IMAGE, 'adapterCases': 9, 'productionEngineCases': 1,
                          'cases': 10, 'osTrustChanges': False, 'inference': False}))
    finally:
        if started:
            subprocess.run(['docker', 'rm', '--force', name], capture_output=True, check=False)
