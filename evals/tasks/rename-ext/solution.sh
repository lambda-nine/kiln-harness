cd /workspace/docs || exit 1
for file in *.txt; do
  mv -- "$file" "${file%.txt}.md"
done
