#!/usr/bin/env python3
"""Architecture-only SQLite mechanics probe; no app adapter, network or inference.

Run with --output-dir pointing to an authoring evidence directory. All database
and file mutations occur in a newly allocated owned temporary fixture directory.
"""
import argparse
import hashlib
import json
import shutil
import sqlite3
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = ROOT / 'docs/architecture/persistence-schema.sql'


def run(output_dir, schema=SCHEMA):
    results = []
    def check(name, condition):
        if not condition:
            raise AssertionError(name)
        results.append(name)
    def rejects(db, name, sql, args=()):
        db.execute('SAVEPOINT expected_constraint')
        rejected = False
        try:
            db.execute(sql, args)
        except sqlite3.IntegrityError:
            rejected = True
        finally:
            db.execute('ROLLBACK TO expected_constraint')
            db.execute('RELEASE expected_constraint')
        check(name, rejected)
    def connect(path):
        db = sqlite3.connect(path, isolation_level=None, timeout=0)
        db.execute('PRAGMA foreign_keys=ON')
        db.execute('PRAGMA journal_mode=WAL')
        db.execute('PRAGMA synchronous=FULL')
        return db
    with tempfile.TemporaryDirectory(prefix='devworks-persistence-') as tmp:
        tmp = Path(tmp)
        db = connect(tmp / 'workspace.sqlite')
        db.executescript(schema.read_text())
        check('foreign_keys_enabled', db.execute('PRAGMA foreign_keys').fetchone()[0] == 1)
        check('wal_and_full', db.execute('PRAGMA journal_mode').fetchone()[0] == 'wal' and db.execute('PRAGMA synchronous').fetchone()[0] == 2)
        db.executescript("""
        INSERT INTO store_meta VALUES(1,'store',1,1,1,0);
        INSERT INTO app_boots VALUES('boot',1,NULL,'Open');
        INSERT INTO actors VALUES('user','user','Fixture user');
        INSERT INTO projects VALUES('p','Before','Active',1,1,1);
        INSERT INTO projects VALUES('q','Other','Active',1,1,1);
        INSERT INTO work_items VALUES('w','p','feature',NULL,NULL,NULL,'Work',10,0,1,1,1,'{}');
        INSERT INTO root_controls VALUES('w','Enabled',1,1,NULL);
        INSERT INTO executions VALUES('x','w','boot',1,'Running','owner',100,1);
        INSERT INTO events VALUES(NULL,'event','user','p','w',NULL,'work','w',1,'Changed',1,'{}');
        INSERT INTO queue_entries VALUES('queue','consumer','event','w',NULL,10,0,'Ready',NULL,NULL,0,0,0,'{}');
        """)
        rejects(db, 'cross_project_parent_refused', "INSERT INTO work_items VALUES('bad','q','task','w',NULL,NULL,'Bad',1,0,1,1,1,'{}')")
        rejects(db, 'second_live_root_activation_refused', "INSERT INTO executions VALUES('x2','w','boot',2,'Pending',NULL,NULL,1)")
        db.execute('BEGIN IMMEDIATE')
        db.execute("UPDATE projects SET title='Uncommitted',revision=2 WHERE id='p'")
        db.execute("INSERT INTO events VALUES(NULL,'rolled-event','user','p','w',NULL,'project','p',2,'Changed',2,'{}')")
        db.execute("INSERT INTO queue_entries VALUES('rolled-queue','consumer','rolled-event','w',NULL,1,0,'Ready',NULL,NULL,0,0,0,'{}')")
        db.execute('ROLLBACK')
        check('domain_history_queue_roll_back_together', db.execute("SELECT title FROM projects WHERE id='p'").fetchone()[0] == 'Before' and db.execute("SELECT count(*) FROM events WHERE id='rolled-event'").fetchone()[0] == 0 and db.execute("SELECT count(*) FROM queue_entries WHERE id='rolled-queue'").fetchone()[0] == 0)
        check('stale_revision_update_changes_nothing', db.execute("UPDATE projects SET title='Wrong' WHERE id='p' AND revision=999").rowcount == 0)
        db.execute("INSERT INTO command_receipts VALUES('user','w','command','Start','digest',NULL,'{}',1)")
        rejects(db, 'receipt_key_cannot_duplicate_with_new_payload', "INSERT INTO command_receipts VALUES('user','w','command','Start','other',NULL,'{}',1)")
        check('original_receipt_retained', db.execute("SELECT payload_digest FROM command_receipts").fetchone()[0] == 'digest')
        other = connect(tmp / 'workspace.sqlite')
        db.execute('BEGIN IMMEDIATE')
        locked = False
        try:
            other.execute('BEGIN IMMEDIATE')
        except sqlite3.OperationalError:
            locked = True
        check('competing_write_transaction_is_serialized', locked)
        db.execute("UPDATE queue_entries SET state='Claimed',owner_id='owner',lease_until=100,generation=generation+1 WHERE id='queue' AND state='Ready' AND generation=0")
        db.execute('COMMIT')
        check('second_conditional_claim_loses', other.execute("UPDATE queue_entries SET state='Claimed',owner_id='other',lease_until=100,generation=generation+1 WHERE id='queue' AND state='Ready' AND generation=0").rowcount == 0)
        check('stale_generation_cannot_complete', other.execute("UPDATE queue_entries SET state='Done' WHERE id='queue' AND owner_id='owner' AND generation=0").rowcount == 0)
        rejects(db, 'consumer_event_deduplicated', "INSERT INTO queue_entries VALUES('duplicate','consumer','event','w',NULL,1,0,'Ready',NULL,NULL,0,0,0,'{}')")
        other.execute('BEGIN')
        check('read_snapshot_established', other.execute("SELECT title FROM projects WHERE id='p'").fetchone()[0] == 'Before')
        db.execute("UPDATE projects SET title='After',revision=2 WHERE id='p'")
        check('reader_keeps_committed_snapshot', other.execute("SELECT title FROM projects WHERE id='p'").fetchone()[0] == 'Before')
        other.execute('COMMIT')
        check('new_read_observes_commit', other.execute("SELECT title FROM projects WHERE id='p'").fetchone()[0] == 'After')
        db.executescript("""
        INSERT INTO index_generations VALUES('base','repo','code','Sealed','base',0,'{}');
        INSERT INTO index_generations VALUES('branch','repo','code','Sealed','branch',0,'{}');
        INSERT INTO index_generations VALUES('dirty','repo','code','Sealed','dirty',0,'{}');
        INSERT INTO source_units VALUES('base','A','1','Replace'),('base','B','2','Replace'),('base','C','3','Replace'),('branch','A','4','Replace'),('branch','C','5','Delete'),('dirty','A','6','Replace');
        INSERT INTO graph_nodes VALUES('base','A','a0','Symbol','Old A','{}'),('base','B','b','Symbol','B','{}'),('base','C','c','Symbol','C','{}'),('branch','A','a1','Symbol','Branch A','{}'),('dirty','A','a2','Symbol','Dirty A','{}');
        INSERT INTO index_views VALUES('view','repo','view-digest','base','branch','dirty','{}');
        INSERT INTO index_views VALUES('newview','repo','new-digest','base','branch','dirty','{}');
        INSERT INTO published_views VALUES('repo','view',1);
        """)
        visible = """WITH layers(id,rank) AS (VALUES('base',0),('branch',1),('dirty',2)),
        choices AS (SELECT u.*,row_number() OVER(PARTITION BY unit_key ORDER BY rank DESC) winner
        FROM source_units u JOIN layers l ON l.id=u.generation_id)
        SELECT n.node_id FROM graph_nodes n JOIN choices c ON c.generation_id=n.generation_id AND c.unit_key=n.unit_key
        WHERE c.winner=1 AND c.action='Replace' ORDER BY n.node_id"""
        check('replacement_and_deletion_mask_before_expansion', [r[0] for r in db.execute(visible)] == ['a2','b'])
        check('pointer_compare_and_swap_wins_once', db.execute("UPDATE published_views SET view_id='newview',revision=2 WHERE source_key='repo' AND revision=1 AND view_id='view'").rowcount == 1)
        check('stale_builder_cannot_replace_new_pointer', db.execute("UPDATE published_views SET view_id='view',revision=2 WHERE source_key='repo' AND revision=1").rowcount == 0)
        rejects(db, 'publication_source_identity_checked', "INSERT INTO published_views VALUES('other-source','view',1)")
        db.executescript("""
        INSERT INTO embedding_spaces VALUES('space','model',2,'L2','float32');
        INSERT INTO search_chunks VALUES('chunk','dirty','A',NULL,'{}','visible passage','digest');
        INSERT INTO search_fts VALUES('chunk','visible passage');
        """)
        rejects(db, 'vector_dimension_identity_checked', "INSERT INTO chunk_vectors VALUES('chunk','space',1,?)", (b'\0'*4,))
        rejects(db, 'vector_byte_length_checked', "INSERT INTO chunk_vectors VALUES('chunk','space',2,?)", (b'\0'*4,))
        db.execute("INSERT INTO chunk_vectors VALUES('chunk','space',2,?)", (b'\0'*8,))
        check('lexical_fixture_searchable', db.execute("SELECT chunk_id FROM search_fts WHERE search_fts MATCH 'visible'").fetchone()[0] == 'chunk')
        payload=b'Original managed document version'
        digest=hashlib.sha256(payload).hexdigest()
        (tmp/'artifacts').mkdir(); (tmp/'artifacts'/digest).write_bytes(payload)
        db.execute("INSERT INTO blobs VALUES(?,?,?,'Published')",(digest,len(payload),'artifacts/'+digest))
        db.execute("INSERT INTO artifacts VALUES('artifact','p',NULL,'Document',1,'p',1)")
        db.execute("INSERT INTO artifact_versions VALUES('v1','artifact',1,?,'text/plain',1,NULL,'{}')",(digest,))
        db.execute("INSERT INTO documents VALUES('artifact','v1',1,1)")
        db.execute("INSERT INTO maintenance_operations VALUES('backup','Backup','Active','{}',1)")
        backup_path=tmp/'backup'; backup_path.mkdir()
        copied=sqlite3.connect(backup_path/'workspace.sqlite',isolation_level=None)
        db.backup(copied,pages=16)
        refs=copied.execute("SELECT DISTINCT b.digest,b.relative_path,b.byte_count FROM blobs b JOIN artifact_versions v ON v.blob_digest=b.digest WHERE v.deleted_at IS NULL").fetchall()
        for expected,relative,size in refs:
            target=backup_path/relative;target.parent.mkdir(parents=True,exist_ok=True)
            shutil.copyfile(tmp/relative,target)
            check('backup_referenced_file_digest',target.stat().st_size==size and hashlib.sha256(target.read_bytes()).hexdigest()==expected)
        db.execute("UPDATE projects SET title='After backup',revision=3 WHERE id='p'")
        check('backup_database_snapshot_is_independent',copied.execute("SELECT title FROM projects WHERE id='p'").fetchone()[0]=='After')
        check('backup_database_integrity',copied.execute('PRAGMA integrity_check').fetchone()[0]=='ok' and copied.execute('PRAGMA foreign_key_check').fetchall()==[])
        (backup_path/refs[0][1]).write_bytes(b'corrupt')
        check('corrupt_backup_file_detected',hashlib.sha256((backup_path/refs[0][1]).read_bytes()).hexdigest()!=refs[0][0])
        db.execute('BEGIN IMMEDIATE')
        db.execute('ALTER TABLE projects ADD COLUMN migration_test TEXT')
        db.execute("INSERT INTO schema_migrations VALUES(2,'checksum',2,'fixture')")
        db.execute('ROLLBACK')
        check('schema_and_ledger_roll_back_together','migration_test' not in [r[1] for r in db.execute('PRAGMA table_info(projects)')] and db.execute('SELECT count(*) FROM schema_migrations').fetchone()[0]==0)
        check('final_integrity_and_foreign_keys',db.execute('PRAGMA integrity_check').fetchone()[0]=='ok' and db.execute('PRAGMA foreign_key_check').fetchall()==[])
        tables=db.execute("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchone()[0]
        copied.close();other.close();db.close()
    report={'status':'Passed architecture mechanics probe; not an implemented application adapter','sqliteVersion':sqlite3.sqlite_version,'schemaSha256':hashlib.sha256(schema.read_bytes()).hexdigest(),'physicalTableCountIncludingFtsInternals':tables,'checks':results,'inference':False,'limitations':['No Rust repository/application policy implementation','No live external effects or resource/process fencing','No sqlite-vec extension loaded by this Python probe','No target packaging, power-loss or representative performance acceptance','Overlay query and backup reference copy are architecture fixtures']}
    output_dir.mkdir(parents=True,exist_ok=True)
    (output_dir/'sqlite-mechanics.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({'checks':len(results),'sqliteVersion':sqlite3.sqlite_version,'status':report['status']}))


if __name__ == '__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--output-dir',type=Path,required=True)
    parser.add_argument('--schema',type=Path,default=SCHEMA)
    args=parser.parse_args()
    run(args.output_dir.resolve(),args.schema.resolve())
