-- Isolated metadata_fixture only. No source rows or inference.
CREATE TABLE meta_pair_parent(z INT,a INT,PRIMARY KEY(z,a),UNIQUE KEY pair_reverse(a,z),CONSTRAINT pair_nonnegative CHECK(z>=0));
CREATE TABLE meta_pair_child(from_z INT,from_a INT,CONSTRAINT pair_reference FOREIGN KEY(from_z,from_a) REFERENCES meta_pair_parent(z,a));
CREATE PROCEDURE meta_parameters(IN third INT,IN first_param VARCHAR(10),OUT last_param INT) SET last_param=third;
SET @ddl=CONCAT('CREATE TABLE meta_large_default(value TEXT DEFAULT (''',REPEAT('x',6000),'''));');
PREPARE metadata_fixture_ddl FROM @ddl;
EXECUTE metadata_fixture_ddl;
DEALLOCATE PREPARE metadata_fixture_ddl;

CREATE TABLE meta_empty_default(value VARCHAR(20) DEFAULT '', absent_value VARCHAR(20) DEFAULT NULL);
