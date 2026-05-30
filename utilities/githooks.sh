#!/bin/bash
# Setup script for Git pre-commit hooks

set -e

# ANSI color codes
WHITE='\033[1;37m'
YELLOW='\033[1;33m'
RESET='\033[0m'

echo ""
echo -e " ${WHITE}Setting up Git pre-commit hooks...${RESET}"
echo ""

# Resolve repo root and script directory so the script works from anywhere
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo -e " ${YELLOW}Error: Not in a Git repository. Please run this from inside the project.${RESET}"
    exit 1
fi

GIT_DIR="$(git -C "$REPO_ROOT" rev-parse --git-dir)"
# git rev-parse may return a relative path; normalise it against the repo root
case "$GIT_DIR" in
    /*) ;;
    *) GIT_DIR="$REPO_ROOT/$GIT_DIR" ;;
esac

HOOKS_DIR="$GIT_DIR/hooks"
SOURCE_HOOK="$SCRIPT_DIR/pre-commit"
TARGET_HOOK="$HOOKS_DIR/pre-commit"

# Check the source hook is present before we touch anything
if [ ! -f "$SOURCE_HOOK" ]; then
    echo -e " ${YELLOW}Error: Source hook not found at ${SOURCE_HOOK}.${RESET}"
    exit 1
fi

# Check if pre-commit hook already exists
if [ -f "$TARGET_HOOK" ]; then
    echo -e " ${YELLOW}Pre-commit hook already exists.${RESET}"
    read -p " Do you want to overwrite it? (y/N): " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo ""
        echo -e " ${YELLOW}Setup cancelled.${RESET}"
        exit 0
    fi
fi

# Make sure the hooks directory exists
mkdir -p "$HOOKS_DIR"

# Copy the hook file and make sure it's executable
cp "$SOURCE_HOOK" "$TARGET_HOOK"
chmod u+x "$TARGET_HOOK"

echo ""
echo -e " ${WHITE}Pre-commit hook setup complete!${RESET}"
echo
echo -e "${YELLOW} The hook will now run these checks before each commit:${RESET}"
echo -e " - ${WHITE}cargo fmt -- --check${RESET} (code formatting)"
echo -e " - ${WHITE}cargo clippy --all-targets -- -D warnings${RESET} (linting)"
echo -e " - ${WHITE}cargo check${RESET} (compilation)"
echo
echo -e " To test the hook manually, run: ${WHITE}${TARGET_HOOK}${RESET}"
echo -e " To skip the hook for a commit, use: ${WHITE}git commit --no-verify${RESET}${YELLOW}, not recommended.${RESET}"
