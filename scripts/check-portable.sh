#!/usr/bin/env bash
# Every shell script the gates run works on Linux, macOS and Windows (Git Bash).
#
# ⚠️ Rung 3, because the failure is silent on the machine that introduces it. `gates.sh` ran
# on one OS for seven milestones; on Windows every Python gate failed (`python3` is the Store
# stub there), and nothing said so, because nobody had run it on Windows. A GNU-only flag or a
# bash-4 form breaks macOS the same way: green where it was written, red everywhere else.
#
#   scripts/check-portable.sh              scan scripts/
#   scripts/check-portable.sh --selftest   prove it refuses each form and accepts the idioms
#
# Scanned: every file under scripts/ whose first line is a bash or sh shebang -- so the
# pre-commit hook too. Full-line comments are skipped; prose may name what code may not use.
#
# A script that is Linux-only by nature says so on a line of its own:
#
#   # portable: no -- <why>
#
# ⚠️ **Refused on `gates.sh`, the hook, and every script `gates.sh` runs** -- derived by
# reading `gates.sh`, so a script added there is covered the moment it is added. Exempting a
# gated script would switch the check off on exactly what the `portable` CI job exists to run.
#
# ⚠️ **It is a grep.** It catches the forms below and no others. bash 3.2's `set -u` on an
# empty array, or a test that assumes `/`, is the `portable` CI job's to find.
set -euo pipefail
cd "$(dirname "$0")/.."

# form<TAB>why. ERE, matched against non-comment lines.
FORMS='(^|[^[:alnum:]_./-])python3([^[:alnum:]_.]|$)	python3 is the Store stub on Windows: source scripts/lib/py.sh and use py
(^|[^[:alnum:]_])sed[[:space:]]+-i	sed -i takes a suffix argument on BSD sed
(^|[^[:alnum:]_])grep[[:space:]]+-[[:alnum:]]*P	grep -P does not exist in BSD grep
(^|[^[:alnum:]_])base64[[:space:]]+-w	base64 -w is GNU-only: pipe through tr -d "\\n"
(^|[^[:alnum:]_])readlink[[:space:]]+-f	readlink -f is GNU-only
(^|[^[:alnum:]_])date[[:space:]]+(-[[:alnum:]]*[[:space:]]+)*-d	date -d is GNU-only
(^|[^[:alnum:]_])stat[[:space:]]+-c	stat -c is GNU-only
(^|[^[:alnum:]_-])nproc([^[:alnum:]_-]|$)	nproc is absent on macOS: getconf _NPROCESSORS_ONLN
(^|[^[:alnum:]_$-])timeout[[:space:]]+[0-9]	timeout is absent on macOS
(^|[^[:alnum:]_])(mapfile|readarray)([^[:alnum:]_]|$)	mapfile is bash 4; macOS has bash 3.2
declare[[:space:]]+-[[:alnum:]]*A	associative arrays are bash 4; macOS has bash 3.2
(^|[^[:alnum:]_$])/tmp/	a fixed /tmp path: use mktemp'

# A `.py` run as a command rather than through `py`.
# Lines arrive as `N:text` from `grep -n`, so a line start is `^N:`.
DIRECT_PY='(^[0-9]+:|[;&|(!]|then|do|else|[[:space:]]|<\()[[:space:]]*\./scripts/[[:alnum:]_.-]+\.py'

is_shell() { head -1 "$1" 2>/dev/null | grep -qE '^#!.*[/ ](ba)?sh([[:space:]]|$)'; }

# The scripts `gates.sh` runs, plus itself and the hook -- TRANSITIVELY, because
# `selftest-review.sh` runs `review.sh`, and a direct-only reading would let that one be
# exempted. Read from the files, never listed.
gated() {
    local root=$1 seen next s
    seen=$(printf '%s\n%s' "$root/scripts/gates.sh" "$root/scripts/githooks/pre-commit")
    next=$seen
    while [ -n "$next" ]; do
        next=$(printf '%s\n' "$next" | while IFS= read -r s; do
                   [ -f "$s" ] && grep -oE '\./scripts/[[:alnum:]_.-]+\.sh' "$s" | sed "s#^\./#$root/#"
               done | sort -u | while IFS= read -r s; do
                   printf '%s\n' "$seen" | grep -qxF "$s" || echo "$s"
               done)
        if [ -n "$next" ]; then seen=$(printf '%s\n%s' "$seen" "$next"); fi
    done
    printf '%s\n' "$seen"
}

scan() {
    local root=$1 bad=0 f rel lines form why hit
    local gated_list; gated_list=$(gated "$root")
    while IFS= read -r f; do
        is_shell "$f" || continue
        rel=${f#"$root"/}
        # This file IS the list of forms, and its fixtures must spell each one out.
        [ "$rel" = scripts/check-portable.sh ] && continue
        if grep -qE '^#[[:space:]]*portable:[[:space:]]*no' "$f"; then
            if printf '%s\n' "$gated_list" | grep -qxF "$f"; then
                echo "  $rel: declares 'portable: no', but gates.sh runs it on every OS"
                bad=1
            fi
            continue
        fi
        # `py ./scripts/x.py` is the sanctioned form, so it is removed before the direct-run
        # pattern looks for a `./scripts/x.py` after a space.
        lines=$(grep -nvE '^[[:space:]]*#' "$f" |
                sed -E 's#(^|[^[:alnum:]_])py[[:space:]]+\./scripts/[[:alnum:]_.-]+\.py#\1#g' || true)
        while IFS='	' read -r form why; do
            [ -n "$form" ] || continue
            # `lib/py.sh` is where `python3` is tried as a candidate; it is the definition.
            case "$rel:$form" in scripts/lib/py.sh:*python3*) continue ;; esac
            hit=$(printf '%s\n' "$lines" | grep -E "$form" || true)
            if [ -n "$hit" ]; then
                printf '%s\n' "$hit" | while IFS= read -r l; do echo "  $rel:$l -- $why"; done
                bad=1
            fi
        done <<EOF
$FORMS
EOF
        hit=$(printf '%s\n' "$lines" | grep -E "$DIRECT_PY" || true)
        if [ -n "$hit" ]; then
            printf '%s\n' "$hit" | while IFS= read -r l; do
                echo "  $rel:$l -- a .py run directly uses its python3 shebang: use py"
            done
            bad=1
        fi
    done <<EOF
$(find "$root/scripts" -type f 2>/dev/null | sort)
EOF
    # ⚠️ **The exec bit, read from the index.** Windows has no exec bit, so a script created
    # there is committed 100644 and every Linux and macOS checkout runs it as "Permission
    # denied" -- while the Windows author, and a container over an NTFS bind mount that shows
    # every file executable, see nothing wrong. Found in review of the commit adding this file,
    # which had exactly that defect. Sourced helpers have no shebang and need no bit.
    if git -C "$root" rev-parse --git-dir >/dev/null 2>&1; then
        local mode path
        while read -r mode _ _ path; do
            [ -f "$root/$path" ] && is_shell "$root/$path" || continue
            if [ "$mode" != 100755 ]; then
                echo "  $path: committed as $mode -- run: git update-index --chmod=+x $path"
                bad=1
            fi
        done <<EOF
$(git -C "$root" ls-files -s -- scripts)
EOF
    fi
    return "$bad"
}

if [ "${1:-}" = "--selftest" ]; then
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    # A fresh tree per case, with a gates.sh that runs `gated.sh` and not `free.sh`.
    fixture() { # $1 = file name under scripts/, stdin = its body
        rm -rf "$tmp/t"; mkdir -p "$tmp/t/scripts/githooks" "$tmp/t/scripts/lib"
        printf '#!/usr/bin/env bash\n./scripts/gated.sh\n' > "$tmp/t/scripts/gates.sh"
        printf '#!/usr/bin/env bash\n./scripts/gates.sh\n' > "$tmp/t/scripts/githooks/pre-commit"
        printf '#!/usr/bin/env bash\ntrue\n' > "$tmp/t/scripts/gated.sh"
        { printf '#!/usr/bin/env bash\n'; cat; } > "$tmp/t/scripts/$1"
    }
    n=0
    refuses() { # $1 = case name, $2 = file, stdin = body
        fixture "$2"
        if scan "$tmp/t" >/dev/null 2>&1; then
            echo "FAIL check-portable.sh accepted: $1" >&2; exit 1
        fi
        n=$((n + 1))
    }
    accepts() {
        fixture "$2"
        if ! scan "$tmp/t" >/dev/null 2>&1; then
            echo "FAIL check-portable.sh refused legitimate input: $1" >&2
            scan "$tmp/t" >&2 || true; exit 1
        fi
        n=$((n + 1))
    }
    refuses "bare python3"        free.sh <<<'python3 -c "print(1)"'
    refuses "piped python3"       free.sh <<<'echo x | python3 -c "import sys"'
    refuses "direct .py"          free.sh <<<'./scripts/check-slos.py'
    refuses "direct .py in <()"   free.sh <<<'done < <(./scripts/mutants-scope.py a)'
    refuses "sed -i"              free.sh <<<"sed -i 's/a/b/' f"
    refuses "grep -oP"            free.sh <<<"grep -oP '\\d+' f"
    refuses "base64 -w0"          free.sh <<<'base64 -w0 < f'
    refuses "readlink -f"         free.sh <<<'readlink -f .'
    refuses "date -u -d"          free.sh <<<'date -u -d @0'
    refuses "stat -c"             free.sh <<<'stat -c %s f'
    refuses "nproc"               free.sh <<<'echo $(( $(nproc) / 2 ))'
    refuses "timeout"             free.sh <<<'timeout 30 docker run x'
    refuses "mapfile"             free.sh <<<'mapfile -t a < f'
    refuses "declare -A"          free.sh <<<'declare -A m'
    refuses "fixed /tmp path"     free.sh <<<'echo x > /tmp/pstore.log'
    refuses "a sh script too"     free.sh <<<'sed -i s/a/b/ f'
    refuses "exempting a gated script" gated.sh <<<'# portable: no -- it is convenient
sed -i s/a/b/ f'
    fixture free.sh <<<'true'
    printf '#!/usr/bin/env bash\n./scripts/deep.sh\n' > "$tmp/t/scripts/gated.sh"
    printf '#!/usr/bin/env bash\n# portable: no -- nope\n' > "$tmp/t/scripts/deep.sh"
    if scan "$tmp/t" >/dev/null 2>&1; then
        echo "FAIL accepted an exemption on a script gates.sh runs indirectly" >&2; exit 1
    fi
    n=$((n + 1))
    fixture free.sh <<<'true'
    printf '#!/usr/bin/env bash\n# portable: no -- nope\n' > "$tmp/t/scripts/githooks/pre-commit"
    if scan "$tmp/t" >/dev/null 2>&1; then echo "FAIL accepted an exempt pre-commit hook" >&2; exit 1; fi
    n=$((n + 1))

    accepts "py and a quoted .py"  free.sh <<<'. scripts/lib/py.sh
py ./scripts/check-slos.py
run "scripts/build-index.py --check" py ./scripts/build-index.py --check'
    accepts "a comment naming every form" free.sh <<<'# never: python3, sed -i, grep -P, nproc, timeout 30, /tmp/x
true'
    accepts "mktemp and getconf"   free.sh <<<'T=$(mktemp -d); n=$(getconf _NPROCESSORS_ONLN)'
    accepts "an exempt ungated script" free.sh <<<'# portable: no -- host networking
timeout 30 docker run x'
    accepts "a variable named like a form" free.sh <<<'MINIO_TIMEOUT=3; echo "$TMPDIR/x"; python3_ok=1'
    fixture free.sh <<<'true'
    printf 'sed -i s/a/b/ f\n' > "$tmp/t/scripts/notes.txt"
    scan "$tmp/t" >/dev/null 2>&1 || { echo "FAIL scanned a file with no shell shebang" >&2; exit 1; }
    n=$((n + 1))
    # The exec bit, which lives in the index and not on a Windows filesystem.
    fixture free.sh <<<'true'
    git -C "$tmp/t" init -q && git -C "$tmp/t" -c core.autocrlf=false add -A
    git -C "$tmp/t" update-index --chmod=+x scripts/gates.sh scripts/gated.sh scripts/githooks/pre-commit
    git -C "$tmp/t" update-index --chmod=-x scripts/free.sh
    if scan "$tmp/t" >/dev/null 2>&1; then echo "FAIL accepted a script committed without +x" >&2; exit 1; fi
    git -C "$tmp/t" update-index --chmod=+x scripts/free.sh
    scan "$tmp/t" >/dev/null 2>&1 || { echo "FAIL refused scripts that are all +x" >&2; exit 1; }
    n=$((n + 2))
    fixture lib/py.sh <<<'for _c in python3 python "py -3"; do :; done'
    scan "$tmp/t" >/dev/null 2>&1 || { echo "FAIL refused lib/py.sh naming python3" >&2; exit 1; }
    n=$((n + 1))
    echo "ok check-portable.sh: $n cases, refuses each form and a gated exemption, accepts the idioms"
    exit 0
fi

if ! scan .; then
    echo "FAIL a shell script under scripts/ is not portable (see above)" >&2
    exit 1
fi
echo "ok every shell script under scripts/ is portable, or declares why it is not"
