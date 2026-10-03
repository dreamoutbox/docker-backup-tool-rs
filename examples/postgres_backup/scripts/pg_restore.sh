#!/bin/sh
#
# PostgreSQL restore hook script for dvb (Docker Volume Backup)
#
# Usage:
#   pg_restore.sh <extracted-dir> [extra-psql-or-pg_restore-args...]
#
# Usage with dvb CLI:
#   # Run restore with this script hook:
#   dvb restore db --script /scripts/pg_restore.sh
#
#   # Pass extra arguments to psql/pg_restore:
#   dvb restore db --script /scripts/pg_restore.sh -- --clean --if-exists
#
# Usage in docker-compose:
#   # 1. Mount this script into the dvb container:
#   #      volumes:
#   #        - ./examples/pg_restore.sh:/scripts/pg_restore.sh:ro
#   #
#   # 2. Run the restore command:
#   #      docker exec backup dvb restore db --script /scripts/pg_restore.sh
#
# Environment variables:
#   DATABASE_URL        Full PostgreSQL connection URI (e.g. postgres://user:pass@host:5432/dbname)
#   PGHOST              Postgres host (default: "db", fallback: "localhost")
#   PGPORT              Postgres port (default: 5432)
#   PGUSER              Postgres user (default: "postgres")
#   PGDATABASE          Target database (default: "app")
#   PGPASSWORD          Postgres password (optional)
#   POSTGRES_CONTAINER  Target container name when restoring via docker exec (default: "db")
#   DVB_RESTORE_DIR     Extraction directory (set automatically by dvb restore)
#   DVB_JOB             Job name (set automatically by dvb restore)
#   DVB_ARCHIVE         Backup archive name (set automatically by dvb restore)
#   DVB_ARCHIVE_TIME    Backup timestamp RFC3339 (set automatically by dvb restore)
#

set -eu

show_usage() {
    cat << 'EOF'
Usage: pg_restore.sh <extracted-dir> [extra-args...]

Arguments:
  <extracted-dir>    Path to the directory containing the extracted backup files.
  [extra-args...]    Optional flags passed directly to psql or pg_restore.

Environment Variables:
  DATABASE_URL        Postgres connection URI (e.g. postgres://user:pass@host:5432/dbname)
  PGHOST              Postgres host (default: "db", fallback: "localhost")
  PGPORT              Postgres port (default: 5432)
  PGUSER              Postgres user (default: "postgres")
  PGDATABASE          Target database (default: "app")
  PGPASSWORD          Postgres password
  POSTGRES_CONTAINER  Target container name for docker exec (default: "db")
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    show_usage
    exit 0
fi

# 1. Determine extracted directory
EXTRACT_DIR="${1:-${DVB_RESTORE_DIR:-}}"
if [ -z "$EXTRACT_DIR" ]; then
    echo "ERROR: Missing extracted directory argument or DVB_RESTORE_DIR environment variable." >&2
    show_usage >&2
    exit 1
fi

if [ $# -ge 1 ]; then
    shift
fi

if [ ! -d "$EXTRACT_DIR" ]; then
    echo "ERROR: Directory does not exist: $EXTRACT_DIR" >&2
    exit 1
fi

# 2. Connection defaults
PGHOST="${PGHOST:-db}"
PGPORT="${PGPORT:-5432}"
PGUSER="${PGUSER:-postgres}"
PGDATABASE="${PGDATABASE:-app}"
POSTGRES_CONTAINER="${POSTGRES_CONTAINER:-db}"

# Fall back to localhost if host "db" cannot be resolved and no DATABASE_URL is set
if [ -z "${DATABASE_URL:-}" ] && [ "$PGHOST" = "db" ]; then
    if ! getent hosts db >/dev/null 2>&1 && ! ping -c 1 -W 1 db >/dev/null 2>&1; then
        PGHOST="localhost"
    fi
fi

# 3. Locate dump file in the extracted directory
DUMP_FILE=""
DUMP_TYPE="sql" # "sql" or "custom"

# Check standard file names first
if [ -f "$EXTRACT_DIR/dump.sql" ]; then
    DUMP_FILE="$EXTRACT_DIR/dump.sql"
    DUMP_TYPE="sql"
elif [ -f "$EXTRACT_DIR/backup.sql" ]; then
    DUMP_FILE="$EXTRACT_DIR/backup.sql"
    DUMP_TYPE="sql"
elif [ -f "$EXTRACT_DIR/dump.dump" ]; then
    DUMP_FILE="$EXTRACT_DIR/dump.dump"
    DUMP_TYPE="custom"
elif [ -f "$EXTRACT_DIR/dump.tar" ]; then
    DUMP_FILE="$EXTRACT_DIR/dump.tar"
    DUMP_TYPE="custom"
else
    # Find any SQL or custom dump file
    SQL_MATCH=$(find "$EXTRACT_DIR" -maxdepth 2 -type f -name "*.sql" | head -n 1)
    if [ -n "$SQL_MATCH" ]; then
        DUMP_FILE="$SQL_MATCH"
        DUMP_TYPE="sql"
    else
        CUSTOM_MATCH=$(find "$EXTRACT_DIR" -maxdepth 2 -type f \( -name "*.dump" -o -name "*.custom" \) | head -n 1)
        if [ -n "$CUSTOM_MATCH" ]; then
            DUMP_FILE="$CUSTOM_MATCH"
            DUMP_TYPE="custom"
        fi
    fi
fi

# Check for raw PostgreSQL data directory (base/ and PG_VERSION)
if [ -z "$DUMP_FILE" ] && [ -f "$EXTRACT_DIR/PG_VERSION" ]; then
    echo "ERROR: $EXTRACT_DIR appears to be a raw PGDATA directory, not a SQL/pg_dump archive." >&2
    echo "Raw database directories cannot be restored via psql/pg_restore." >&2
    echo "To restore raw data, stop PostgreSQL and copy the directory into the PGDATA volume." >&2
    exit 1
fi

if [ -z "$DUMP_FILE" ]; then
    echo "ERROR: No dump file (.sql, .dump, .custom, .tar) found in $EXTRACT_DIR" >&2
    exit 1
fi

echo "Found dump file: $DUMP_FILE (type: $DUMP_TYPE)"

# 4. Perform restore
if command -v psql >/dev/null 2>&1 || command -v pg_restore >/dev/null 2>&1; then
    # Mode A: Native PostgreSQL client utilities available
    if [ "$DUMP_TYPE" = "custom" ]; then
        if ! command -v pg_restore >/dev/null 2>&1; then
            echo "ERROR: pg_restore binary not found in PATH." >&2
            exit 1
        fi
        echo "Restoring via pg_restore..."
        if [ -n "${DATABASE_URL:-}" ]; then
            pg_restore -d "$DATABASE_URL" --no-owner --clean --if-exists "$@" "$DUMP_FILE"
        else
            export PGHOST PGPORT PGUSER PGDATABASE
            pg_restore --no-owner --clean --if-exists "$@" "$DUMP_FILE"
        fi
    else
        if ! command -v psql >/dev/null 2>&1; then
            echo "ERROR: psql binary not found in PATH." >&2
            exit 1
        fi
        echo "Restoring via psql..."
        if [ -n "${DATABASE_URL:-}" ]; then
            psql "$DATABASE_URL" -v ON_ERROR_STOP=1 "$@" -f "$DUMP_FILE"
        else
            export PGHOST PGPORT PGUSER PGDATABASE
            psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d "$PGDATABASE" -v ON_ERROR_STOP=1 "$@" -f "$DUMP_FILE"
        fi
    fi
elif command -v docker >/dev/null 2>&1 && [ -S "/var/run/docker.sock" ]; then
    # Mode B: Docker CLI and socket available, pipe into running PostgreSQL container
    echo "Local postgres utilities not found; executing inside container '$POSTGRES_CONTAINER' via docker..."
    if [ "$DUMP_TYPE" = "custom" ]; then
        docker exec -i "$POSTGRES_CONTAINER" pg_restore -U "$PGUSER" -d "$PGDATABASE" --no-owner --clean --if-exists "$@" < "$DUMP_FILE"
    else
        docker exec -i "$POSTGRES_CONTAINER" psql -U "$PGUSER" -d "$PGDATABASE" -v ON_ERROR_STOP=1 "$@" < "$DUMP_FILE"
    fi
else
    echo "ERROR: Neither PostgreSQL client tools (psql / pg_restore) nor Docker CLI were found in PATH." >&2
    echo "To use this script hook, ensure either:" >&2
    echo "  1. psql / pg_restore is installed in the container running dvb, or" >&2
    echo "  2. docker CLI is installed and /var/run/docker.sock is mounted into the container." >&2
    exit 1
fi

echo "PostgreSQL restore completed successfully."
