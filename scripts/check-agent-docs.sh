#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
root_doc="$repo_root/AGENTS.md"
workflow="$repo_root/.github/workflows/ci.yml"
shared_root="$repo_root/.."
fail() { echo "agent entry check failed: $*" >&2; exit 1; }
test -f "$root_doc" || fail "AGENTS.md is missing"
grep -Fq "[AGENTS.md](AGENTS.md)" "$repo_root/README.md" || fail "README.md does not link AGENTS.md"
grep -Fq "../AGENTS.md" "$root_doc" || fail "module entry does not route to workspace rules"
grep -Fq "../.agents/guides/core/" "$root_doc" || fail "module entry does not route to shared Core guides"
grep -Fq "../.agents/skills/git-pr-local-sync/SKILL.md" "$root_doc" || fail "module entry does not route to shared Git Skill"
test ! -e "$repo_root/docs/git-pr-local-sync.md" || fail "legacy Git workflow document remains"
test ! -d "$repo_root/docs/guides" || fail "legacy module guide directory remains"
if [[ -f "$shared_root/.agents/guides/core/release.md" ]]; then grep -Fq "required checks" "$shared_root/.agents/guides/core/release.md" || fail "shared Core release guide must document required checks"; else echo "Shared Core release guide is outside this standalone checkout; workspace link/content check deferred."; fi
grep -Fq "rust-version = \"1.85\"" "$repo_root/Cargo.toml" || fail "Cargo.toml MSRV changed; update standalone checker"
grep -Eq 'toolchain:[[:space:]]*"?1\.85\.0"?$' "$workflow" || fail "CI minimum Rust toolchain changed; update standalone checker"
grep -Fq "https://github.com/olu-py/1H-Agent-webUI" "$repo_root/README.md" || fail "README.md does not link WebUI"
grep -Fq "https://github.com/olu-py/1H-Agent" "$repo_root/README.md" || fail "README.md does not link TUI"
echo "Core module entry and standalone CI metadata checks passed."
