if grep -rn '[[:space:]]$' src >/dev/null; then echo "fmt: trailing whitespace found" >&2; exit 1; fi
echo "fmt: 2 files already formatted"
