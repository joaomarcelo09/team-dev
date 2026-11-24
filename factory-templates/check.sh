#!/bin/sh
# Offline checks for the project templates. See README.md.
#
# Fails when a template lacks README.md or .github/workflows/ci.yml, when the CI workflow lacks
# lint, format, test, type-check or build steps, when STANDARDS_VERSION differs from the standards
# pack VERSION, or when a YAML, JSON or TOML file does not parse. No network access.

set -u

templates_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$templates_dir/.." && pwd)
pack_version_file=$repo_root/factory-skills/standards/VERSION
templates_version_file=$templates_dir/STANDARDS_VERSION

failures=0

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    failures=$((failures + 1))
}

# Both version files exist and agree.
if [ ! -f "$pack_version_file" ]; then
    fail "missing $pack_version_file"
elif [ ! -f "$templates_version_file" ]; then
    fail "missing $templates_version_file"
else
    pack_version=$(tr -d '[:space:]' < "$pack_version_file")
    templates_version=$(tr -d '[:space:]' < "$templates_version_file")
    if [ "$pack_version" != "$templates_version" ]; then
        fail "STANDARDS_VERSION ($templates_version) does not match standards VERSION ($pack_version)"
    else
        printf 'standards pack version %s\n' "$pack_version"
    fi
fi

# Every template has a README and a CI workflow with the required steps.
for dir in "$templates_dir"/*/; do
    [ -d "$dir" ] || continue
    name=$(basename "$dir")
    case $name in
        .*) continue ;;
    esac

    if [ ! -s "$dir/README.md" ]; then
        fail "$name: missing README.md"
    fi

    ci_file=$dir/.github/workflows/ci.yml
    if [ ! -s "$ci_file" ]; then
        fail "$name: missing .github/workflows/ci.yml"
        continue
    fi

    grep -Eqi 'lint|ruff check|clippy' "$ci_file" || fail "$name: CI workflow lacks a lint step"
    grep -Eqi 'format' "$ci_file" || fail "$name: CI workflow lacks a format step"
    grep -Eqi 'test|pytest' "$ci_file" || fail "$name: CI workflow lacks a test step"
    grep -Eqi 'typecheck|type-check|mypy|tsc' "$ci_file" || fail "$name: CI workflow lacks a type-check step"
    grep -Eqi 'build' "$ci_file" || fail "$name: CI workflow lacks a build step"

    printf 'ok: %s\n' "$name"
done

# YAML, JSON and TOML files parse. YAML uses PyYAML when it is installed and basic checks
# otherwise, so the script stays offline.
if ! command -v python3 >/dev/null 2>&1; then
    fail "python3 is required for the parse checks"
elif ! python3 -I - "$templates_dir" <<'PY'
import json
import os
import sys

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

try:
    import yaml
except ModuleNotFoundError:
    yaml = None


def bracket_delta(text):
    """Return the net flow-bracket depth of one line, ignoring quoted text."""
    depth = 0
    quote = None
    escaped = False
    for char in text:
        if quote:
            if escaped:
                escaped = False
            elif char == "\\" and quote == '"':
                escaped = True
            elif char == quote:
                quote = None
        elif char in ("'", '"'):
            quote = char
        elif char in "[{":
            depth += 1
        elif char in "]}":
            depth -= 1
    return depth


def basic_yaml_check(text):
    """Conservative checks for when PyYAML is unavailable: indentation and flow balance."""
    depth = 0
    block_indent = None
    for number, line in enumerate(text.splitlines(), 1):
        indent_text = line[: len(line) - len(line.lstrip(" \t"))]
        if "\t" in indent_text:
            raise ValueError("line %d: tab used for indentation" % number)
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(indent_text)
        if block_indent is not None:
            if indent > block_indent:
                continue
            block_indent = None
        content = line.split("#", 1)[0]
        token = content.rstrip().split()[-1] if content.strip() else ""
        if token in ("|", "|-", "|+", ">", ">-", ">+"):
            block_indent = indent
        depth += bracket_delta(content)
        if depth < 0:
            raise ValueError("line %d: unbalanced brackets" % number)
    if depth != 0:
        raise ValueError("unbalanced brackets at end of document")


root = sys.argv[1]
errors = []
parsed = 0

for current, dirs, files in os.walk(root):
    dirs[:] = sorted(directory for directory in dirs if directory != ".git")
    for name in sorted(files):
        extension = os.path.splitext(name)[1].lower()
        if extension not in (".yml", ".yaml", ".json", ".toml"):
            continue
        path = os.path.join(current, name)
        relative = os.path.relpath(path, root)
        parsed += 1
        try:
            with open(path, encoding="utf-8") as handle:
                text = handle.read()
            if extension == ".json":
                json.loads(text)
            elif extension == ".toml":
                if tomllib is None:
                    raise RuntimeError("tomllib is unavailable (Python 3.11 or newer is required)")
                tomllib.loads(text)
            elif yaml is not None:
                yaml.safe_load(text)
            else:
                basic_yaml_check(text)
        except Exception as error:
            errors.append("%s: %s" % (relative, error))

if yaml is None:
    print("note: PyYAML is not installed; YAML files got basic checks only", file=sys.stderr)
for error in errors:
    print("FAIL: %s" % error, file=sys.stderr)
print("parsed %d data files" % parsed)

sys.exit(1 if errors else 0)
PY
then
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    printf 'templates check failed (%d problem(s))\n' "$failures" >&2
    exit 1
fi

printf 'templates check passed\n'
