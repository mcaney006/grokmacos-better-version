#!/bin/bash

# push-private.sh
# Simple script to push to private repo (default workflow)
# This is the safe, everyday push - no sanitization needed

set -e

CYAN='\033[0;36m'
GREEN='\033[0;32m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo -e "${CYAN}Pushing to private repo (xai-grok-private)...${NC}"
git push origin main
echo -e "${GREEN}✅ Successfully pushed to private repo${NC}"
