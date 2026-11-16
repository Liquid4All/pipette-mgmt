-- Collect each matcher value as a token array and join it once.
-- Appending to a text value copies the whole value on each append.
CREATE OR REPLACE FUNCTION queue_match_clauses(body text) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE
    token text;
    depth integer := 0;
    field text;
    expecting_field boolean := false;
    capture boolean := false;
    raw_parts text[] := '{}';
    clauses jsonb := '{}'::jsonb;
    fields constant text[] := ARRAY['requires', 'any_of', 'excludes', 'exclude_clients', 'not_all_of', 'clients'];
    raw_fields text[] := array_fill(NULL::text, ARRAY[6]);
    i integer;
BEGIN
    -- Validate with json, which accepts strings that jsonb cannot represent.
    -- Native field extraction also decodes ignored strings, so walk the tokens
    -- without decoding them. Only top-level matcher values become jsonb.
    IF json_typeof(body::json) <> 'object' THEN
        RETURN clauses;
    END IF;
    FOR token IN
        SELECT part[1] FROM regexp_matches(
            body, $tokens$"(?:[^"\\]|\\.)*"|[{}\[\],:]|[^[:space:]{}\[\],:"]+$tokens$, 'g'
        ) AS part
    LOOP
        IF depth = 1 AND token IN (',', '}') THEN
            IF capture THEN
                raw_fields[array_position(fields, field)] := array_to_string(raw_parts, '');
            END IF;
            capture := false;
            raw_parts := '{}';
            expecting_field := token = ',';
        ELSIF depth = 1 AND expecting_field THEN
            BEGIN
                field := token::jsonb #>> '{}';
            EXCEPTION WHEN untranslatable_character THEN
                -- Such a key cannot name one of the six matcher fields.
                field := NULL;
            END;
            expecting_field := false;
        ELSIF depth = 1 AND token = ':' THEN
            capture := field = ANY(fields);
        ELSIF capture THEN
            raw_parts := array_append(raw_parts, token);
        END IF;

        IF token IN ('{', '[') THEN
            depth := depth + 1;
            IF depth = 1 THEN
                expecting_field := true;
            END IF;
        ELSIF token IN ('}', ']') THEN
            depth := depth - 1;
        END IF;
    END LOOP;
    -- Decode only the final value of each field, like serde_json::Value does
    -- for duplicate keys. A discarded earlier value may not fit jsonb.
    FOR i IN 1..6 LOOP
        IF raw_fields[i] IS NOT NULL THEN
            clauses := jsonb_set(clauses, ARRAY[fields[i]], raw_fields[i]::jsonb);
        END IF;
    END LOOP;
    RETURN clauses;
EXCEPTION WHEN invalid_text_representation OR untranslatable_character OR numeric_value_out_of_range THEN
    -- A corrupt or unrepresentable matcher value gives NULL. The text store
    -- reports the value per item, and migration and promotion continue.
    RETURN NULL;
END;
$$;
