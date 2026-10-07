#!/bin/sh
#
# bootstrap.sh — end-to-end demo for the dvb postgres_backup example
#
# Runs every step documented in docker-compose.example.yml in order:
#   1. Start the stack
#   2. Wait for services to be healthy
#   3. Seed the database with sample data
#   4. Check dvb configuration and connectivity
#   5. Trigger a one-shot backup
#   6. List remote backups
#   7. Inspect the SeaweedFS web UIs
#   8. Dry-run retention prune
#   9. Restore the latest backup (using pg_restore.sh)
#
# Usage:
#   chmod +x bootstrap.sh
#   ./bootstrap.sh
#
# Prerequisites: docker, docker compose v2

set -eu

COMPOSE_FILE="docker-compose.example.yml"
BACKUP_CONTAINER="backup"
DB_CONTAINER="db"

# ─── helpers ────────────────────────────────────────────────────────────────

step() {
    printf '\n\n\033[1;36m==> %s\033[0m\n' "$*"
}

wait_healthy() {
    container="$1"
    max=30
    i=0
    printf 'Waiting for %s to be healthy' "$container"
    while [ $i -lt $max ]; do
        status=$(docker inspect --format '{{.State.Health.Status}}' "$container" 2>/dev/null || echo "starting")
        if [ "$status" = "healthy" ]; then
            printf ' done.\n'
            return 0
        fi
        printf '.'
        sleep 2
        i=$((i + 1))
    done
    printf '\nERROR: %s did not become healthy in time.\n' "$container" >&2
    exit 1
}

# Poll until the container is in "running" state (not restarting / starting).
wait_running() {
    container="$1"
    max=30
    i=0
    printf 'Waiting for %s to be running' "$container"
    while [ $i -lt $max ]; do
        state=$(docker inspect --format '{{.State.Status}}' "$container" 2>/dev/null || echo "missing")
        if [ "$state" = "running" ]; then
            printf ' done.\n'
            return 0
        fi
        printf '.'
        sleep 2
        i=$((i + 1))
    done
    printf '\nERROR: %s did not reach running state in time.\n' "$container" >&2
    exit 1
}

# ─── 1. Start the stack ──────────────────────────────────────────────────────

step "Starting the stack"
docker compose -f "$COMPOSE_FILE" up -d

# ─── 2. Wait for services ────────────────────────────────────────────────────

step "Waiting for s3 (SeaweedFS) to be healthy"
wait_healthy s3

step "Waiting for db (PostgreSQL) to be ready"
# db has no healthcheck; poll pg_isready via docker exec
max=30
i=0
printf 'Waiting for PostgreSQL to accept connections'
while [ $i -lt $max ]; do
    if docker exec "$DB_CONTAINER" pg_isready -q 2>/dev/null; then
        printf ' done.\n'
        break
    fi
    printf '.'
    sleep 2
    i=$((i + 1))
done

step "Waiting for backup (dvb daemon) to be running"
wait_running "$BACKUP_CONTAINER"

# ─── 3. Seed sample data ─────────────────────────────────────────────────────

step "Seeding sample data into the database"
docker exec "$DB_CONTAINER" psql -U postgres -d app -c "
  CREATE TABLE IF NOT EXISTS demo (id serial PRIMARY KEY, note text, created_at timestamptz DEFAULT now());
  INSERT INTO demo (note) VALUES ('bootstrap demo row 1'), ('bootstrap demo row 2');
  SELECT * FROM demo;
"

# ─── 4. Check dvb configuration and connectivity ─────────────────────────────

step "Checking dvb configuration and connectivity"
docker exec "$BACKUP_CONTAINER" dvb check

# ─── 5. One-shot backup ──────────────────────────────────────────────────────

step "Running a one-shot backup"
docker exec "$BACKUP_CONTAINER" dvb backup db

# ─── 6. List remote backups ──────────────────────────────────────────────────

step "Listing remote backups"
docker exec "$BACKUP_CONTAINER" dvb list db

# ─── 7. Web UIs ──────────────────────────────────────────────────────────────

step "SeaweedFS web UIs (open in your browser)"
printf '  Filer (file browser):   http://localhost:8888/buckets/dvb/\n'
printf '  Master (cluster info):  http://localhost:9333/\n'

# ─── 8. Dry-run retention prune ──────────────────────────────────────────────

step "Dry-run retention prune (no files deleted)"
docker exec "$BACKUP_CONTAINER" dvb prune db --dry-run

# ─── 9. Restore the latest backup ───────────────────────────────────────────

step "Restoring the latest backup"
# dvb extracts the archive to /restore inside the backup container.
# We then pipe dump.sql from there into psql in the db container using
# docker on the host — no psql or docker CLI needed inside dvb itself.
docker exec "$BACKUP_CONTAINER" dvb restore db
DUMP_PATH=$(docker exec "$BACKUP_CONTAINER" find /restore -name 'dump.sql' | head -1)
printf 'Restoring from %s\n' "$DUMP_PATH"
docker exec "$BACKUP_CONTAINER" cat "$DUMP_PATH" | \
    docker exec -i "$DB_CONTAINER" psql -U postgres -d app -v ON_ERROR_STOP=1

printf '\n\033[1;32mDemo complete.\033[0m\n'
printf 'To tear down the stack:\n'
printf '  docker compose -f %s down -v\n' "$COMPOSE_FILE"
