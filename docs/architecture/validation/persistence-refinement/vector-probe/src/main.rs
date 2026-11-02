use rusqlite::{Connection, ffi::sqlite3_auto_extension};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())));
    }
    let db = Connection::open_in_memory()?;
    let schema = std::fs::read_to_string(std::env::args().nth(1).ok_or("schema path required")?)?;
    db.execute_batch(&schema)?;
    db.execute_batch("INSERT INTO index_generations VALUES('g','source','document','Sealed','digest',0,'{}');
        INSERT INTO source_units VALUES('g','doc','digest','Replace');
        INSERT INTO embedding_spaces VALUES('space','model',2,'L2','float32');
        INSERT INTO search_chunks VALUES('denied','g','doc',NULL,'{}','Private','a'),('allowed','g','doc',NULL,'{}','Allowed','b');
        INSERT INTO chunk_vectors VALUES('denied','space',2,vec_f32('[1,0]')),('allowed','space',2,vec_f32('[0.6,0.8]'));
        CREATE TEMP TABLE eligible_chunks(id TEXT PRIMARY KEY);
        INSERT INTO eligible_chunks VALUES('allowed');")?;
    let id:String = db.query_row("WITH eligible AS MATERIALIZED (
        SELECT v.chunk_id,v.vector FROM chunk_vectors v JOIN eligible_chunks e ON e.id=v.chunk_id
        WHERE v.space_id='space')
        SELECT chunk_id FROM eligible ORDER BY vec_distance_L2(vector,vec_f32('[1,0]')),chunk_id LIMIT 1",[],|r|r.get(0))?;
    assert_eq!(id,"allowed");
    let global:String=db.query_row("SELECT chunk_id FROM chunk_vectors ORDER BY vec_distance_L2(vector,vec_f32('[1,0]')) LIMIT 1",[],|r|r.get(0))?;
    assert_eq!(global,"denied");
    let distance:f64=db.query_row("SELECT vec_distance_L2(vec_f32('[0.6,0.8]'),vec_f32('[1,0]'))",[],|r|r.get(0))?;
    assert!((distance-(0.8_f64).sqrt()).abs()<1e-6);
    let mismatch=db.query_row::<f64,_,_>("SELECT vec_distance_L2('[1]','[1,2]')",[],|r|r.get(0));
    assert!(mismatch.is_err());
    let version:String=db.query_row("SELECT sqlite_version()",[],|r|r.get(0))?;
    let extension:String=db.query_row("SELECT vec_version()",[],|r|r.get(0))?;
    println!("{}",serde_json::json!({"status":"Passed narrow native scalar-distance fixture","sqliteVersion":version,"sqliteVecVersion":extension,"checks":{"schemaLoads":true,"scopeBeforeRanking":true,"globalNearestIsDenied":true,"distanceCorrect":true,"dimensionMismatchRejected":true},"inference":false,"limitations":"In-memory architecture fixture, not production adapter, full scope/overlay suite or performance/platform acceptance"}));
    Ok(())
}
