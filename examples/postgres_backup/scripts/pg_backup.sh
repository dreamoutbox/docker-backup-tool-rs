#!/bin/sh
#
# PostgreSQL pre-backup hook script for dvb (Docker Volume Backup)
#
# Runs pg_dump inside the running PostgreSQL container (via docker exec) and writes
# a plain-SQL dump to a staging directory that dvb then archives and uploads.
#
# Usage with dvb (dvb.toml):
#   [job]
#   pre_backup_script = "/scripts/pg_backup.sh"
#   source            = ["/backup/pgdump"]   # must match OUTPUT_DIR below
#
# Environment variables:
#   PGUSER              Postgres user (default: "postgres")
#   PGDATABASE          Target database name (default: "app")
#   PGPASSWORD          Postgres password (optional; passed through env)
#   POSTGRES_CONTAINER  Container name to exec into (default: "db")
#   OUTPUT_DIR          Directory to write dump.sql into (default: "/backup/pgdump")
#
# The script exits non-zero on any failure so dvb aborts the backup rather than
# uploading an empty or partial dump.

set -eu

PGUSER="${PGUSER:-postgres}"
PGDATABASE="${PGDATABASE:-app}"
POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-db}"
OUTPUT_DIR="${OUTPUT_DIR:-/backup/pgdump}"

mkdir -p "$OUTPUT_DIR"
DUMP_FILE="$OUTPUT_DIR/dump.sql"

echo "pg_backup: dumping database '$PGDATABASE' from container '$POSTGRES_CONTAINER'..."

if command -v pg_dump >/dev/null 2>&1; then
    # Native pg_dump available inside the dvb container
    if [ -n "${PGPASSWORD:-}" ]; then
        export PGPASSWORD
    fi
    pg_dump -h "${PGHOST:-db}" -p "${PGPORT:-5432}" \
            -U "$PGUSER" "$PGDATABASE" \
            --no-password \
            -f "$DUMP_FILE"
elif command -v docker >/dev/null 2>&1 && [ -S "/var/run/docker.sock" ]; then
    # Pipe pg_dump output from the database container via docker exec
    env_args=""
    if [ -n "${PGPASSWORD:-}" ]; then
        env_args="-e PGPASSWORD=$PGPASSWORD"
    fi
    # shellcheck disable=SC2086
    docker exec $env_args "$POSTGRES_CONTAINER" \
        pg_dump -U "$PGUSER" "$PGDATABASE" > "$DUMP_FILE"
else
    echo "ERROR: Neither pg_dump nor docker CLI found. Cannot produce SQL dump." >&2
    exit 1
fi

echo "pg_backup: dump written to $DUMP_FILE ($(wc -c < "$DUMP_FILE") bytes)"
