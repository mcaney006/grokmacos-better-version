#!/bin/bash

# verify-repos.sh
# Verifies the state of both repos and checks for proprietary info leaks

set -e

# ANSI colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${CYAN}   Grok for Mac - Repository Verification${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""

# Proprietary patterns
PROPRIETARY_PATTERNS=(
    "topoffunnel.com"
    "a+vXV7cwhCxuoSLMpuoX8e1G8O223alkm0FX+QxYHlk="
)

echo -e "${CYAN}Local Working Directory:${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━"

# Check local files for proprietary info
LOCAL_HAS_PROPRIETARY=0
for pattern in "${PROPRIETARY_PATTERNS[@]}"; do
    if grep -rq "$pattern" Sources/Info.plist 2>/dev/null; then
        LOCAL_HAS_PROPRIETARY=1
        break
    fi
done

if [[ $LOCAL_HAS_PROPRIETARY -eq 1 ]]; then
    echo -e "  Info.plist: ${GREEN}Contains proprietary config${NC} (correct for private work)"
else
    echo -e "  Info.plist: ${YELLOW}Sanitized (placeholder values)${NC}"
fi

if [[ -f "appcast.xml" ]]; then
    echo -e "  appcast.xml: ${GREEN}Present${NC}"
else
    echo -e "  appcast.xml: ${YELLOW}Not present${NC}"
fi

echo ""
echo -e "${CYAN}Remote Repositories:${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━"

# Check remotes
echo ""
echo "Configured remotes:"
git remote -v | head -4

echo ""
echo -e "${CYAN}Commit Status:${NC}"
echo "━━━━━━━━━━━━━━"

LOCAL_COMMIT=$(git rev-parse HEAD)
echo "Local HEAD: $LOCAL_COMMIT"

# Check if private remote is in sync
PRIVATE_COMMIT=$(git ls-remote origin main 2>/dev/null | cut -f1 || echo "unknown")
if [[ "$PRIVATE_COMMIT" == "$LOCAL_COMMIT" ]]; then
    echo -e "Private (origin): ${GREEN}In sync${NC}"
elif [[ "$PRIVATE_COMMIT" == "unknown" ]]; then
    echo -e "Private (origin): ${YELLOW}Unable to check${NC}"
else
    echo -e "Private (origin): ${YELLOW}Behind local ($PRIVATE_COMMIT)${NC}"
fi

# Check if public remote
PUBLIC_COMMIT=$(git ls-remote public main 2>/dev/null | cut -f1 || echo "unknown")
if [[ "$PUBLIC_COMMIT" == "unknown" ]]; then
    echo -e "Public: ${YELLOW}Unable to check${NC}"
else
    echo -e "Public: $PUBLIC_COMMIT"
fi

echo ""
echo -e "${CYAN}Recommendations:${NC}"
echo "━━━━━━━━━━━━━━━━"

if [[ $LOCAL_HAS_PROPRIETARY -eq 1 ]]; then
    echo -e "  ${GREEN}✓${NC} Your local files have proprietary config (good for private work)"
    echo -e "  ${YELLOW}!${NC} Before pushing to public, run: ./scripts/prepare-public-push.sh"
else
    echo -e "  ${YELLOW}!${NC} Your local files are sanitized"
    echo -e "  ${YELLOW}!${NC} Remember to restore proprietary config before pushing to private"
fi

echo ""
