-- The one dynamic-SQL primitive of the migration Job (tools/migrate/README.md, "SQL 怎麼組").
--
-- The Job's Rust code sends no SQL built from strings (tools/sql-check), but creating roles and
-- schemas, granting, and applying the outbox policy template need identifiers, which Postgres
-- cannot take as bound parameters. So the Job keeps each such statement as a constant template
-- with `example_<name>` placeholders, the way crates/service-kit/sql/ writes its templates, and
-- passes the template and the names to this function as bound parameters. The function quotes
-- every name with quote_ident (every value with quote_literal), refuses anything that is not a
-- plain lower-case identifier or a safe literal, and runs the result.
--
-- `run_as` (or NULL) is the role the statements run as: a service's owner role, so the objects a
-- service's template creates belong to it, as with its migrations. The switch lasts until the
-- function returns (SET LOCAL semantics, ended explicitly).
--
-- SECURITY INVOKER (the default): it runs with the migration role's own rights. Only the
-- migration role, which owns this schema, can call it.

REVOKE ALL ON SCHEMA trustiot_migrate FROM PUBLIC;

CREATE FUNCTION trustiot_migrate.apply_template(run_as text, template text, replacements jsonb)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    placeholder text;
    replacement jsonb;
    fragment text;
    item text;
    statement text := template;
BEGIN
    IF jsonb_typeof(replacements) <> 'object' THEN
        RAISE EXCEPTION 'replacements must be a JSON object';
    END IF;
    FOR placeholder, replacement IN SELECT key, value FROM jsonb_each(replacements) LOOP
        IF placeholder !~ '^example_[a-z0-9_]{1,55}$' THEN
            RAISE EXCEPTION 'placeholder % is not example_<lower-case name>', placeholder;
        END IF;
        IF jsonb_typeof(replacement) = 'array' THEN
            -- A list of identifiers, joined with ", " (for example the readers of a policy).
            IF jsonb_array_length(replacement) = 0 THEN
                RAISE EXCEPTION 'placeholder % needs at least one name', placeholder;
            END IF;
            fragment := NULL;
            FOR item IN SELECT value FROM jsonb_array_elements_text(replacement) LOOP
                IF item !~ '^[a-z_][a-z0-9_]{0,62}$' OR item ~ '^(pg_|example_)' THEN
                    RAISE EXCEPTION 'name for % is not a plain lower-case identifier', placeholder;
                END IF;
                fragment := concat_ws(', ', fragment, quote_ident(item));
            END LOOP;
        ELSIF jsonb_typeof(replacement) = 'string' THEN
            -- A literal: a password verifier or a tenant ID, never free text.
            item := replacement #>> '{}';
            IF item !~ '^[A-Za-z0-9$:+/=._-]{1,255}$' THEN
                RAISE EXCEPTION 'value for % has characters a literal here never needs', placeholder;
            END IF;
            fragment := quote_literal(item);
        ELSE
            RAISE EXCEPTION 'placeholder % needs a list of names or a string', placeholder;
        END IF;
        -- \m and \M are word boundaries, so example_app never matches inside example_app_x.
        statement := regexp_replace(statement, '\m' || placeholder || '\M', fragment, 'g');
    END LOOP;
    IF statement ~ '\mexample_[a-z0-9_]+' THEN
        RAISE EXCEPTION 'the template has a placeholder without a replacement';
    END IF;
    IF run_as IS NOT NULL THEN
        PERFORM set_config('role', run_as, true);
    END IF;
    -- A template may hold several commands separated by semicolons (CREATE_OWNER_ROLE in
    -- src/postgres.rs, crates/service-kit/sql/outbox-event.sql): an EXECUTE without USING
    -- parameters runs the whole string, command by command, in this transaction. The tests run
    -- both (tests/fresh_database.rs checks the owner role's SET grant that the first SET LOCAL
    -- of CREATE_OWNER_ROLE produces; tests/outbox_and_tenant_roles.rs checks every policy of the
    -- outbox template).
    EXECUTE statement;
    IF run_as IS NOT NULL THEN
        PERFORM set_config('role', 'none', true);
    END IF;
END
$function$;

REVOKE ALL ON FUNCTION trustiot_migrate.apply_template(text, text, jsonb) FROM PUBLIC;
