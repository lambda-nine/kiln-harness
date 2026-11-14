# Using dus

    dus.sh summary scan.txt     files, total and average size
    dus.sh top 5 scan.txt       the five largest files
    dus.sh hist scan.txt        a histogram by power-of-two bucket
    dus.sh json scan.txt        one JSON object per file

Sizes are shown as `fmt_size` shows them: `0B`, `1023B`, `1.0K`, `1.5M`.
`sizeof.sh -s` uses `fmt_size_si` instead: `1.5kB`, `2.0MB`.
