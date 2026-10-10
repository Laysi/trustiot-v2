-- Development smoke data for Postgres: a relational table and an Apache AGE graph with the same
-- two machines. It is not the product schema (each service owns its schema and migrations);
-- it only proves that the database and the AGE extension work. Safe to run repeatedly.
\set ON_ERROR_STOP on

CREATE SCHEMA IF NOT EXISTS dev_seed;

CREATE TABLE IF NOT EXISTS dev_seed.machine (
    id   text PRIMARY KEY,
    name text NOT NULL,
    kind text NOT NULL
);

INSERT INTO dev_seed.machine (id, name, kind)
VALUES ('m-001', 'Press 1', 'press'),
       ('m-002', 'Conveyor 1', 'conveyor')
ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, kind = EXCLUDED.kind;

CREATE EXTENSION IF NOT EXISTS age;
LOAD 'age';
SET search_path = ag_catalog, "$user", public;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM ag_catalog.ag_graph WHERE name = 'dev_seed_graph') THEN
        PERFORM ag_catalog.create_graph('dev_seed_graph');
    END IF;
END
$$;

SELECT * FROM cypher('dev_seed_graph', $$
    MERGE (m:Machine {id: 'm-001'})
    SET m.name = 'Press 1'
    RETURN m.id
$$) AS (id agtype);

SELECT * FROM cypher('dev_seed_graph', $$
    MERGE (m:Machine {id: 'm-002'})
    SET m.name = 'Conveyor 1'
    RETURN m.id
$$) AS (id agtype);

SELECT * FROM cypher('dev_seed_graph', $$
    MATCH (a:Machine {id: 'm-001'}), (b:Machine {id: 'm-002'})
    MERGE (a)-[r:FEEDS]->(b)
    RETURN type(r)
$$) AS (relation agtype);
