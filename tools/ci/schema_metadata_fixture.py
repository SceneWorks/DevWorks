#!/usr/bin/env python3
"""Isolated PostgreSQL/MySQL/MariaDB metadata and TLS qualification for one server version.

Starts a disposable container, loads the retained fixture DDL, then runs every ignored native
adapter and production-engine case for that engine. Never changes OS trust or user databases.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
SOURCES = ROOT / 'docs/architecture/validation/schema-sources'
OWNER_PASSWORD = 'devworks_metadata_fixture'
# Files copied into the database container live under this prefix inside it.
IN_CONTAINER = '/tmp/devworks-metadata'  # dev-host: allow-path a path inside the database container, not on the host
# Each image is pinned by digest; the tag names the qualified release line.
IMAGES = {
    'postgres': {
        '14': 'postgres:14.25@sha256:9c8a7df7224219e4fa32bb93f4dea665c5b3f322cb443a849bd92eec79af8b81',
        '15': 'postgres:15.20@sha256:ad51f1a9aec85898aff1d1702ac361039286460de92d25610c85d76950f5d212',
        '16': 'postgres:16.16@sha256:30551eca978d79ccf49d037bc297b5670baea61a04ceca42ff7fd3100163d3ce',
        '17': 'postgres:17.12@sha256:dbb9623d5121b5d2c2c9e2fba56e40193cbb3510d4af7f035b1ab1823aff3205',
        '18': 'postgres:18.7@sha256:9f4fd4aa918f52ae3d6f3b687f1de90d8f867cd4d520021545a9a50c120eae3b',
    },
    'mysql': {
        '8.4': 'mysql:8.4.13@sha256:a81d043930e51745522bd5c430961cf8286f0a23f0a9a2a6e0108f7ea71b0902',
        '9.7': 'mysql:9.7.4@sha256:df90258da46f6e7ff8574ff84f17618aba0765cac749928299c69fd4e7cb8ad8',
    },
    'mariadb': {
        '10.11': 'mariadb:10.11.19@sha256:7d829180b5b2a9ba74f6242876ab401a0d7dd338a807e522ef92bc9698760f20',
        '11.4': 'mariadb:11.4.13@sha256:5e73baf8179b29f115101bb102763a47a017361f3e9792daccba6646c3ab1532',
        '11.8': 'mariadb:11.8.9@sha256:d36f18d65a0459510b8b0f643b226a6d299e3206486e2b64ff20754a6d291707',
    },
}
PORTS = {'postgres': 5432, 'mysql': 3306, 'mariadb': 3306}
VARIABLES = {'postgres': 'DEVWORKS_METADATA_POSTGRES', 'mysql': 'DEVWORKS_METADATA_MYSQL',
             'mariadb': 'DEVWORKS_METADATA_MARIADB'}

p = argparse.ArgumentParser()
p.add_argument('engine', choices=sorted(IMAGES))
p.add_argument('version')
a = p.parse_args()
if a.version not in IMAGES[a.engine]:
    raise SystemExit(f'{a.engine} {a.version} is not qualified; choose {", ".join(IMAGES[a.engine])}')
IMAGE = IMAGES[a.engine][a.version]
name = f'devworks-schema-{a.engine}-{os.getpid()}'


def run(*cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw)


def client(database=None):
    """Admin client inside the container over TCP, so the entrypoint's socket-only
    initialisation server never answers readiness."""
    if a.engine == 'postgres':
        return ['docker', 'exec', '-i', '-e', f'PGPASSWORD={OWNER_PASSWORD}', name, 'psql',
                '-v', 'ON_ERROR_STOP=1', '-h', '127.0.0.1', '-U', 'postgres', '-d', database or 'postgres']
    tool = ['mysql'] if a.engine == 'mysql' else ['mariadb', '--skip-ssl']
    return ['docker', 'exec', '-i', name, *tool, '-h127.0.0.1', '-uroot', f'-p{OWNER_PASSWORD}',
            *([database] if database else [])]


def sql(text, database=None):
    try:
        return run(*client(database), input=text)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f'fixture SQL failed: {error.stderr}') from error


def source(file, database=None):
    sql((SOURCES / file).read_text(), database)


def ready():
    end = time.monotonic() + 180
    while time.monotonic() < end:
        try:
            run(*client(), input='SELECT 1;')
            return
        except subprocess.CalledProcessError:
            time.sleep(1)
    raise RuntimeError(f'Isolated {a.engine} {a.version} fixture did not start within 180s')


def certificates(f):
    tls = f / 'tls'
    tls.mkdir()
    for ca in ('ca', 'unused-ca'):
        run('openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(f/f'{ca}.key'),
            '-out', str(tls/f'{ca}.pem'), '-days', '2', '-subj', f'/CN=DevWorks-Isolated-Metadata-{ca}')
    run('openssl', 'req', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(tls/'server.key'),
        '-out', str(f/'server.csr'), '-subj', '/CN=localhost')
    (f/'server.ext').write_text('subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n')
    run('openssl', 'x509', '-req', '-in', str(f/'server.csr'), '-CA', str(tls/'ca.pem'),
        '-CAkey', str(f/'ca.key'), '-CAcreateserial', '-out', str(tls/'server.pem'),
        '-days', '2', '-extfile', str(f/'server.ext'))
    # The MySQL/MariaDB server user reads the mounted files; they are disposable fixture keys.
    tls.chmod(0o755)
    for file in tls.iterdir():
        file.chmod(0o644)
    return tls


def start(tls):
    common = ['docker', 'run', '--detach', '--rm', '--name', name, '--platform', 'linux/amd64',
              '--publish', f'127.0.0.1::{PORTS[a.engine]}', '--memory', '2g', '--cpus', '2']
    if a.engine == 'postgres':
        run(*common, '--env', f'POSTGRES_PASSWORD={OWNER_PASSWORD}', IMAGE)
        return
    password = 'MYSQL_ROOT_PASSWORD' if a.engine == 'mysql' else 'MARIADB_ROOT_PASSWORD'
    run(*common, '--volume', f'{tls}:/devworks-tls:ro', '--env', f'{password}={OWNER_PASSWORD}', IMAGE,
        '--ssl-ca=/devworks-tls/ca.pem', '--ssl-cert=/devworks-tls/server.pem',
        '--ssl-key=/devworks-tls/server.key')


def postgres_tls(tls):
    # PostgreSQL refuses a group/world-readable key, so copy and own it inside the container.
    for file in ('server.pem', 'server.key'):
        run('docker', 'cp', str(tls/file), f'{name}:{IN_CONTAINER}-{file}')
    run('docker', 'exec', '-u', 'root', name, 'chown', 'postgres:postgres',
        f'{IN_CONTAINER}-server.pem', f'{IN_CONTAINER}-server.key')
    run('docker', 'exec', '-u', 'root', name, 'chmod', '600', f'{IN_CONTAINER}-server.key')
    # ALTER SYSTEM cannot run inside a transaction block, so each statement is its own -c.
    run(*client(),
        '-c', f"ALTER SYSTEM SET ssl_cert_file='{IN_CONTAINER}-server.pem'",
        '-c', f"ALTER SYSTEM SET ssl_key_file='{IN_CONTAINER}-server.key'",
        '-c', 'ALTER SYSTEM SET ssl=on', '-c', 'SELECT pg_reload_conf()')
    end = time.monotonic() + 30
    while True:
        try:
            run('docker', 'exec', '-e', f'PGPASSWORD={OWNER_PASSWORD}', name, 'psql',
                'host=127.0.0.1 user=postgres dbname=postgres sslmode=require', '-c', 'SELECT 1')
            return
        except subprocess.CalledProcessError:
            if time.monotonic() > end:
                raise
            time.sleep(1)


def fixtures():
    if a.engine == 'postgres':
        source('postgres-base-fixture.sql')
        source('postgres-edge-fixtures.sql', 'metadata_fixture')
        source('postgres-catalog-fixture.sql')
        source('postgres-selected-bootstrap-fixture.sql', 'metadata_fixture')
        return
    source('mysql-base-fixture.sql')
    source('mysql-catalog-fixture.sql', 'metadata_fixture')
    source('mysql-edge-fixtures.sql', 'metadata_fixture')
    if a.engine == 'mysql':
        source('mysql-functional-index-fixture.sql', 'metadata_fixture')
        # MySQL captures are Complete: the reader may read routine definitions.
        sql("GRANT SHOW_ROUTINE ON *.* TO 'metadata_reader'@'%';")
    else:
        source('mariadb-check-name-fixture.sql', 'metadata_fixture')


with tempfile.TemporaryDirectory(prefix=f'devworks-schema-{a.engine}-') as folder:
    f = Path(folder)
    f.chmod(0o755)
    tls = certificates(f)
    started = False
    try:
        start(tls)
        started = True
        ready()
        fixtures()
        if a.engine == 'postgres':
            postgres_tls(tls)
        endpoint = run('docker', 'port', name, f'{PORTS[a.engine]}/tcp').stdout.split()[0]
        env = os.environ.copy()
        env[VARIABLES[a.engine]] = f'{a.engine}://{endpoint}/metadata_fixture'
        env['DEVWORKS_METADATA_CA'] = str(tls/'ca.pem')
        env['DEVWORKS_METADATA_UNUSED_CA'] = str(tls/'unused-ca.pem')
        if a.engine == 'postgres':
            env['DEVWORKS_METADATA_POSTGRES_LIMITS'] = f'postgres://{endpoint}/metadata_limits'
        # Ordinary tests ignore network fixture cases; this explicit job runs every case for
        # this engine (extraction, bounds, TLS/CA/hostname refusal) without inference.
        subprocess.run(['cargo', 'test', '--locked', '-p', 'devworks-adapters', '--test',
                        'database_metadata', a.engine, '--', '--ignored', '--nocapture'],
                       cwd=ROOT, env=env, check=True)
        subprocess.run(['cargo', 'test', '--locked', '-p', 'devworks-desktop', '--test',
                        'database_network', a.engine, '--', '--ignored', '--nocapture'],
                       cwd=ROOT, env=env, check=True)
        print(json.dumps({'engine': a.engine, 'version': a.version, 'image': IMAGE,
                          'osTrustChanges': False, 'inference': False}))
    finally:
        if started:
            subprocess.run(['docker', 'rm', '--force', name], capture_output=True, check=False)
