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
# Each image is pinned by digest; the key names the qualified release.
IMAGES = {
    '2017': 'mcr.microsoft.com/mssql/server:2017-latest@sha256:5f6fd78f8752390dbd3584b91dee463f1f4b2b30beef9af3fd35753d32940c52',
    '2019': 'mcr.microsoft.com/mssql/server:2019-latest@sha256:debdc21628cddd1a4433371c601c8dee9bad3533ae8b18fe9dc8c5596368b10d',
    '2022': 'mcr.microsoft.com/mssql/server@sha256:cf8f0ab2ebe61feec2eed2629061a07d247f791c2a8ddda74a4ac9f06a291718',
    '2025': 'mcr.microsoft.com/mssql/server:2025-latest@sha256:05644dad1266ca3d445d1e8f6d2ebb9a6a03df9a39376e0f7b48917356dca9d9',
}
PASSWORD = 'Devworks_Metadata_Fixture_2026!'
# Files copied into the database container live under this prefix inside it.
IN_CONTAINER = '/tmp/devworks-metadata'  # dev-host: allow-path a path inside the database container, not on the host
p = argparse.ArgumentParser()
p.add_argument('--allow-emulation', action='store_true', help='Functional fixture development only; never native-x86 acceptance')
p.add_argument('--version', choices=sorted(IMAGES), default='2022')
p.add_argument('--data-only', action='store_true', help='Test the SQL Server 2022 data adapter')
a = p.parse_args()
if a.data_only and a.version != '2022':
    p.error('--data-only requires SQL Server 2022')
IMAGE = IMAGES[a.version]
if platform.machine().lower() not in ('x86_64', 'amd64') and not a.allow_emulation:
    raise SystemExit('Native SQLServer qualification requires an actual x86_64 host')

def run(*cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw)

name = f'devworks-schema-sqlserver-{os.getpid()}'

def sqlcmd():
    # 2017 images ship only the ODBC 17 tools; later images ship mssql-tools18, whose
    # client encrypts by default and needs -C for the fixture's own certificate.
    try:
        run('docker', 'exec', name, 'test', '-x', '/opt/mssql-tools18/bin/sqlcmd')
        return ['/opt/mssql-tools18/bin/sqlcmd', '-C']
    except subprocess.CalledProcessError:
        return ['/opt/mssql-tools/bin/sqlcmd']

def sql(*args):
    return run('docker', 'exec', name, *sqlcmd(), '-S', 'localhost',
               '-U', 'sa', '-P', PASSWORD, '-b', *args)

def ready():
    end = time.monotonic() + 180
    while time.monotonic() < end:
        try:
            sql('-Q', 'SELECT 1 AS isolated_fixture_ready')
            return
        except subprocess.CalledProcessError:
            time.sleep(1)
    raise RuntimeError('Isolated SQLServer fixture did not start within180s')

def start():
    run('docker', 'run', '--detach', '--rm', '--name', name, '--platform', 'linux/amd64',
        '--publish', '127.0.0.1::1433', '--memory', '4g', '--cpus', '2',
        '--env', 'ACCEPT_EULA=Y', '--env', 'MSSQL_PID=Developer',
        '--env', f'MSSQL_SA_PASSWORD={PASSWORD}', IMAGE)

def qualify_data():
    started = False
    try:
        start()
        started = True
        ready()
        sql('-Q', "CREATE DATABASE dw37_fixture")
        sql('-d', 'dw37_fixture', '-Q', """
            CREATE TABLE dbo.dw_public(id int NOT NULL PRIMARY KEY, secret varchar(40) NOT NULL);
            INSERT dbo.dw_public VALUES(1,'public'),(2,'next');
            CREATE TABLE dbo.dw_private(id int NOT NULL PRIMARY KEY, secret varchar(40) NOT NULL);
            INSERT dbo.dw_private VALUES(1,'private');
            CREATE TABLE dbo.dw_nullable_integers(
                id int NOT NULL PRIMARY KEY,
                tiny_value tinyint NULL, small_value smallint NULL,
                int_value int NULL, big_value bigint NULL
            );
            INSERT dbo.dw_nullable_integers VALUES
                (1,42,-42,-42000,4200000000),
                (2,0,-32768,-2147483648,-9223372036854775808),
                (3,255,32767,2147483647,9223372036854775807),
                (4,NULL,NULL,NULL,NULL);
            CREATE TABLE dbo.dw_check_digest (
                id int NOT NULL PRIMARY KEY,
                CONSTRAINT ck_dw_check_digest_id CHECK (id > 0)
            );
            INSERT dbo.dw_check_digest VALUES (1);
        """)
        for statement in (
            'CREATE VIEW dbo.dw_view AS SELECT secret FROM dbo.dw_private',
            'CREATE VIEW dbo.dw_allowed AS SELECT id AS item FROM dbo.dw_public',
            'CREATE VIEW dbo.dw_allowed_nested AS SELECT item FROM dbo.dw_allowed',
        ):
            sql('-d', 'dw37_fixture', '-Q', statement)
        endpoint = run('docker', 'port', name, '1433/tcp').stdout.strip()
        env = os.environ.copy()
        env.update(DW37_TARGET=f'sqlserver://{endpoint}/dw37_fixture',
                   DW37_USER='sa', DW37_PASSWORD=PASSWORD)
        subprocess.run(['cargo', 'test', '--locked', '-p', 'devworks-adapters', '--test',
                        'database_data', 'sql_server_data', '--', '--exact', '--ignored'],
                       cwd=ROOT, env=env, check=True)
        print(json.dumps({'version': a.version, 'host': platform.machine(),
                          'nativeX86': platform.machine().lower() in ('x86_64','amd64'),
                          'image': IMAGE, 'dataCases': 1, 'inference': False}))
    finally:
        if started:
            subprocess.run(['docker', 'rm', '--force', name], capture_output=True, check=False)

if a.data_only:
    qualify_data()
    raise SystemExit(0)

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
        start()
        started = True
        ready()
        for file in ('ca.pem', 'server.pem', 'server.key'):
            run('docker', 'cp', str(f/file), f'{name}:{IN_CONTAINER}-{file}')
        # 2017 images run SQL Server as root with no mssql user; 2019 has the user but no
        # mssql group, so change only the owner.
        run('docker', 'exec', '-u', 'root', name, 'sh', '-c',
            f'if id mssql >/dev/null 2>&1; then chown mssql {IN_CONTAINER}-server.key; fi')
        run('docker', 'exec', '-u', 'root', name, 'chmod', '600', f'{IN_CONTAINER}-server.key')
        for key, value in [('network.tlscert', f'{IN_CONTAINER}-server.pem'),
                           ('network.tlskey', f'{IN_CONTAINER}-server.key'),
                           ('network.forceencryption', '0')]:
            run('docker', 'exec', '-u', 'root', name, '/opt/mssql/bin/mssql-conf', 'set', key, value)
        run('docker', 'restart', name)
        ready()
        run('docker', 'cp', str(ROOT/'docs/architecture/validation/schema-sources/sqlserver-functional-fixture.sql'),
            f'{name}:{IN_CONTAINER}-fixture.sql')
        sql('-i', f'{IN_CONTAINER}-fixture.sql')
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
        print(json.dumps({'version': a.version, 'host': platform.machine(), 'nativeX86': platform.machine().lower() in ('x86_64','amd64'),
                          'image': IMAGE, 'adapterCases': 10, 'productionEngineCases': 1,
                          'cases': 11, 'osTrustChanges': False, 'inference': False}))
    finally:
        if started:
            subprocess.run(['docker', 'rm', '--force', name], capture_output=True, check=False)
