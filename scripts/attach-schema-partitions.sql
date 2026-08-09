-- Attach partition child tables after pgschema apply.
--
-- pgschema currently emits existing partition children as standalone CREATE TABLE
-- statements when applying schema/schema.sql in CI. The tables exist, but they
-- are not attached to their partitioned parents, so inserts into events or
-- delivery_log fail with "no partition of relation ... found for row". Keep this
-- idempotent: raw psql/schema.sql already attaches these partitions, while
-- pgschema-created schemas need this repair step.

DO $$
DECLARE
    part   RECORD;
    trig   text;
BEGIN
    -- pgschema copies the parent's triggers onto each standalone child. Drop
    -- those copies before ATTACH: PostgreSQL recreates the inherited parent
    -- triggers while attaching and rejects same-named child triggers.
    --
    -- Every non-internal trigger on the child is dropped rather than a
    -- hardcoded list of names. schema/schema.sql declares no partition-local
    -- triggers — every trigger a child carries here is a copy of one the
    -- parent owns — so this is equivalent to naming them, and it does not go
    -- stale the next time a trigger is added to `events`. The previous list
    -- named three and silently became wrong when the NIP-RS guards landed.
    FOR part IN
        SELECT *
        FROM (VALUES
            ('events_p_past',    'FROM (MINVALUE) TO (''2026-01-01'')'),
            ('events_p2026_01',  'FROM (''2026-01-01'') TO (''2026-02-01'')'),
            ('events_p2026_02',  'FROM (''2026-02-01'') TO (''2026-03-01'')'),
            ('events_p2026_03',  'FROM (''2026-03-01'') TO (''2026-04-01'')'),
            ('events_p2026_04',  'FROM (''2026-04-01'') TO (''2026-05-01'')'),
            ('events_p2026_05',  'FROM (''2026-05-01'') TO (''2026-06-01'')'),
            ('events_p2026_06',  'FROM (''2026-06-01'') TO (''2026-07-01'')'),
            ('events_p_future',  'FROM (''2026-07-01'') TO (MAXVALUE)')
        ) AS t(child, bounds)
    LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_inherits
            WHERE inhparent = 'events'::regclass
              AND inhrelid = part.child::regclass
        ) THEN
            FOR trig IN
                SELECT tgname FROM pg_trigger
                WHERE tgrelid = part.child::regclass AND NOT tgisinternal
            LOOP
                EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I', trig, part.child);
            END LOOP;

            EXECUTE format(
                'ALTER TABLE events ATTACH PARTITION %I FOR VALUES %s',
                part.child, part.bounds);
        END IF;
    END LOOP;

    -- When pgschema creates partition children as standalone tables, it also
    -- preserves the parent's identity column on delivery_log children. PostgreSQL
    -- rejects attaching a child table that has its own identity column, so each
    -- delivery_log attach path drops that standalone identity first. Raw
    -- schema-created partitions are already attached, so these branches do not
    -- run against inherited partition columns.

    IF NOT EXISTS (
        SELECT 1 FROM pg_inherits
        WHERE inhparent = 'delivery_log'::regclass
          AND inhrelid = 'delivery_log_p_past'::regclass
    ) THEN
        ALTER TABLE delivery_log_p_past ALTER COLUMN id DROP IDENTITY IF EXISTS;
        ALTER TABLE delivery_log ATTACH PARTITION delivery_log_p_past
            FOR VALUES FROM (MINVALUE) TO ('2026-03-01');
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_inherits
        WHERE inhparent = 'delivery_log'::regclass
          AND inhrelid = 'delivery_log_p2026_03'::regclass
    ) THEN
        ALTER TABLE delivery_log_p2026_03 ALTER COLUMN id DROP IDENTITY IF EXISTS;
        ALTER TABLE delivery_log ATTACH PARTITION delivery_log_p2026_03
            FOR VALUES FROM ('2026-03-01') TO ('2026-04-01');
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_inherits
        WHERE inhparent = 'delivery_log'::regclass
          AND inhrelid = 'delivery_log_p2026_04'::regclass
    ) THEN
        ALTER TABLE delivery_log_p2026_04 ALTER COLUMN id DROP IDENTITY IF EXISTS;
        ALTER TABLE delivery_log ATTACH PARTITION delivery_log_p2026_04
            FOR VALUES FROM ('2026-04-01') TO ('2026-05-01');
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_inherits
        WHERE inhparent = 'delivery_log'::regclass
          AND inhrelid = 'delivery_log_p2026_05'::regclass
    ) THEN
        ALTER TABLE delivery_log_p2026_05 ALTER COLUMN id DROP IDENTITY IF EXISTS;
        ALTER TABLE delivery_log ATTACH PARTITION delivery_log_p2026_05
            FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_inherits
        WHERE inhparent = 'delivery_log'::regclass
          AND inhrelid = 'delivery_log_p2026_06'::regclass
    ) THEN
        ALTER TABLE delivery_log_p2026_06 ALTER COLUMN id DROP IDENTITY IF EXISTS;
        ALTER TABLE delivery_log ATTACH PARTITION delivery_log_p2026_06
            FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_inherits
        WHERE inhparent = 'delivery_log'::regclass
          AND inhrelid = 'delivery_log_p_future'::regclass
    ) THEN
        ALTER TABLE delivery_log_p_future ALTER COLUMN id DROP IDENTITY IF EXISTS;
        ALTER TABLE delivery_log ATTACH PARTITION delivery_log_p_future
            FOR VALUES FROM ('2026-07-01') TO (MAXVALUE);
    END IF;
END $$;
