cd /workspace || exit 1
awk '$2 == "ERROR" { n++ } END { print n + 0 }' logs/a.log logs/b.log logs/c.log > answer.txt
