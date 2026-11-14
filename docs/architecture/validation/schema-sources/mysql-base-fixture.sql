-- Admin-only setup for isolated MySQL/MariaDB qualification containers.
-- The fixture driver adds the dialect-specific routine-definition grant: MySQL's reader
-- receives SHOW_ROUTINE (Complete captures); MariaDB's reader does not (Partial captures).
CREATE DATABASE metadata_fixture;
CREATE USER 'metadata_reader'@'%' IDENTIFIED BY 'devworks_metadata_reader';
GRANT SELECT, SHOW VIEW, EXECUTE ON metadata_fixture.* TO 'metadata_reader'@'%';
USE metadata_fixture;
CREATE TABLE parent(id INT PRIMARY KEY, note VARCHAR(255) NOT NULL DEFAULT 'metadata default');
CREATE TABLE child(id INT PRIMARY KEY, parent_id INT NOT NULL,
 CONSTRAINT child_parent_fk FOREIGN KEY(parent_id) REFERENCES parent(id));
CREATE INDEX child_pair ON child(parent_id, id);
INSERT INTO parent(id, note) VALUES (1, 'SOURCE_ROW_SENTINEL_NEVER_INDEX');
CREATE VIEW read_parent AS SELECT id, note FROM parent;
CREATE PROCEDURE describe_parent(IN input_id INT) SELECT id FROM parent WHERE id = input_id;
