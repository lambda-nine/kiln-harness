# Rename the identifier get_cfg (not part of a longer name) to config_get in
# every .sh file and in docs/api.md.
cd /workspace || exit 1
for f in $(find . -name '*.sh' -type f) docs/api.md; do
  awk '
  {
    rest = $0
    done = ""
    while ((i = index(rest, "get_cfg")) > 0) {
      before = i > 1 ? substr(rest, i - 1, 1) : substr(done, length(done), 1)
      after = substr(rest, i + 7, 1)
      if (before !~ /[A-Za-z0-9_]/ && after !~ /[A-Za-z0-9_]/)
        done = done substr(rest, 1, i - 1) "config_get"
      else
        done = done substr(rest, 1, i + 6)
      rest = substr(rest, i + 7)
    }
    print done rest
  }' "$f" >"$f.renamed" || exit 1
  mv "$f.renamed" "$f" || exit 1
done
