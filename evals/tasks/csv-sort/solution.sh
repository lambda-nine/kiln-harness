cd /workspace || exit 1
{ head -n 1 people.csv; tail -n +2 people.csv | LC_ALL=C sort -t , -k 2,2n -k 1,1; } > sorted.csv
