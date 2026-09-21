# Sourced, never run: defines `py`, the one way a shell script here runs Python.
#
#   . "$(dirname "$0")/lib/py.sh"
#   py ./scripts/check-slos.py
#
# ⚠️ **`python3` is not a portable command name.** On Windows it is usually the Microsoft
# Store stub, which prints "Python was not found" and exits 49 -- so every gate that ran a
# `.py` through its `#!/usr/bin/env python3` shebang failed on a machine with a perfectly good
# Python on PATH as `python` or `py`. So each candidate is RUN, not looked up: `command -v`
# finds the stub and calls it success.
#
# ⚠️ `PYTHONUTF8=1` because the tree is UTF-8 and Windows' default text encoding is not: a
# `print` of a `⚠️` to a cp1252 console raises. The scripts also pass `encoding="utf-8"` for
# every file they read, so they are correct when run without this helper too.
#
# `scripts/check-portable.sh` refuses a shell script that names `python3` or runs a
# `scripts/*.py` other than through `py`.
_pstore_py=""
for _c in python3 python "py -3"; do
    # shellcheck disable=SC2086  # "py -3" is a command and an argument, split on purpose
    if $_c -c 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)' >/dev/null 2>&1; then
        _pstore_py=$_c
        break
    fi
done
unset _c
export PYTHONUTF8=1 PYTHONIOENCODING=utf-8

py() {
    if [ -z "$_pstore_py" ]; then
        echo "FAIL no Python >= 3.9 found as python3, python or py -3" >&2
        return 127
    fi
    # shellcheck disable=SC2086
    $_pstore_py "$@"
}
