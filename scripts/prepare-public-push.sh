#!/bin/bash

# prepare-public-push.sh
# Sanitizes proprietary information and pushes to the public repo
# Usage: ./scripts/prepare-public-push.sh [commit-message]

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
echo -e "${CYAN}   Grok for Mac - Public Repo Push Preparation${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""

# Check if we're on main branch
CURRENT_BRANCH=$(git branch --show-current)
if [[ "$CURRENT_BRANCH" != "main" ]]; then
    echo -e "${YELLOW}⚠️  Warning: Not on main branch (currently on: $CURRENT_BRANCH)${NC}"
    read -p "Continue anyway? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 1
    fi
fi

# Check for uncommitted changes
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo -e "${YELLOW}⚠️  You have uncommitted changes. Please commit or stash them first.${NC}"
    git status --short
    exit 1
fi

# Files to completely remove from public repo (contain proprietary URLs)
PROPRIETARY_FILES=(
    "appcast.xml"
    "release.sh"
    "release_repackage.sh"
    "deploy-notarized.sh"
    "RELEASE-WORKFLOW.md"
    "RELEASE_WORKFLOW_2025.md"
    "NOTARIZED-RELEASE-WORKFLOW.md"
    "DEVELOPMENT.md"
)

# Backup original files
echo -e "${GREEN}Step 1/5: Backing up original files...${NC}"
BACKUP_DIR=$(mktemp -d)
cp Sources/Info.plist "$BACKUP_DIR/Info.plist"
cp .gitignore "$BACKUP_DIR/.gitignore"

# Store which proprietary files exist
for file in "${PROPRIETARY_FILES[@]}"; do
    if [[ -f "$file" ]]; then
        cp "$file" "$BACKUP_DIR/$file"
    fi
done

echo -e "${GREEN}Step 2/5: Sanitizing Info.plist...${NC}"

# Sanitize Info.plist - replace proprietary URLs
sed -i '' 's|https://www\.topoffunnel\.com/downloads/appcast\.xml|https://your-domain.com/appcast.xml|g' Sources/Info.plist
sed -i '' 's|a+vXV7cwhCxuoSLMpuoX8e1G8O223alkm0FX+QxYHlk=|YOUR_SPARKLE_PUBLIC_KEY|g' Sources/Info.plist

echo -e "${GREEN}Step 3/5: Removing proprietary files from tracking...${NC}"

# Remove proprietary files from git tracking
for file in "${PROPRIETARY_FILES[@]}"; do
    if git ls-files --error-unmatch "$file" &>/dev/null 2>&1; then
        git rm --cached "$file" 2>/dev/null || true
        echo "  Removed from tracking: $file"
    fi
done

# Update .gitignore for public repo
echo -e "${GREEN}Step 4/5: Updating .gitignore for public repo...${NC}"

# Add proprietary files to .gitignore if not already there
for file in "${PROPRIETARY_FILES[@]}"; do
    if ! grep -q "^${file}$" .gitignore 2>/dev/null; then
        echo "$file" >> .gitignore
    fi
done

echo -e "${GREEN}Step 5/5: Creating sanitized commit and pushing...${NC}"

# Get commit message
COMMIT_MSG="${1:-Sync with development (sanitized for public)}"

git add Sources/Info.plist .gitignore
git commit -m "$COMMIT_MSG

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>" || echo "No changes to commit"

# Push to public
echo ""
echo -e "${CYAN}Pushing to public repo...${NC}"
git push public main

echo ""
echo -e "${GREEN}✅ Successfully pushed to public repo!${NC}"
echo ""

# Restore original state for private repo
echo -e "${YELLOW}Restoring original files for private repo...${NC}"

# Restore Info.plist
cp "$BACKUP_DIR/Info.plist" Sources/Info.plist

# Restore .gitignore
cp "$BACKUP_DIR/.gitignore" .gitignore

# Re-add proprietary files to tracking
for file in "${PROPRIETARY_FILES[@]}"; do
    if [[ -f "$BACKUP_DIR/$file" ]]; then
        # File exists in backup, restore and re-add
        git add "$file" 2>/dev/null || true
    fi
done

git add Sources/Info.plist .gitignore
git commit -m "Restore proprietary config for private repo

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>" || echo "No changes to commit"

# Cleanup backup
rm -rf "$BACKUP_DIR"

echo ""
echo -e "${GREEN}✅ Private repo configuration restored!${NC}"
echo ""
echo -e "${CYAN}Summary:${NC}"
echo "  - Public repo (xai-grok): pushed sanitized version"
echo "  - Private repo (xai-grok-private): ready to push with full config"
echo ""
echo -e "${YELLOW}To sync private repo, run:${NC}"
echo "  git push origin main"
echo ""
