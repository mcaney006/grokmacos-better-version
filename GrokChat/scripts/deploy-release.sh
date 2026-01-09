#!/bin/bash

# =============================================================================
# deploy-release.sh - Full Automated Grok Release Deployment
# =============================================================================
#
# This script handles the ENTIRE release process from notarized app to live:
#   1. Creates DMG from notarized app
#   2. Signs DMG with Developer ID
#   3. Notarizes DMG with Apple
#   4. Staples notarization ticket
#   5. Signs with Sparkle (EdDSA)
#   6. Updates appcast.xml
#   7. Deploys to tofu-main-site
#   8. Updates Grok page version
#   9. Commits and pushes
#
# Prerequisites:
#   - Notarized Grok.app at ~/Desktop/Grok.app
#   - notarytool credentials stored: xcrun notarytool store-credentials "notarytool-profile"
#   - Sparkle sign_update tool at GrokChat/bin/sign_update
#
# Usage:
#   ./scripts/deploy-release.sh <version> <build>
#
# Example:
#   ./scripts/deploy-release.sh 1.0.85 52
#
# =============================================================================

set -e  # Exit on any error

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

# Configuration
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
TOFU_DIR="$HOME/Documents/DeveloperProjects/tofu-main-site"
DOWNLOADS_DIR="$TOFU_DIR/public/downloads"
GROK_PAGE="$TOFU_DIR/app/grok/page.tsx"

APP_SOURCE="$HOME/Desktop/Grok.app"
DMG_OUTPUT="$HOME/Desktop/Grok.dmg"
SIGN_TOOL="$PROJECT_DIR/bin/sign_update"
APPCAST="$PROJECT_DIR/appcast.xml"
INFO_PLIST="$PROJECT_DIR/Sources/Info.plist"

DEVELOPER_ID="Developer ID Application: Brandon Charleson (CC989JZCNV)"
NOTARY_PROFILE="notarytool-profile"

# =============================================================================
# Helper Functions
# =============================================================================

print_header() {
    echo ""
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${CYAN}  $1${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

print_step() {
    echo -e "${BLUE}▶ $1${NC}"
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

print_error() {
    echo -e "${RED}✗ $1${NC}"
}

fail() {
    print_error "$1"
    exit 1
}

# =============================================================================
# Validation
# =============================================================================

validate_prerequisites() {
    print_header "Validating Prerequisites"

    # Check version arguments
    if [ -z "$1" ] || [ -z "$2" ]; then
        echo "Usage: $0 <version> <build>"
        echo "Example: $0 1.0.85 52"
        exit 1
    fi

    VERSION="$1"
    BUILD="$2"

    # Validate version format
    if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        fail "Invalid version format: $VERSION (expected X.Y.Z)"
    fi

    # Validate build is a number
    if ! [[ "$BUILD" =~ ^[0-9]+$ ]]; then
        fail "Invalid build number: $BUILD (expected integer)"
    fi

    print_success "Version: $VERSION (Build $BUILD)"

    # Check notarized app exists
    if [ ! -d "$APP_SOURCE" ]; then
        fail "Notarized app not found at $APP_SOURCE"
    fi
    print_success "Found notarized app"

    # Verify app is notarized
    if ! spctl -a -vv "$APP_SOURCE" 2>&1 | grep -q "Notarized Developer ID"; then
        fail "App is not notarized. Export from Xcode Organizer first."
    fi
    print_success "App is notarized"

    # Check sign_update tool
    if [ ! -x "$SIGN_TOOL" ]; then
        fail "Sparkle sign_update tool not found at $SIGN_TOOL"
    fi
    print_success "Sparkle signing tool available"

    # Check notarytool credentials
    if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" &>/dev/null; then
        fail "Notarytool credentials not found. Run: xcrun notarytool store-credentials \"$NOTARY_PROFILE\""
    fi
    print_success "Notarytool credentials valid"

    # Check tofu-main-site exists
    if [ ! -d "$TOFU_DIR" ]; then
        fail "tofu-main-site not found at $TOFU_DIR"
    fi
    print_success "tofu-main-site directory found"
}

# =============================================================================
# Main Process
# =============================================================================

create_dmg() {
    print_header "Step 1/8: Creating DMG"

    rm -f "$DMG_OUTPUT"
    hdiutil create -volname "Grok" -srcfolder "$APP_SOURCE" -ov -format UDZO "$DMG_OUTPUT"

    if [ ! -f "$DMG_OUTPUT" ]; then
        fail "Failed to create DMG"
    fi

    DMG_SIZE=$(stat -f%z "$DMG_OUTPUT")
    DMG_SIZE_MB=$(echo "scale=1; $DMG_SIZE / 1048576" | bc)
    print_success "Created DMG ($DMG_SIZE_MB MB)"
}

sign_dmg() {
    print_header "Step 2/8: Signing DMG with Developer ID"

    codesign --force --sign "$DEVELOPER_ID" "$DMG_OUTPUT"

    if ! codesign -dv "$DMG_OUTPUT" 2>&1 | grep -q "TeamIdentifier=CC989JZCNV"; then
        fail "DMG signing failed"
    fi
    print_success "DMG signed with Developer ID"
}

notarize_dmg() {
    print_header "Step 3/8: Notarizing DMG with Apple"

    print_step "Submitting to Apple (this may take a few minutes)..."

    NOTARY_OUTPUT=$(xcrun notarytool submit "$DMG_OUTPUT" --keychain-profile "$NOTARY_PROFILE" --wait 2>&1)

    if ! echo "$NOTARY_OUTPUT" | grep -q "status: Accepted"; then
        echo "$NOTARY_OUTPUT"
        fail "Notarization failed"
    fi
    print_success "DMG notarized by Apple"
}

staple_dmg() {
    print_header "Step 4/8: Stapling Notarization Ticket"

    xcrun stapler staple "$DMG_OUTPUT"
    print_success "Notarization ticket stapled"
}

sparkle_sign() {
    print_header "Step 5/8: Signing with Sparkle"

    SPARKLE_OUTPUT=$("$SIGN_TOOL" "$DMG_OUTPUT" 2>&1)

    SIGNATURE=$(echo "$SPARKLE_OUTPUT" | grep -o 'edSignature="[^"]*"' | cut -d'"' -f2)
    LENGTH=$(echo "$SPARKLE_OUTPUT" | grep -o 'length="[^"]*"' | cut -d'"' -f2)

    if [ -z "$SIGNATURE" ] || [ -z "$LENGTH" ]; then
        echo "$SPARKLE_OUTPUT"
        fail "Failed to parse Sparkle signature"
    fi

    print_success "Sparkle signature: ${SIGNATURE:0:20}..."
    print_success "Length: $LENGTH bytes"
}

update_appcast() {
    print_header "Step 6/8: Updating appcast.xml"

    PUBDATE=$(date "+%a, %d %b %Y %H:%M:%S %z")

    # Create new item entry
    NEW_ITEM="        <!-- LATEST VERSION: $VERSION (Build $BUILD) -->
        <item>
            <title>Version $VERSION</title>
            <pubDate>$PUBDATE</pubDate>
            <description><![CDATA[
                <h2>What's New in $VERSION</h2>
                <ul>
                    <li>Bug fixes and improvements</li>
                    <li>Universal binary (Apple Silicon + Intel)</li>
                    <li>Apple notarized for enhanced security</li>
                </ul>
            ]]></description>
            <enclosure
                url=\"https://www.topoffunnel.com/downloads/Grok.dmg\"
                sparkle:version=\"$BUILD\"
                sparkle:shortVersionString=\"$VERSION\"
                sparkle:edSignature=\"$SIGNATURE\"
                length=\"$LENGTH\"
                type=\"application/octet-stream\"
            />
            <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
        </item>"

    # Find the line with LATEST VERSION comment and replace with new entry
    # This uses a temp file approach for reliability
    TEMP_FILE=$(mktemp)

    awk -v new_item="$NEW_ITEM" '
        /<!-- LATEST VERSION:/ {
            print new_item
            # Change the old LATEST to just VERSION
            gsub(/LATEST VERSION/, "VERSION")
        }
        { print }
    ' "$APPCAST" > "$TEMP_FILE"

    mv "$TEMP_FILE" "$APPCAST"

    print_success "Updated appcast.xml"
}

update_info_plist() {
    print_header "Step 6b: Updating Info.plist"

    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$INFO_PLIST"
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD" "$INFO_PLIST"

    print_success "Updated Info.plist to $VERSION ($BUILD)"
}

deploy_to_tofu() {
    print_header "Step 7/8: Deploying to tofu-main-site"

    # Copy files
    print_step "Copying DMG..."
    cp "$DMG_OUTPUT" "$DOWNLOADS_DIR/Grok.dmg"

    print_step "Copying appcast.xml..."
    cp "$APPCAST" "$DOWNLOADS_DIR/appcast.xml"

    # Update page version
    print_step "Updating Grok page..."
    DMG_SIZE_MB=$(echo "scale=1; $LENGTH / 1048576" | bc)
    sed -i '' "s/Version [0-9]\+\.[0-9]\+\.[0-9]\+ •/Version $VERSION •/" "$GROK_PAGE"
    sed -i '' "s/• [0-9]\+\.[0-9] MB •/• $DMG_SIZE_MB MB •/" "$GROK_PAGE"

    print_success "Files deployed to tofu-main-site"
}

commit_and_push() {
    print_header "Step 8/8: Committing and Pushing"

    cd "$TOFU_DIR"

    git add app/grok/page.tsx public/downloads/Grok.dmg public/downloads/appcast.xml

    git commit -m "Deploy Grok v$VERSION (Build $BUILD)

- Updated Grok.dmg with notarized universal binary
- Updated appcast.xml with new signature
- Updated page version to $VERSION

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>"

    git push origin main

    print_success "Pushed to tofu-main-site"

    # Also commit to GrokChat
    cd "$PROJECT_DIR"
    git add appcast.xml Sources/Info.plist
    git commit -m "Release v$VERSION (Build $BUILD)

🤖 Generated with [Claude Code](https://claude.com/claude-code)

Co-Authored-By: Claude Opus 4.5 <noreply@anthropic.com>" || true

    git push origin main || true

    print_success "Pushed to xai-grok-private"
}

# =============================================================================
# Main
# =============================================================================

main() {
    print_header "🚀 Grok Release Deployment"
    echo ""
    echo "This script will deploy a new Grok release."
    echo ""

    validate_prerequisites "$1" "$2"

    create_dmg
    sign_dmg
    notarize_dmg
    staple_dmg
    sparkle_sign
    update_appcast
    update_info_plist
    deploy_to_tofu
    commit_and_push

    print_header "🎉 Release Complete!"
    echo ""
    echo -e "${GREEN}Grok v$VERSION (Build $BUILD) is now live!${NC}"
    echo ""
    echo "Deployed to:"
    echo "  • https://www.topoffunnel.com/downloads/Grok.dmg"
    echo "  • https://www.topoffunnel.com/downloads/appcast.xml"
    echo "  • https://www.topoffunnel.com/grok"
    echo ""
    echo "Users will receive the update automatically via Sparkle."
    echo ""
}

main "$@"
