#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

# Copy .env from example if it doesn't exist
if [ ! -f .env ]; then
    cp .env.example .env
    echo "Created .env from .env.example — edit it with your secrets."
fi

echo "Starting infrastructure services..."
docker compose up -d nats arangodb

# Wait for NATS (port 4222)
echo -n "Waiting for NATS"
for i in $(seq 1 30); do
    if nc -z localhost 4222 2>/dev/null; then
        echo " ready."
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo " FAILED (timeout after 30s)"
        exit 1
    fi
    echo -n "."
    sleep 1
done

# Wait for ArangoDB (port 8529)
echo -n "Waiting for ArangoDB"
for i in $(seq 1 60); do
    if curl -sf http://localhost:8529/_api/version >/dev/null 2>&1; then
        echo " ready."
        break
    fi
    if [ "$i" -eq 60 ]; then
        echo " FAILED (timeout after 60s)"
        exit 1
    fi
    echo -n "."
    sleep 1
done

echo ""
echo "Infrastructure is up:"
echo "  NATS:     nats://localhost:4222"
echo "  NATS Mon: http://localhost:8222"
echo "  ArangoDB: http://localhost:8529"
