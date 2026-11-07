-- Isolated metadata_fixture only. No source rows or inference.
CREATE TABLE meta_pair_parent(z INT,a INT,PRIMARY KEY(z,a),UNIQUE KEY pair_reverse(a,z),CONSTRAINT pair_nonnegative CHECK(z>=0));
CREATE TABLE meta_pair_child(from_z INT,from_a INT,CONSTRAINT pair_reference FOREIGN KEY(from_z,from_a) REFERENCES meta_pair_parent(z,a));
CREATE PROCEDURE meta_parameters(IN third INT,IN first_param VARCHAR(10),OUT last_param INT) SET last_param=third;
SET @ddl=CONCAT('CREATE TABLE meta_large_default(value TEXT DEFAULT (''',REPEAT('x',6000),'''));');
PREPARE metadata_fixture_ddl FROM @ddl;
EXECUTE metadata_fixture_ddl;
DEALLOCATE PREPARE metadata_fixture_ddl;

CREATE TABLE meta_empty_default(value VARCHAR(20) DEFAULT '', absent_value VARCHAR(20) DEFAULT NULL);

SET NAMES utf8mb4;
SET @ddl=CONCAT('CREATE OR REPLACE VIEW meta_oversized_definition AS SELECT ''',REPEAT('界',50000),''' AS metadata_only');
PREPARE metadata_fixture_ddl FROM @ddl;
EXECUTE metadata_fixture_ddl;
DEALLOCATE PREPARE metadata_fixture_ddl;
SET SESSION group_concat_max_len=131072;
WITH RECURSIVE fixture_labels(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM fixture_labels WHERE n<200)
 SELECT GROUP_CONCAT(QUOTE(CONCAT('label_',LPAD(n,4,'0'),'_',REPEAT('x',30))) ORDER BY n SEPARATOR ',') INTO @metadata_enum_values FROM fixture_labels;
SET @ddl=CONCAT('CREATE TABLE meta_large_enum(value ENUM(',@metadata_enum_values,'))');
PREPARE metadata_fixture_ddl FROM @ddl;
EXECUTE metadata_fixture_ddl;
DEALLOCATE PREPARE metadata_fixture_ddl;
CREATE PROCEDURE meta_many_parameters(IN p001 INT,IN p002 INT,IN p003 INT,IN p004 INT,IN p005 INT,IN p006 INT,IN p007 INT,IN p008 INT,IN p009 INT,IN p010 INT,IN p011 INT,IN p012 INT,IN p013 INT,IN p014 INT,IN p015 INT,IN p016 INT,IN p017 INT,IN p018 INT,IN p019 INT,IN p020 INT,IN p021 INT,IN p022 INT,IN p023 INT,IN p024 INT,IN p025 INT,IN p026 INT,IN p027 INT,IN p028 INT,IN p029 INT,IN p030 INT,IN p031 INT,IN p032 INT,IN p033 INT,IN p034 INT,IN p035 INT,IN p036 INT,IN p037 INT,IN p038 INT,IN p039 INT,IN p040 INT,IN p041 INT,IN p042 INT,IN p043 INT,IN p044 INT,IN p045 INT,IN p046 INT,IN p047 INT,IN p048 INT,IN p049 INT,IN p050 INT,IN p051 INT,IN p052 INT,IN p053 INT,IN p054 INT,IN p055 INT,IN p056 INT,IN p057 INT,IN p058 INT,IN p059 INT,IN p060 INT,IN p061 INT,IN p062 INT,IN p063 INT,IN p064 INT,IN p065 INT,IN p066 INT,IN p067 INT,IN p068 INT,IN p069 INT,IN p070 INT,IN p071 INT,IN p072 INT,IN p073 INT,IN p074 INT,IN p075 INT,IN p076 INT,IN p077 INT,IN p078 INT,IN p079 INT,IN p080 INT,IN p081 INT,IN p082 INT,IN p083 INT,IN p084 INT,IN p085 INT,IN p086 INT,IN p087 INT,IN p088 INT,IN p089 INT,IN p090 INT,IN p091 INT,IN p092 INT,IN p093 INT,IN p094 INT,IN p095 INT,IN p096 INT,IN p097 INT,IN p098 INT,IN p099 INT,IN p100 INT,IN p101 INT,IN p102 INT,IN p103 INT,IN p104 INT,IN p105 INT,IN p106 INT,IN p107 INT,IN p108 INT,IN p109 INT,IN p110 INT,IN p111 INT,IN p112 INT,IN p113 INT,IN p114 INT,IN p115 INT,IN p116 INT,IN p117 INT,IN p118 INT,IN p119 INT,IN p120 INT,IN p121 INT,IN p122 INT,IN p123 INT,IN p124 INT,IN p125 INT,IN p126 INT,IN p127 INT,IN p128 INT,IN p129 INT) SET @unused=0;

-- Routine type participates in identity; both dialects permit these same-name objects.
USE metadata_fixture;
CREATE FUNCTION meta_same_routine(function_argument INT) RETURNS INT DETERMINISTIC NO SQL RETURN function_argument;
CREATE PROCEDURE meta_same_routine(IN procedure_argument INT,OUT procedure_result INT) SET procedure_result=procedure_argument;
