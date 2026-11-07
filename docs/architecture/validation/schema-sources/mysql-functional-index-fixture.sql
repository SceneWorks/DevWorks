-- MySQL8.4 only; fixed catalog reads never execute this source routine or expression.
USE metadata_fixture;
CREATE TABLE meta_expression_parent(name VARCHAR(40),suffix INT);
CREATE INDEX meta_functional_index ON meta_expression_parent((LOWER(name)),suffix);
CREATE INDEX meta_only_functional_index ON metadata_fixture.meta_expression_parent((LOWER(name)));

CREATE TABLE metadata_fixture.meta_constraint_namespaces(id INT, CONSTRAINT same_name UNIQUE(id), CONSTRAINT same_name CHECK(id>0));
