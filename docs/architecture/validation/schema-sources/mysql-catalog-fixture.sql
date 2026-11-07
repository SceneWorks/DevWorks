-- Admin-only setup for isolated MySQL8.4/MariaDB11.4 qualification containers.
-- More than 129 visible names precede the explicitly requested fixture catalog.
DELIMITER //
CREATE PROCEDURE metadata_fixture.setup_catalog_discovery_fixture()
BEGIN
  DECLARE n INT DEFAULT 0;
  WHILE n < 130 DO
    SET @catalog_name = CONCAT('aaa_metadata_discovery_', LPAD(n,3,'0'));
    SET @ddl = CONCAT('CREATE DATABASE IF NOT EXISTS `', @catalog_name, '`');
    PREPARE fixture_stmt FROM @ddl; EXECUTE fixture_stmt; DEALLOCATE PREPARE fixture_stmt;
    SET @ddl = CONCAT('GRANT SELECT ON `', @catalog_name, '`.* TO ''metadata_reader''@''%''');
    PREPARE fixture_stmt FROM @ddl; EXECUTE fixture_stmt; DEALLOCATE PREPARE fixture_stmt;
    SET n = n + 1;
  END WHILE;
END//
DELIMITER ;
CALL metadata_fixture.setup_catalog_discovery_fixture();
DROP PROCEDURE metadata_fixture.setup_catalog_discovery_fixture;
CREATE DATABASE zzz_metadata_child;
CREATE DATABASE zzz_metadata_parent;
CREATE TABLE zzz_metadata_parent.parent(id INT PRIMARY KEY);
CREATE TABLE zzz_metadata_child.child(id INT PRIMARY KEY, parent_id INT,
 CONSTRAINT cross_catalog_parent FOREIGN KEY(parent_id) REFERENCES zzz_metadata_parent.parent(id));
GRANT SELECT ON zzz_metadata_child.* TO 'metadata_reader'@'%';
GRANT SELECT ON zzz_metadata_parent.* TO 'metadata_reader'@'%';
