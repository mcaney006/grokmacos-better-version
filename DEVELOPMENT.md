# Grok for Mac - Development Guide

## Repository Structure

This project uses a dual-repository setup:

| Repository | URL | Purpose |
|------------|-----|---------|
| **Private** (origin) | `xai-grok-private` | Full source with proprietary configuration |
| **Public** | `xai-grok` | Open source version with sanitized config |

## Proprietary Information

The following items are considered proprietary and must **NEVER** appear in the public repo:

1. **Sparkle Update URLs** - `topoffunnel.com` domain
2. **Sparkle Public Key** - The `SUPublicEDKey` value
3. **appcast.xml** - Auto-update manifest file

## Daily Workflow

### Normal Development (Private Repo)

For everyday work, just use standard git commands. The private repo is the default:

```bash
# Make changes, commit, and push to private
git add .
git commit -m "Your commit message"
git push origin main
# or use the helper script:
./scripts/push-private.sh
```

### Syncing to Public Repo

When you want to update the public/open-source version:

```bash
# This script automatically:
# 1. Sanitizes proprietary info
# 2. Pushes to public
# 3. Restores proprietary config
# 4. Leaves you ready to push to private

./scripts/prepare-public-push.sh "Commit message for public"
git push origin main  # Push the restored config to private
```

## Safety Mechanisms

### Pre-Push Hook

A git hook automatically blocks pushes to the public repo if proprietary information is detected:

```
🚫 Push to PUBLIC repo BLOCKED to protect proprietary information
```

If you see this, run `./scripts/prepare-public-push.sh` to sanitize first.

### Verification Script

Check the status of both repos:

```bash
./scripts/verify-repos.sh
```

This shows:
- Whether local files have proprietary config
- Sync status with both remotes
- Recommendations for next steps

## Scripts Reference

| Script | Purpose |
|--------|---------|
| `scripts/push-private.sh` | Quick push to private repo |
| `scripts/prepare-public-push.sh` | Sanitize and push to public |
| `scripts/verify-repos.sh` | Check status of both repos |

## File Differences Between Repos

### Sources/Info.plist

**Private (has real values):**
```xml
<key>SUFeedURL</key>
<string>https://your-actual-domain.com/appcast.xml</string>
<key>SUPublicEDKey</key>
<string>[YOUR-REAL-SPARKLE-KEY]</string>
```

**Public (has placeholders):**
```xml
<key>SUFeedURL</key>
<string>https://your-domain.com/appcast.xml</string>
<key>SUPublicEDKey</key>
<string>YOUR_SPARKLE_PUBLIC_KEY</string>
```

### appcast.xml

- **Private**: File exists with real update information
- **Public**: File is gitignored and not tracked

## Troubleshooting

### "Push blocked" error

Run the sanitization script:
```bash
./scripts/prepare-public-push.sh
```

### Accidentally pushed proprietary info to public

1. Immediately update the Sparkle keys (they're now compromised)
2. Force push a sanitized version to public
3. Update the new keys in the private repo

### Local files are sanitized but shouldn't be

Check git status and restore from private repo:
```bash
git checkout origin/main -- Sources/Info.plist
```

## Building the App

```bash
# Debug build
/Applications/Xcode-beta.app/Contents/Developer/usr/bin/xcodebuild \
  -project GrokApp.xcodeproj \
  -scheme Grok \
  -configuration Debug \
  build

# Release build
/Applications/Xcode-beta.app/Contents/Developer/usr/bin/xcodebuild \
  -project GrokApp.xcodeproj \
  -scheme Grok \
  -configuration Release \
  build
```

## Important Reminders

1. **Always work with proprietary config locally** - The private repo is your main workspace
2. **Never manually push to public** - Always use `prepare-public-push.sh`
3. **The pre-push hook is your safety net** - Don't disable it
4. **Verify before releases** - Run `verify-repos.sh` before any major release
